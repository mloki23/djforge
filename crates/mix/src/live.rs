//! Live slots: the Session view's clip slots, launched over a playing set.
//!
//! What plays is held by the engine ([`ColumnPlay`] per column, a fixed array), changed by
//! [`LiveCmd`]s and never by the session. Each block, a column's playing slot is added to its
//! track's input, before the strip, so the fader, inserts and stem faders apply. While a slot
//! plays, the column track's own (arrangement) clips are muted, as in Ableton; they fade back in
//! when the column stops.
//!
//! - **Launches and stops** take effect exactly at their sample: the block is split there. The
//!   slot that was playing fades out over [`XFADE_S`] from that sample (its tail), so it does not
//!   click; the new one starts hard on the launch sample, its downbeat whole.
//! - **Loops** follow session beats: at timeline sample `t` a loop plays track beat
//!   `start_beat + (beat(t) − beat at launch) mod length_beats` through its warp map, so tempo
//!   changes hold. Slots hold whole 4/4 bars, so a loop launched on a bar line restarts on bar
//!   lines. At a restart the pass that ends fades out over [`XFADE_S`] while the next fades in
//!   (equal power).
//! - **One-shots** (and slots without a warp map) play their source once, sample for sample:
//!   sources are at the session rate (they are resampled on import).
//!
//! **Players.** Each audio of a slot (its source, or each stem) has two warp players, A and B,
//! prepared off the audio thread and handed over in a [`crate::WarpRoom`] with the column's
//! [`LiveStore`] (a fixed table: nothing on the audio thread allocates). One plays; the other is
//! warmed up for the next restart or launch in the blocks before it (a stretcher just restarted
//! would put the first transient out at a fraction of its level), at most about one block of
//! extra stretching per player per block, and takes over at the restart. A warm-up that cannot
//! finish in time (a launch with too little lead) finishes at the restart, for that player only.
//! On the audio thread a slot without its players is silent.

use crate::warp::{self, PlayerStem, SOURCE_PLAYER, WarpPlayer};
use soundcraft_dsp::db_to_gain;
use soundcraft_model::{LIVE_SLOTS, MAX_LIVE_COLUMNS, STEMS, Session, Slot, SlotMode, SourceId, Track, TrackId};
use soundcraft_time::{Samples, TICKS_PER_QUARTER};

/// Length of the crossfades at loop restarts, launches and stops, in seconds.
pub const XFADE_S: f64 = 0.002;

/// Longest crossfade in samples (2 ms up to 256 kHz).
const CARRY: usize = 512;

/// Frames rendered at once (the gain scratch's length).
const CHUNK: usize = 512;

/// Players per slot audio: the one playing and the one warming up for the next restart.
pub(crate) const SIDES: usize = 2;

/// Player keys per slot: one per stem and one for the unsplit source.
const KEYS: usize = STEMS.len() + 1;

/// Players a column's storage holds.
const SLOT_PLAYERS: usize = LIVE_SLOTS * KEYS * SIDES;

/// How far ahead (samples) a column looks for its next restart or launch to warm up for: twice
/// the longest warm-up, so one block of warm-up per block finishes in time.
const LOOKAHEAD: Samples = 2 * 4096;

/// A launch or stop for the audio thread, at a timeline sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveCmd {
    /// Start row `slot` of `column` at `at`, stopping what the column plays.
    Launch { column: usize, slot: usize, at: Samples },
    /// Stop `column` at `at`.
    Stop { column: usize, at: Samples },
    /// Stop every column at `at`.
    StopAll { at: Samples },
}

/// A slot playing in a column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LivePlaying {
    /// Its row.
    pub slot: usize,
    /// The sample it started at.
    pub start: Samples,
    /// Session beat at the start (snapped to the tick it was launched on).
    pub(crate) beat0: f64,
    /// Where its current fade-in starts: the latest loop restart (a launch starts hard, on its
    /// sample).
    pub(crate) fade_at: Option<Samples>,
    /// Where its players last restarted: the launch or the latest loop restart.
    pub(crate) head_at: Samples,
}

/// A launch (`Some(row)`) or stop (`None`) waiting for its sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveQueued {
    pub slot: Option<usize>,
    pub at: Samples,
}

/// What a column plays and has queued.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColumnPlay {
    pub playing: Option<LivePlaying>,
    pub queued: Option<LiveQueued>,
    /// Which of its two players each row plays through (bit per row).
    pub(crate) sides: u8,
    /// The column track's arrangement clips are heard (fading in) from `arr_since`, or muted
    /// (fading out) from it.
    pub(crate) arr_on: bool,
    pub(crate) arr_since: Samples,
}

impl Default for ColumnPlay {
    fn default() -> Self {
        ColumnPlay { playing: None, queued: None, sides: 0, arr_on: true, arr_since: Samples::MIN }
    }
}

impl ColumnPlay {
    /// Which of its two players row `row` plays through.
    pub(crate) fn side_of(&self, row: usize) -> usize {
        self.side(row)
    }

    fn side(&self, row: usize) -> usize {
        usize::from((self.sides >> (row % 8)) & 1)
    }

    fn flip(&mut self, row: usize) {
        self.sides ^= 1 << (row % 8);
    }

