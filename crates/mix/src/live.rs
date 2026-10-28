//! Live slots: the Session view's clip slots, launched over a playing set.
//!
//! What plays is held by the engine ([`ColumnPlay`] per column, a fixed array), changed by
//! [`LiveCmd`]s and never by the session. Each block, a column's playing slot is added to its
//! track's input, before the strip, so the fader, inserts and stem faders apply.
//!
//! - **Launches and stops** take effect exactly at their sample: the block is split there. The
//!   slot that was playing fades out over [`XFADE_S`] from that sample (its tail), so it does not
//!   click; the new one starts hard on the launch sample, its downbeat whole.
//! - **Loops** follow session beats: at timeline sample `t` a loop plays track beat
//!   `start_beat + (beat(t) − beat at launch) mod length_beats` through its warp map, so tempo
//!   changes hold and, launched on a bar line, every restart lands on one. A restart re-seeks the
//!   player (with pre-roll) and crossfades the old pass's tail into the new pass's head over
//!   [`XFADE_S`] (equal power).
//! - **One-shots** (and slots without a warp map) play their source once, at its own rate.
//!
//! Players are prepared off the audio thread, per slot row and [`PlayerStem`] of the column
//! track ([`LiveStore`], handed over in a [`crate::WarpRoom`]); on the audio thread a slot
//! without its player is silent.

use crate::warp::{self, PlayerStem, SOURCE_PLAYER, WarpPlayer};
use soundcraft_dsp::db_to_gain;
use soundcraft_model::{LIVE_SLOTS, MAX_LIVE_COLUMNS, STEMS, Session, Slot, SlotMode, SourceId, Track, TrackId};
use soundcraft_time::{Samples, TICKS_PER_QUARTER};
use std::collections::HashMap;

/// Length of the crossfades at loop restarts, launches and stops, in seconds.
pub const XFADE_S: f64 = 0.002;

/// Longest crossfade in samples (2 ms up to 256 kHz).
const CARRY: usize = 512;

/// Frames rendered at once (the gain scratch's length).
const CHUNK: usize = 512;

/// Players a column's storage holds at most: every row with a player per stem and its source.
const SLOT_PLAYERS: usize = LIVE_SLOTS * (STEMS.len() + 1);

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
}

/// A launch (`Some(row)`) or stop (`None`) waiting for its sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveQueued {
    pub slot: Option<usize>,
    pub at: Samples,
}

/// What a column plays and has queued.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ColumnPlay {
    pub playing: Option<LivePlaying>,
    pub queued: Option<LiveQueued>,
}

/// Warp players of a column's slots, by row and [`PlayerStem`].
pub(crate) type SlotPlayers = HashMap<(u8, PlayerStem), WarpPlayer>;

/// A column track's storage for its slots: players, and the tail of a slot fading out. Built off
/// the audio thread at its full size, so playing never grows it.
pub struct LiveStore {
    pub(crate) players: SlotPlayers,
    /// Audio still to be added from `carry_at` on: tails reach past the block they start in.
    carry: Vec<Vec<f32>>,
    carry_at: Samples,
    carry_len: usize,
    /// A stem's gain for the frames being rendered.
    gain: Vec<f32>,
}

impl Default for LiveStore {
    fn default() -> Self {
        LiveStore::new()
    }
}

impl LiveStore {
    /// Storage for a column (allocates).
    pub fn new() -> LiveStore {
        LiveStore {
            players: HashMap::with_capacity(SLOT_PLAYERS),
            carry: vec![vec![0.0; CARRY]; crate::MAX_CHANNELS],
            carry_at: 0,
            carry_len: 0,
            gain: vec![0.0; CHUNK],
        }
    }

    /// Forgets the tail waiting to be added (after a seek or a stop).
    pub(crate) fn clear_carry(&mut self) {
        if self.carry_len > 0 {
            self.carry.iter_mut().for_each(|c| c.fill(0.0));
        }
        self.carry_len = 0;
    }

    pub(crate) fn player_capacity(&self) -> usize {
        self.players.capacity()
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
}

/// True when `slot` loops through warp players: a loop with a usable warp map. Other slots play
/// their source directly, at its own rate.
pub fn slot_loops(slot: &Slot) -> bool {
    slot.mode == SlotMode::Loop && slot.warp.as_ref().is_some_and(|w| w.is_usable())
}

/// The audio a slot plays, one entry per player it needs (as [`crate::played_sources`] for
/// clips): `(SOURCE_PLAYER, source)`, or `(i, stem)` for each of a split slot's four stems.
pub fn slot_sources(slot: &Slot) -> impl Iterator<Item = (PlayerStem, SourceId)> + '_ {
    let source = slot.stems.is_none().then_some((SOURCE_PLAYER, slot.source));
    source.into_iter().chain(slot.stems.iter().flat_map(|set| set.sources.iter().enumerate().map(|(i, s)| (i as PlayerStem, *s))))
}

/// The Live column that plays into `track` (see [`soundcraft_model::LiveState::column_track`]: a
/// lost column plays into none).
pub fn column_of(s: &Session, track: TrackId) -> Option<usize> {
    s.live.columns.iter().position(|c| c.track == track).filter(|c| *c < MAX_LIVE_COLUMNS && s.live.column_track(*c, &s.tracks) == Some(track))
}