    /// What plays from `at`; the arrangement fades out when a slot starts and back in when the
    /// column stops.
    pub(crate) fn set_playing(&mut self, next: Option<LivePlaying>, at: Samples) {
        match (self.playing.is_some(), next.is_some()) {
            (false, true) => (self.arr_on, self.arr_since) = (false, at),
            (true, false) => (self.arr_on, self.arr_since) = (true, at),
            _ => {}
        }
        self.playing = next;
    }

    /// Moves what plays and its fades by `by` samples (the transport jumped).
    pub(crate) fn shift(&mut self, by: Samples) {
        if let Some(p) = &mut self.playing {
            // A loop's phase counts session beats from its launch beat, which stays.
            p.start = p.start.saturating_add(by);
            p.fade_at = p.fade_at.map(|f| f.saturating_add(by));
            p.head_at = p.head_at.saturating_add(by);
        }
        if self.arr_since != Samples::MIN {
            self.arr_since = self.arr_since.saturating_add(by);
        }
    }
}

/// Index of `stem`'s players in a row.
fn key(stem: PlayerStem) -> Option<usize> {
    if stem == SOURCE_PLAYER {
        return Some(STEMS.len());
    }
    let k = usize::from(stem);
    (k < STEMS.len()).then_some(k)
}

fn index(row: usize, stem: PlayerStem, side: usize) -> Option<usize> {
    (row < LIVE_SLOTS && side < SIDES).then_some(())?;
    Some((row * KEYS + key(stem)?) * SIDES + side)
}

/// The row, stem and side of player `i`.
fn unindex(i: usize) -> (usize, PlayerStem, usize) {
    let (k, side) = (i / SIDES, i % SIDES);
    let (row, s) = (k / KEYS, k % KEYS);
    (row, if s == STEMS.len() { SOURCE_PLAYER } else { s as PlayerStem }, side)
}

/// A column track's storage for its slots: two players per slot audio, and the tail of a slot
/// fading out. Built off the audio thread at its full size, so playing never grows it.
pub struct LiveStore {
    players: Vec<Option<WarpPlayer>>,
    /// Audio still to be added from `carry_at` on: tails reach past the block they start in.
    carry: Vec<Vec<f32>>,
    carry_at: Samples,
    carry_len: usize,
    /// A stem's gain for the frames being rendered.
    gain: Vec<f32>,
    /// Frames the stretchers produced (heard or warming up) since last read.
    pub(crate) stretched: usize,
}

impl LiveStore {
    /// Storage for a column track of `channels` channels (allocates).
    pub fn new(channels: usize) -> LiveStore {
        LiveStore {
            players: std::iter::repeat_with(|| None).take(SLOT_PLAYERS).collect(),
            carry: vec![vec![0.0; CARRY]; channels.clamp(1, crate::MAX_CHANNELS)],
            carry_at: 0,
            carry_len: 0,
            gain: vec![0.0; CHUNK],
            stretched: 0,
        }
    }

    /// Room for players (fixed).
    pub(crate) fn player_room(&self) -> usize {
        self.players.len()
    }

    pub(crate) fn player_count(&self) -> usize {
        self.players.iter().filter(|p| p.is_some()).count()
    }

    /// The players held, with their row, stem and side.
    pub(crate) fn players(&self) -> impl Iterator<Item = (usize, PlayerStem, usize, &WarpPlayer)> {
        self.players.iter().enumerate().filter_map(|(i, p)| {
            let (row, stem, side) = unindex(i);
            p.as_ref().map(|p| (row, stem, side, p))
        })
    }

    /// Takes every player out.
    pub(crate) fn drain_players(&mut self) -> impl Iterator<Item = WarpPlayer> + '_ {
        self.players.iter_mut().filter_map(Option::take)
    }

    /// Moves the players of the rows and stems `keep` rejects into `out`.
    pub(crate) fn retire_where(&mut self, keep: impl Fn(usize, PlayerStem) -> bool, out: &mut Vec<WarpPlayer>) {
        for (i, p) in self.players.iter_mut().enumerate() {
            let (row, stem, _) = unindex(i);
            if p.is_some() && !keep(row, stem) {
                out.extend(p.take());
            }
        }
    }

    /// Puts `player` in its place, returning what is to be retired: the player it replaces, or
    /// `player` itself when the place doesn't exist.
    pub(crate) fn place(&mut self, row: usize, stem: PlayerStem, side: usize, player: WarpPlayer) -> Option<WarpPlayer> {
        match index(row, stem, side).and_then(|i| self.players.get_mut(i)) {
            Some(p) => p.replace(player),
            None => Some(player),
        }
    }

    /// Forgets the tail waiting to be added (after a seek or a stop).
    pub(crate) fn clear_carry(&mut self) {
        if self.carry_len > 0 {
            self.carry.iter_mut().for_each(|c| c.fill(0.0));
        }
        self.carry_len = 0;
    }

    /// Moves the waiting tail by `by` samples (the transport wrapped).
    pub(crate) fn shift_carry(&mut self, by: Samples) {
        self.carry_at = self.carry_at.saturating_add(by);
    }

    /// Adds the waiting tail's samples before `upto` into `buf` (which starts at `pos`), and
    /// drops them.
    fn mix_carry(&mut self, buf: &mut [Vec<f32>], pos: Samples, upto: Samples) {
        if self.carry_len == 0 {
            self.carry_at = upto;
            return;
        }
        if upto <= self.carry_at {
            return;
        }
        let n = usize::try_from(upto - self.carry_at).unwrap_or(usize::MAX).min(self.carry_len);
        for (dst, src) in buf.iter_mut().zip(self.carry.iter()) {
            for (i, v) in src.iter().take(n).enumerate() {
                let Ok(j) = usize::try_from(self.carry_at.saturating_add(i as i64).saturating_sub(pos)) else { continue };
                if let Some(d) = dst.get_mut(j) {
                    *d += *v;
                }
            }
        }
        let len = self.carry_len;
        for c in self.carry.iter_mut() {
            c.copy_within(n..len, 0);
            c.iter_mut().take(len).skip(len - n).for_each(|x| *x = 0.0);
        }
        self.carry_len = len - n;
        self.carry_at = if self.carry_len == 0 { upto } else { self.carry_at.saturating_add(n as i64) };
    }

    /// Readies the carry for a tail from `at`: what waits must start there (else it is stale).
    fn begin_tail(&mut self, at: Samples) {
        if self.carry_len > 0 && self.carry_at != at {
            self.clear_carry();
        }
        if self.carry_len == 0 {
            self.carry_at = at;
        }
    }
}