/// True when the engine plays the warp player `stem` of row `row` of the column on `track`: the
/// column that plays into the track (not a lost one), whose slot there loops ([`slot_loops`]) and plays `stem`.
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

fn xfade_len(s: &Session) -> usize {
    ((XFADE_S * s.sample_rate.as_f64()).round() as usize).clamp(1, CARRY)
}

/// Equal-power fade-in gain of frame `i` of an `n`-frame crossfade (the fade-out is its
/// complement: `fade_in(n - 1 - i)`). Positive from the first frame.
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

impl LivePlaying {
    pub(crate) fn new(s: &Session, slot: usize, at: Samples) -> LivePlaying {
        LivePlaying { slot, start: at, beat0: start_beat(s, at), fade_at: None }
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

/// What a render needs about the column.
pub(crate) struct Column<'a> {
    pub s: &'a Session,
    pub track: &'a Track,
    pub column: usize,
}

impl Column<'_> {
    /// Applies the queued launch or stop at `at` (when it is due by then) without playing
    /// anything: for a column whose track is not rendered.
    pub(crate) fn advance(&self, play: &mut ColumnPlay, end: Samples) {
        if let Some(q) = play.queued.filter(|q| q.at < end) {
            play.queued = None;
            play.playing = q.slot.filter(|r| self.s.live.slot(self.column, *r).is_some()).map(|r| LivePlaying::new(self.s, r, q.at));
        }
    }

    /// Adds the column's slots for `[pos, pos + frames)` into `buf`.
    pub(crate) fn render(&self, play: &mut ColumnPlay, store: &mut LiveStore, buf: &mut [Vec<f32>], pos: Samples, frames: usize) {
        if store.carry_len > 0 && store.carry_at != pos {
            store.clear_carry();
        }
        let end = pos.saturating_add(frames as i64);
        let mut a = pos;
        loop {
            let due = play.queued.filter(|q| q.at < end);
            let b = due.map_or(end, |q| q.at.max(a));
            if let Some(p) = play.playing
                && b > a
            {
                play.playing = self.render_playing(p, store, buf, pos, a, b, b == end);
            }
            let Some(q) = due else { break };
            store.mix_carry(buf, pos, b);
            if let Some(p) = play.playing.take() {
                self.tail(p, store, b);
            }
            play.queued = None;
            play.playing = q.slot.filter(|r| self.s.live.slot(self.column, *r).is_some()).map(|r| LivePlaying::new(self.s, r, b));
            a = b;
        }
        store.mix_carry(buf, pos, end);
    }

    /// Renders `p` over `[a, b)`; a loop restart at `b` itself is handled when `at_end` (the
    /// block ends there, rather than a launch or stop taking over). The slot afterwards: `None`
    /// once a one-shot ends or the slot is gone.
    #[allow(clippy::too_many_arguments)]
    fn render_playing(
        &self,
        mut p: LivePlaying,
        store: &mut LiveStore,
        buf: &mut [Vec<f32>],
        pos: Samples,
        a: Samples,
        b: Samples,
        at_end: bool,
    ) -> Option<LivePlaying> {
        let slot = self.s.live.slot(self.column, p.slot)?;
        let xf = xfade_len(self.s);
        if !slot_loops(slot) {
            let mut t = a;
            while t < b {
                let n = usize::try_from(b - t).unwrap_or(0).min(CHUNK);
                let off = usize::try_from(t - pos).unwrap_or(0);
                let more = self.one_shot(slot, &p, store, t, n, Out::Buf(buf, off), |at| p.fade(at, xf));
                if !more {
                    return None;
                }
                t = t.saturating_add(n as i64);
            }
            return Some(p);
        }
        let length = slot.length_beats.max(1.0);
        let mut t = a;
        while t < b {
            let c = t.saturating_add(CHUNK as i64).min(b);
            let k = p.pass(self.s, length, t);
            // The first sample of the next pass, if it falls in (t, c) (or at c, when the block
            // ends there).
            let last = if at_end && c == b { c } else { c - 1 };
            let seam = (last > t && p.pass(self.s, length, last) > k).then(|| {
                let (mut lo, mut hi) = (t, last);
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    if p.pass(self.s, length, mid) > k { hi = mid } else { lo = mid }
                }
                hi
            });
            let e = seam.unwrap_or(c).min(c);
            if e > t {
                let off = usize::try_from(t - pos).unwrap_or(0);
                let n = usize::try_from(e - t).unwrap_or(0);
                self.loop_pass(slot, &p, k, store, t, n, Out::Buf(buf, off), |at| p.fade(at, xf));
            }
            if let Some(seam) = seam {
                // Restart: the pass that ends fades out into the carry; the next fades in.
                store.mix_carry(buf, pos, seam);
                self.loop_pass(slot, &p, k, store, seam, xf, Out::Carry, |at| usize::try_from(at - seam).map_or(0.0, |i| fade_out(i, xf)));
                store.carry_len = store.carry_len.max(xf);
                p.fade_at = Some(seam);
            }
            t = e;
        }
        Some(p)
    }

    /// Fades `p` out from `at` into the carry (it stops there).
    fn tail(&self, p: LivePlaying, store: &mut LiveStore, at: Samples) {
        let Some(slot) = self.s.live.slot(self.column, p.slot) else { return };
        let xf = xfade_len(self.s);
        let gain = |t: Samples| usize::try_from(t - at).map_or(0.0, |i| fade_out(i, xf)) * p.fade(t, xf);
        if slot_loops(slot) {
            let length = slot.length_beats.max(1.0);
            // The pass heard just before `at` goes on (at a restart sample too).
            let k = p.pass(self.s, length, if at > p.start { at - 1 } else { at });
            self.loop_pass(slot, &p, k, store, at, xf, Out::Carry, gain);
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

    /// Adds `n` frames from `t` of pass `k` of loop `slot` into `out`, times `fade(t)`.
    #[allow(clippy::too_many_arguments)]
    fn loop_pass(
        &self,
        slot: &Slot,
        p: &LivePlaying,
        k: i64,
        store: &mut LiveStore,
        t: Samples,
        n: usize,
        mut out: Out<'_>,
        fade: impl Fn(Samples) -> f32,
    ) {
        let Some(warp) = slot.warp.as_ref() else { return };
        let s = self.s;
        let g = db_to_gain(slot.gain_db);
        let first = slot.start_beat + k as f64 * -slot.length_beats.max(1.0) - p.beat0;
        let heard = |x: Samples| warp.source_at(first + beat(s, x));
        let row = u8::try_from(p.slot).unwrap_or(u8::MAX);
        let sr = s.sample_rate.as_f64() as f32;
        let LiveStore { players, carry, gain, .. } = store;
        for (stem, source) in slot_sources(slot) {
            let Some(audio) = s.pool.get(source) else { continue };
            let src = &audio.buffer.channels;
            if src.is_empty() || !self.stem_gain(slot, stem, t, n, gain) {
                continue;
            }
            let player = match players.entry((row, stem)) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                // Players are prepared off the audio thread; until one arrives the slot is silent.
                std::collections::hash_map::Entry::Vacant(_) if crate::on_audio_thread() => continue,
                std::collections::hash_map::Entry::Vacant(e) => match WarpPlayer::build(src.len(), sr, false) {
                    Some(pl) => e.insert(pl),
                    None => continue,
                },
            };
            let gain = &*gain;
            let gain_at = |i: usize| g * fade(t.saturating_add(i as i64)) * gain.get(i).copied().unwrap_or(0.0);
            let (dst, off): (&mut [Vec<f32>], usize) = match &mut out {
                Out::Buf(b, off) => (&mut **b, *off),
                Out::Carry => (carry.as_mut_slice(), 0),
            };
            warp::stretch_into(player, src, source, heard, t, n, dst, off, gain_at);
        }
    }

    /// Adds `n` frames from `t` of one-shot `slot` into `out`, times `fade(t)`. False when the
    /// source has ended by `t + n`.
    #[allow(clippy::too_many_arguments)]
    fn one_shot(
        &self,
        slot: &Slot,
        p: &LivePlaying,
        store: &mut LiveStore,
        t: Samples,
        n: usize,
        mut out: Out<'_>,
        fade: impl Fn(Samples) -> f32,
    ) -> bool {
        let s = self.s;
        let g = db_to_gain(slot.gain_db);
        let first = slot.warp.as_ref().map_or(0, |w| w.source_at(slot.start_beat).round() as i64);
        let at0 = first.saturating_add(t.saturating_sub(p.start));
        let mut longest = 0usize;
        let LiveStore { carry, gain, .. } = store;
        for (stem, source) in slot_sources(slot) {
            let Some(audio) = s.pool.get(source) else { continue };
            let src = &audio.buffer.channels;
            longest = longest.max(audio.frames());
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
        usize::try_from(at0.saturating_add(n as i64)).is_ok_and(|e| e < longest)
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
        let mut batch = vec![crate::PreparedWarp::for_slot(t, 0, SOURCE_PLAYER, 1, 48_000.0).unwrap()];
        batch.extend((0..4).map(|k| crate::PreparedWarp::for_slot(t, 1, k, 1, 48_000.0).unwrap()));
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
        assert_eq!(eng.slot_players().count(), 5);
        assert!(eng.slot_players().all(|(_, _, _, p)| p.is_prepared()));
        assert_eq!(eng.slot_table_capacity(t), cap, "never grew");
        assert_eq!(eng.retired_warps.capacity(), retired);
        assert!(eng.live_state()[0].playing.is_none());
        // A later room brings no new storage for the strip that has one; clearing a slot
        // retires its players.
        let back = eng.adopt_room(&s, WarpRoom::for_session(&s, 4));
        assert_eq!(back.live_stores(), 1, "the strip keeps its own");
        s.live.columns[0].slots[1] = None;
        eng.retire_stale_warps(&s);
        assert_eq!(eng.take_retired_warps().len(), 4);
        assert_eq!(eng.slot_players().count(), 1);
    }
}