/// True when `slot` loops through warp players: a loop with a usable warp map. Other slots play
/// their source directly, sample for sample.
pub fn slot_loops(slot: &Slot) -> bool {
    slot.mode == SlotMode::Loop && slot.warp.as_ref().is_some_and(|w| w.is_usable())
}

/// The audio a slot plays, one entry per player key it needs (as [`crate::played_sources`] for
/// clips): `(SOURCE_PLAYER, source)`, or `(i, stem)` for each of a split slot's four stems. Each
/// key has two players (see the module docs).
pub fn slot_sources(slot: &Slot) -> impl Iterator<Item = (PlayerStem, SourceId)> + '_ {
    let source = slot.stems.is_none().then_some((SOURCE_PLAYER, slot.source));
    source.into_iter().chain(slot.stems.iter().flat_map(|set| set.sources.iter().enumerate().map(|(i, s)| (i as PlayerStem, *s))))
}

/// The Live column that plays into `track` (see [`soundcraft_model::LiveState::column_track`]: a
/// lost column plays into none).
pub fn column_of(s: &Session, track: TrackId) -> Option<usize> {
    s.live.columns.iter().position(|c| c.track == track).filter(|c| *c < MAX_LIVE_COLUMNS && s.live.column_track(*c, &s.tracks) == Some(track))
}

/// True when the engine plays the warp players `stem` of row `row` of the column on `track`: the
/// column that plays into the track (not a lost one), whose slot there loops
/// ([`slot_loops`]) and plays `stem`.
pub fn slot_player_plays(s: &Session, track: TrackId, row: usize, stem: PlayerStem) -> bool {
    column_of(s, track).and_then(|c| s.live.slot(c, row)).is_some_and(|sl| slot_loops(sl) && slot_sources(sl).any(|(k, _)| k == stem))
}

/// Session beat (quarter notes) at timeline sample `t`.
fn beat(s: &Session, t: Samples) -> f64 {
    s.tempo.samples_to_ticks_f(t, s.sample_rate) / TICKS_PER_QUARTER as f64
}

/// Session beat at the middle of sample `t`: which loop pass a sample belongs to is read here,
/// so a restart lands on the sample nearest its beat, where the tempo map puts a bar line.
fn mid_beat(s: &Session, t: Samples) -> f64 {
    0.5 * (beat(s, t) + beat(s, t.saturating_add(1)))
}

/// The session beat a slot launched at `at` counts from: the tick it falls on, when `at` is
/// that tick's nearest sample.
fn start_beat(s: &Session, at: Samples) -> f64 {
    let b = beat(s, at);
    let half = 0.5 * (beat(s, at.saturating_add(1)) - b) * TICKS_PER_QUARTER as f64;
    let ticks = b * TICKS_PER_QUARTER as f64;
    if (ticks - ticks.round()).abs() <= half { ticks.round() / TICKS_PER_QUARTER as f64 } else { b }
}

pub(crate) fn xfade_len(s: &Session) -> usize {
    ((XFADE_S * s.sample_rate.as_f64()).round() as usize).clamp(1, CARRY)
}

/// Equal-power fade-in gain of frame `i` of an `n`-frame crossfade (the fade-out is its
/// complement). Positive from the first frame.
fn fade_in(i: usize, n: usize) -> f32 {
    if i >= n {
        return 1.0;
    }
    (std::f64::consts::FRAC_PI_2 * (i + 1) as f64 / (n + 1) as f64).sin() as f32
}

fn fade_out(i: usize, n: usize) -> f32 {
    if i >= n {
        return 0.0;
    }
    (std::f64::consts::FRAC_PI_2 * (i + 1) as f64 / (n + 1) as f64).cos() as f32
}

/// The first sample after `from` (up to `to`) that plays a later pass than `from`, by
/// `pass` (non-decreasing).
fn next_pass_start(from: Samples, to: Samples, pass: impl Fn(Samples) -> i64) -> Option<Samples> {
    let k = pass(from);
    if to <= from || pass(to) <= k {
        return None;
    }
    let (mut lo, mut hi) = (from, to);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if pass(mid) > k { hi = mid } else { lo = mid }
    }
    Some(hi)
}

impl LivePlaying {
    pub(crate) fn new(s: &Session, slot: usize, at: Samples) -> LivePlaying {
        LivePlaying { slot, start: at, beat0: start_beat(s, at), fade_at: None, head_at: at }
    }

    /// Which pass of a loop `length` beats long sample `t` plays.
    fn pass(&self, s: &Session, length: f64, t: Samples) -> i64 {
        ((mid_beat(s, t) - self.beat0) / length).floor() as i64
    }

    /// The fade-in gain at `t`.
    fn fade(&self, t: Samples, xf: usize) -> f32 {
        self.fade_at.and_then(|f| usize::try_from(t.saturating_sub(f)).ok()).map_or(1.0, |i| fade_in(i, xf))
    }
}

/// A restart or launch a player is warmed up for: row, side, launch beat, pass and sample.
#[derive(Clone, Copy)]
struct Target {
    row: usize,
    side: usize,
    beat0: f64,
    pass: i64,
    at: Samples,
}

/// What a render needs about the column.
pub(crate) struct Column<'a> {
    pub s: &'a Session,
    pub track: &'a Track,
    pub column: usize,
}

impl Column<'_> {
    fn slot(&self, row: usize) -> Option<&Slot> {
        self.s.live.slot(self.column, row)
    }

    /// Applies the queued launch or stop when it is due by `end`, without playing anything:
    /// for a column whose track is not rendered.
    pub(crate) fn advance(&self, play: &mut ColumnPlay, end: Samples) {
        if let Some(q) = play.queued.filter(|q| q.at < end) {
            play.queued = None;
            let next = q.slot.filter(|r| self.slot(*r).is_some()).map(|r| LivePlaying::new(self.s, r, q.at));
            play.set_playing(next, q.at);
        }
    }

    /// The source position heard at `t` in pass `pass` of loop `slot` launched on `beat0`.
    fn heard<'b>(&'b self, slot: &'b Slot, beat0: f64, pass: i64) -> Option<impl Fn(Samples) -> f64 + 'b> {
        let warp = slot.warp.as_ref()?;
        let first = slot.start_beat - beat0 - pass as f64 * slot.length_beats.max(1.0);
        let s = self.s;
        Some(move |x: Samples| warp.source_at(first + beat(s, x)))
    }

    /// Where a one-shot started at `p` ends (its longest audio runs out).
    fn one_shot_end(&self, p: &LivePlaying) -> Option<Samples> {
        let slot = self.slot(p.slot)?;
        if slot_loops(slot) {
            return None;
        }
        let first = slot.warp.as_ref().map_or(0, |w| w.source_at(slot.start_beat).round() as i64);
        let longest = slot_sources(slot).filter_map(|(_, id)| self.s.pool.get(id)).map(|a| a.frames()).max().unwrap_or(0);
        Some(p.start.saturating_add((longest as i64).saturating_sub(first).max(0)))
    }

    /// True when every audio `slot` plays is in the pool.
    fn playable(&self, slot: &Slot) -> bool {
        slot_sources(slot).all(|(_, id)| self.s.pool.contains(id))
    }

    /// Adds the column's slots for `[pos, pos + frames)` into `buf` (which holds the track's
    /// arrangement clips, muted while a slot plays).
    pub(crate) fn render(&self, play: &mut ColumnPlay, store: &mut LiveStore, buf: &mut [Vec<f32>], pos: Samples, frames: usize) {
        if store.carry_len > 0 && store.carry_at != pos {
            store.clear_carry();
        }
        let end = pos.saturating_add(frames as i64);
        let xf = xfade_len(self.s);
        self.prewarm(play, store, pos, frames);
        let mut a = pos;
        loop {
            let due = play.queued.filter(|q| q.at < end);
            let due_at = due.map(|q| q.at.max(a));
            let shot_end = play.playing.and_then(|p| self.one_shot_end(&p)).filter(|e| *e < end).map(|e| e.max(a));
            let b = [due_at, shot_end].into_iter().flatten().fold(end, Samples::min);
            mute_arrangement(play, buf, pos, a, b, xf);
            if b > a {
                // A restart exactly at the block's end is left to the next block when a launch or
                // stop waits for that sample, which then takes over from the pass that ends.
                let seam_at_end = b == end && !play.queued.is_some_and(|q| q.at == end);
                self.render_playing(play, store, buf, pos, a, b, seam_at_end);
            }
            if shot_end == Some(b) {
                play.set_playing(None, b);
            }
            if let Some(q) = due.filter(|_| due_at == Some(b)) {
                store.mix_carry(buf, pos, b);
                // A restart handled right at `b` already put the ending pass's tail there.
                if let Some(p) = play.playing.filter(|p| p.fade_at != Some(b)) {
                    self.tail(p, play.side(p.slot), store, b);
                }
                play.queued = None;
                let next = q.slot.filter(|r| self.slot(*r).is_some_and(|sl| self.playable(sl))).map(|r| LivePlaying::new(self.s, r, b));
                if let Some(n) = next {
                    play.flip(n.slot);
                }
                play.set_playing(next, b);
            }
            if b >= end {
                break;
            }
            a = b;
        }
        store.mix_carry(buf, pos, end);
    }

    /// Warms the idle players up for the column's next loop restart and its queued launch,
    /// when they fall within [`LOOKAHEAD`]: about one block of stretching per player, more
    /// (up to twice) once the restart is nearer than the warm-up is long.
    fn prewarm(&self, play: &ColumnPlay, store: &mut LiveStore, pos: Samples, frames: usize) {
        let horizon = pos.saturating_add(frames as i64).saturating_add(LOOKAHEAD);
        let mut targets: [Option<Target>; 2] = [None, None];
        if let Some(p) = play.playing
            && let Some(slot) = self.slot(p.slot).filter(|sl| slot_loops(sl))
        {
            let length = slot.length_beats.max(1.0);
            let k = p.pass(self.s, length, pos);
            if let Some(at) = next_pass_start(pos, horizon, |t| p.pass(self.s, length, t))
                && !play.queued.is_some_and(|q| q.at <= at)
            {
                targets[0] = Some(Target { row: p.slot, side: 1 - play.side(p.slot), beat0: p.beat0, pass: k + 1, at });
            }
        }
        if let Some(q) = play.queued
            && let Some(row) = q.slot
            && (pos..=horizon).contains(&q.at)
            && self.slot(row).is_some_and(slot_loops)
            && !targets[0].is_some_and(|t| t.row == row)
        {
            targets[1] = Some(Target { row, side: 1 - play.side(row), beat0: start_beat(self.s, q.at), pass: 0, at: q.at });
        }
        for t in targets.into_iter().flatten() {
            let Some(slot) = self.slot(t.row) else { continue };
            let Some(heard) = self.heard(slot, t.beat0, t.pass) else { continue };
            for (stem, source) in slot_sources(slot) {
                if stem != SOURCE_PLAYER && !slot.stem_mask.get(usize::from(stem)).copied().unwrap_or(false) {
                    continue;
                }
                let Some(audio) = self.s.pool.get(source) else { continue };
                let Some(Some(player)) = index(t.row, stem, t.side).and_then(|i| store.players.get_mut(i)) else { continue };
                let late = t.at.saturating_sub(pos) < warp::warm_span(player) as i64;
                let budget = if late { 2 * frames } else { frames };
                store.stretched += warp::warm_toward(player, &audio.buffer.channels, source, &heard, t.at, budget);
            }
        }
    }

    /// Renders what `play` plays over `[a, b)`; a loop restart at `b` itself is handled when
    /// `seam_at_end`. A slot that is gone or whose audio is missing stops the column.
    #[allow(clippy::too_many_arguments)]
    fn render_playing(
        &self,
        play: &mut ColumnPlay,
        store: &mut LiveStore,
        buf: &mut [Vec<f32>],
        pos: Samples,
        a: Samples,
        b: Samples,
        seam_at_end: bool,
    ) {
        let Some(mut p) = play.playing else { return };
        let Some(slot) = self.slot(p.slot).filter(|sl| self.playable(sl)) else {
            play.set_playing(None, a);
            return;
        };
        let xf = xfade_len(self.s);
        if !slot_loops(slot) {
            let mut t = a;
            while t < b {
                let n = usize::try_from(b - t).unwrap_or(0).min(CHUNK);
                let off = usize::try_from(t - pos).unwrap_or(0);
                self.one_shot(slot, &p, store, t, n, Out::Buf(buf, off), |at| p.fade(at, xf));
                t = t.saturating_add(n as i64);
            }
            return;
        }
        let length = slot.length_beats.max(1.0);
        let mut t = a;
        while t < b {
            let c = t.saturating_add(CHUNK as i64).min(b);
            let k = p.pass(self.s, length, t);
            // The first sample of the next pass, if it falls in (t, c) (or at c, when the block
            // ends there and nothing takes over at that sample).
            let last = if seam_at_end && c == b { c } else { c - 1 };
            let seam = next_pass_start(t, last, |x| p.pass(self.s, length, x));
            let e = seam.unwrap_or(c).min(c);
            if e > t {
                let off = usize::try_from(t - pos).unwrap_or(0);
                let n = usize::try_from(e - t).unwrap_or(0);
                let side = play.side(p.slot);
                self.loop_pass(slot, &p, k, side, t == p.head_at, store, t, n, Out::Buf(buf, off), |at| p.fade(at, xf));
            }
            if let Some(seam) = seam {
                // Restart: the pass that ends fades out into the carry; the warmed-up player of
                // the other side plays the next, fading in.
                store.mix_carry(buf, pos, seam);
                store.begin_tail(seam);
                let side = play.side(p.slot);
                self.loop_pass(slot, &p, k, side, false, store, seam, xf, Out::Carry, |at| {
                    usize::try_from(at - seam).map_or(0.0, |i| fade_out(i, xf))
                });
                store.carry_len = store.carry_len.max(xf);
                play.flip(p.slot);
                p.fade_at = Some(seam);
                p.head_at = seam;
            }
            t = e;
        }
        play.playing = Some(p);
    }

    /// Fades `p` (playing through `side`) out from `at` into the carry (it stops there).
    pub(crate) fn tail(&self, p: LivePlaying, side: usize, store: &mut LiveStore, at: Samples) {
        let Some(slot) = self.slot(p.slot).filter(|sl| self.playable(sl)) else { return };
        store.begin_tail(at);
        let xf = xfade_len(self.s);
        let gain = |t: Samples| usize::try_from(t - at).map_or(0.0, |i| fade_out(i, xf)) * p.fade(t, xf);
        if slot_loops(slot) {
            let length = slot.length_beats.max(1.0);
            // The pass heard just before `at` goes on (at a restart sample too).
            let k = p.pass(self.s, length, if at > p.start { at - 1 } else { at });
            self.loop_pass(slot, &p, k, side, false, store, at, xf, Out::Carry, gain);
        } else {
            self.one_shot(slot, &p, store, at, xf, Out::Carry, gain);
        }
        store.carry_len = store.carry_len.max(xf);
    }

    /// The gain of stem `i` (or the unsplit source) of `slot` for `n` frames from `t` into
    /// `out`: its mask, the column track's stem fader and the set's scale. False when silent.
    fn stem_gain(&self, slot: &Slot, stem: PlayerStem, t: Samples, n: usize, out: &mut [f32]) -> bool {
        if stem == SOURCE_PLAYER {
            out.iter_mut().take(n).for_each(|g| *g = 1.0);
            return true;
        }
        let scale = slot.stems.as_ref().map_or(1.0, |set| set.scale());
        crate::stem_gains(Some(self.track), &slot.stem_mask, scale, usize::from(stem), t, n, out)
    }

    /// Adds `n` frames from `t` of pass `k` of loop `slot` through its `side` players into
    /// `out`, times `fade(t)`. At a `head` (a launch or restart) a player not warmed up in time
    /// finishes its warm-up first.
    #[allow(clippy::too_many_arguments)]
    fn loop_pass(
        &self,
        slot: &Slot,
        p: &LivePlaying,
        k: i64,
        side: usize,
        head: bool,
        store: &mut LiveStore,
        t: Samples,
        n: usize,
        mut out: Out<'_>,
        fade: impl Fn(Samples) -> f32,
    ) {
        let Some(heard) = self.heard(slot, p.beat0, k) else { return };
        let g = db_to_gain(slot.gain_db);
        let sr = self.s.sample_rate.as_f64() as f32;
        let LiveStore { players, carry, gain, stretched, .. } = store;
        for (stem, source) in slot_sources(slot) {
            let Some(audio) = self.s.pool.get(source) else { continue };
            let src = &audio.buffer.channels;
            if src.is_empty() || !self.stem_gain(slot, stem, t, n, gain) {
                continue;
            }
            let Some(entry) = index(p.slot, stem, side).and_then(|i| players.get_mut(i)) else { continue };
            if entry.is_none() && !crate::on_audio_thread() {
                // Offline (tests, renders without a host): built here. The audio thread never
                // builds one; until it arrives the slot is silent.
                *entry = WarpPlayer::build(src.len(), sr, false);
            }
            let Some(player) = entry.as_mut() else { continue };
            if head {
                *stretched += warp::warm_toward(player, src, source, &heard, t, usize::MAX);
            }
            let gain = &*gain;
            let gain_at = |i: usize| g * fade(t.saturating_add(i as i64)) * gain.get(i).copied().unwrap_or(0.0);
            let (dst, off): (&mut [Vec<f32>], usize) = match &mut out {
                Out::Buf(b, off) => (&mut **b, *off),
                Out::Carry => (carry.as_mut_slice(), 0),
            };
            *stretched += warp::stretch_into(player, src, source, &heard, t, n, dst, off, gain_at);
        }
    }

    /// Adds `n` frames from `t` of one-shot `slot` into `out`, times `fade(t)`.
    #[allow(clippy::too_many_arguments)]
    fn one_shot(&self, slot: &Slot, p: &LivePlaying, store: &mut LiveStore, t: Samples, n: usize, mut out: Out<'_>, fade: impl Fn(Samples) -> f32) {
        let g = db_to_gain(slot.gain_db);
        let first = slot.warp.as_ref().map_or(0, |w| w.source_at(slot.start_beat).round() as i64);
        let at0 = first.saturating_add(t.saturating_sub(p.start));
        let LiveStore { carry, gain, .. } = store;
        for (stem, source) in slot_sources(slot) {
            let Some(audio) = self.s.pool.get(source) else { continue };
            let src = &audio.buffer.channels;
            if src.is_empty() || !self.stem_gain(slot, stem, t, n, gain) {
                continue;
            }
            let (dst, off): (&mut [Vec<f32>], usize) = match &mut out {
                Out::Buf(b, off) => (&mut **b, *off),
                Out::Carry => (carry.as_mut_slice(), 0),
            };
            for (ch, d) in dst.iter_mut().enumerate() {
                let Some(sc) = src.get(ch % src.len()) else { continue };
                for i in 0..n.min(CHUNK) {
                    let Ok(idx) = usize::try_from(at0.saturating_add(i as i64)) else { continue };
                    let (Some(v), Some(o)) = (sc.get(idx), d.get_mut(off + i)) else { continue };
                    *o += v * g * fade(t.saturating_add(i as i64)) * gain.get(i).copied().unwrap_or(0.0);
                }
            }
        }
    }
}

/// Mutes (fades) the column track's arrangement audio in `buf` over `[a, b)` while a slot plays.
fn mute_arrangement(play: &ColumnPlay, buf: &mut [Vec<f32>], pos: Samples, a: Samples, b: Samples, xf: usize) {
    let since = play.arr_since;
    if play.arr_on && (since == Samples::MIN || a.saturating_sub(since) >= xf as i64) {
        return;
    }
    let gain = |t: Samples| {
        let i = usize::try_from(t.saturating_sub(since)).unwrap_or(0);
        if play.arr_on { fade_in(i, xf) } else { fade_out(i, xf) }
    };
    for c in buf.iter_mut() {
        for t in a..b {
            if let Some(x) = usize::try_from(t - pos).ok().and_then(|j| c.get_mut(j)) {
                *x *= gain(t);
            }
        }
    }
}

/// Where a render goes: the block (at an offset) or the column's carry (from its start).
enum Out<'a> {
    Buf(&'a mut [Vec<f32>], usize),
    Carry,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{MixEngine, WarpRoom, as_audio_thread};
    use soundcraft_audio_io::AudioBuffer;
    use soundcraft_model::{ChannelFormat, LiveColumn, SourceAudio, StemSet, TrackKind, WarpMap};
    use std::sync::Arc;

    /// Launched on a bar line, every pass of a loop starts on the sample the tempo map puts
    /// that bar line at, through tempo steps of odd sizes.
    #[test]
    fn restarts_fall_on_the_tempo_maps_bar_lines() {
        let mut s = Session::default();
        s.tempo.set_tempo(0, 120.0).unwrap();
        for b in 4..200 {
            s.tempo.set_tempo(b * TICKS_PER_QUARTER, 117.3 + 0.37 * b as f64).unwrap();
        }
        let bar = |n: i64| s.tempo.tick_to_samples(s.tempo.bar_start_tick(n), s.sample_rate);
        for (launch, len) in [(2, 4.0), (3, 8.0), (5, 16.0)] {
            let p = LivePlaying::new(&s, 0, bar(launch));
            let bars = (len / 4.0) as i64;
            for k in 1..8 {
                let line = bar(launch + k * bars);
                assert_eq!((p.pass(&s, len, line - 1), p.pass(&s, len, line)), (k - 1, k), "launch bar {launch}, pass {k}");
            }
        }
    }

    fn session() -> (Session, TrackId) {
        let mut s = Session::default();
        for k in 900..905 {
            s.pool.insert(SourceId(k), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 96_000]] })));
        }
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let mut col = LiveColumn::new(t);
        let warp = WarpMap::new(vec![0.0, 24_000.0, 48_000.0, 72_000.0], 0.0);
        let mut slot = Slot::one_shot("s", [0; 3], SourceId(900));
        slot.warp = warp;
        slot.mode = SlotMode::Loop;
        slot.length_beats = 1.0;
        col.slots[0] = Some(slot.clone());
        slot.stems = Some(StemSet { scale_log2: 0, key: "k".into(), sources: [SourceId(901), SourceId(902), SourceId(903), SourceId(904)] });
        col.slots[1] = Some(slot);
        s.live.columns.push(col);
        (s, t)
    }

    /// On the audio thread a slot without its prepared player is silent and no player is built.
    #[test]
    fn the_audio_thread_never_builds_a_slot_player() {
        let (s, _) = session();
        let mut eng = MixEngine::new(48_000.0, 512);
        let mut out = vec![vec![0.0; 512]; 2];
        eng.ensure_synced(&s);
        eng.live(LiveCmd::Launch { column: 0, slot: 1, at: 0 });
        for k in 0..4 {
            as_audio_thread(true, || eng.render(&s, k * 512, 512, &mut out));
        }
        assert_eq!(eng.slot_players().count(), 0);
        assert!(out.iter().flatten().all(|x| *x == 0.0), "silent until its players arrive");
        assert!(eng.live_state()[0].playing.is_some(), "but it is playing");
    }

    /// Storage and players come in a room built off the audio thread; launching, looping
    /// (restarts), switching and stopping on the audio thread then never grow a table or list.
    #[test]
    fn slots_play_from_the_room_without_growing() {
        let (mut s, t) = session();
        let mut eng = MixEngine::new(48_000.0, 512);
        let mut out = vec![vec![0.0; 512]; 2];
        as_audio_thread(true, || {
            eng.ensure_synced(&s);
        });
        assert_eq!(eng.slot_table_capacity(t), 0, "the audio thread makes no storage");
        let back = eng.adopt_room(&s, WarpRoom::for_session(&s, 0));
        assert_eq!(back.live_stores(), 0, "the strip took it");
        let cap = eng.slot_table_capacity(t);
        assert!(cap >= SLOT_PLAYERS);
        let mut batch: Vec<_> = (0..SIDES).map(|side| crate::PreparedWarp::for_slot(t, 0, side, SOURCE_PLAYER, 1, 48_000.0).unwrap()).collect();
        batch.extend((0..4).flat_map(|k| (0..SIDES).map(move |side| crate::PreparedWarp::for_slot(t, 1, side, k, 1, 48_000.0).unwrap())));
        eng.adopt_warps(batch);
        let retired = eng.retired_warps.capacity();
        let mut pos = 0;
        as_audio_thread(true, || {
            eng.live(LiveCmd::Launch { column: 0, slot: 0, at: 700 });
            for _ in 0..200 {
                if pos == 94 * 512 {
                    eng.live(LiveCmd::Launch { column: 0, slot: 1, at: 50_123 });
                }
                if pos == 157 * 512 {
                    eng.live(LiveCmd::StopAll { at: 90_001 });
                }
                eng.render(&s, pos, 512, &mut out);
                pos += 512;
            }
        });
        assert_eq!(eng.slot_players().count(), 10, "two players per slot audio");
        assert!(eng.slot_players().all(|(_, _, _, _, p)| p.is_prepared()));
        assert_eq!(eng.slot_table_capacity(t), cap, "never grew");
        assert_eq!(eng.retired_warps.capacity(), retired);
        assert!(eng.live_state()[0].playing.is_none());
        // A later room brings no new storage for the strip that has one; clearing a slot
        // retires its players.
        let back = eng.adopt_room(&s, WarpRoom::for_session(&s, 4));
        assert_eq!(back.live_stores(), 1, "the strip keeps its own");
        s.live.columns[0].slots[1] = None;
        eng.retire_stale_warps(&s);
        assert_eq!(eng.take_retired_warps().len(), 8);
        assert_eq!(eng.slot_players().count(), 2);
    }

    /// Four columns of split loops (4 stems each): a scene launch over a playing scene, with
    /// the 50 ms lead the Session view gives, and the restarts all four columns share on every
    /// bar never cost a callback more than about three blocks of stretching per player (play,
    /// plus at most twice a block warming up), against a full warm-up per player at once before.
    #[test]
    fn warm_ups_are_spread_over_the_blocks_before_a_restart() {
        const BLOCK: usize = 512;
        let mut s = Session::default();
        s.tempo.set_tempo(0, 120.0).unwrap();
        for k in 900..905 {
            let tone: Vec<f32> = (0..400_000).map(|i| 0.1 * (i as f32 * 0.05 + k as f32).sin()).collect();
            s.pool.insert(SourceId(k), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![tone.clone(), tone] })));
        }
        let mut tracks = Vec::new();
        for _ in 0..4 {
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
            let mut col = LiveColumn::new(t);
            for row in 0..2 {
                let mut slot = Slot::one_shot("s", [0; 3], SourceId(900));
                slot.warp = WarpMap::new((0..16).map(|k| 1_000.0 + f64::from(k) * 23_040.0).collect(), 0.0);
                slot.mode = SlotMode::Loop;
                slot.length_beats = 4.0;
                slot.start_beat = f64::from(row as u8) * 4.0;
                slot.stems = Some(StemSet { scale_log2: 0, key: "k".into(), sources: [SourceId(901), SourceId(902), SourceId(903), SourceId(904)] });
                col.slots[row] = Some(slot);
            }
            s.live.columns.push(col);
            tracks.push(t);
        }
        let mut eng = MixEngine::new(48_000.0, BLOCK);
        eng.ensure_synced(&s);
        let batch: Vec<_> = tracks
            .iter()
            .flat_map(|t| (0..2).flat_map(move |row| (0..4).flat_map(move |k| (0..SIDES).map(move |side| (*t, row, k, side)))))
            .map(|(t, row, k, side)| crate::PreparedWarp::for_slot(t, row, side, k, 2, 48_000.0).unwrap())
            .collect();
        eng.adopt_warps(batch);
        let bar = |n: i64| s.tempo.tick_to_samples(s.tempo.bar_start_tick(n), s.sample_rate);
        let lead = 2_400;
        let mut out = vec![vec![0.0; BLOCK]; 2];
        let (mut worst, mut worst_us, mut total_us, mut blocks) = (0usize, 0u128, 0u128, 0u128);
        let mut pos = 0;
        let mut sent = [false; 2];
        as_audio_thread(true, || {
            while pos < bar(8) {
                for (i, (row, at)) in [(0, bar(2)), (1, bar(5))].into_iter().enumerate() {
                    if !sent[i] && pos + BLOCK as i64 > at - lead {
                        for column in 0..4 {
                            eng.live(LiveCmd::Launch { column, slot: row, at });
                        }
                        sent[i] = true;
                    }
                }
                let t0 = std::time::Instant::now();
                eng.render(&s, pos, BLOCK, &mut out);
                let us = t0.elapsed().as_micros();
                if pos > 0 {
                    // The first block places the players.
                    worst_us = worst_us.max(us);
                    total_us += us;
                    blocks += 1;
                }
                worst = worst.max(eng.take_live_stretched());
                pos += BLOCK as i64;
            }
        });
        let players = 16;
        println!(
            "16 stem players: worst {worst} stretched frames per {BLOCK}-frame callback ({:.2} blocks per player); worst {worst_us} µs, mean {} µs; a full warm-up per player at once would be {}",
            worst as f64 / (players * BLOCK) as f64,
            total_us / blocks.max(1),
            players * (BLOCK + 2_880)
        );
        assert!(worst <= 3 * players * BLOCK, "worst callback stretched {worst} frames");
        assert!(eng.live_state().iter().take(4).all(|c| c.playing.is_some_and(|p| p.slot == 1)));
    }
}
