//! Live decks: two CDJ-style players and a DJ mixer, played live over the master bus.
//!
//! The decks are a layer of the mix engine of their own, beside the arrangement and the Session
//! view: their state is never part of the session, is not saved and is not undoable. Commands
//! ([`DeckCmd`]) come from the host over its command channel; what each deck plays is published
//! back as a [`DecksState`] once a block.
//!
//! **Players.** A loaded deck holds its track ([`DeckTrack`]: the decoded audio, its beat grid,
//! key and cues) and two read heads, built off the audio thread ([`PreparedDeck::new`]). A head
//! plays the source either by varispeed (a 4-point Hermite resample: pitch follows the rate,
//! and at rate 1 the source comes out sample for sample) or, with master tempo (key lock) on,
//! through a Signalsmith stretcher that keeps the pitch. One head plays; the other is the spare
//! that takes over at a jump:
//! - **Jumps** (a seek, cue, hot cue, beat jump, a loop's seam, master tempo switched) start the
//!   spare at the target and crossfade from the playing head to it over [`XFADE_S`]
//!   (equal-gain raised cosine, which sums flat for the correlated audio of a loop). A jump that
//!   lands at a known moment (a loop's seam, a quantised hot cue) starts its crossfade
//!   [`XFADE_S`] early, the spare that far before its target, so the target sample sounds
//!   whole on the very sample the old position would have reached the seam: a beat loop is
//!   bar-exact. A stretcher just restarted is run unheard for its output latency first, so its
//!   first frames come out whole; for a quantised hot cue the spare is warmed up in the blocks
//!   before and kept in step, so the jump needs no restart.
//! - **Key-locked loops** don't jump: the playing head wraps what it feeds its stretcher (the
//!   loop's last [`XFADE_S`] crossfaded into the audio before its start, input side), so the
//!   stretcher never restarts, however short the loop.
//! - **Play** starts on the playhead's exact sample, hard; **pause** fades whatever sounds out
//!   over [`XFADE_S`] (both heads, in a jump's crossfade); the playhead stays where pause was
//!   pressed.
//!
//! **Transport.** CDJ cue: while paused, cue down sets the cue point at the playhead (on the beat
//! with quantise) and plays from it while held; release returns there and pauses. While
//! playing, cue down returns to the cue point and pauses. Jog nudge bends the rate by up to
//! [`JOG_BEND`] while jog events keep arriving ([`JOG_HOLD_S`] after the last); jog search moves
//! the playhead. Hot cues: eight, set (on the beat with quantise), jumped to (while playing,
//! with quantise on the next beat, so the beat stays on time; while paused it plays from the cue
//! while held and returns on release) and cleared. Loops: N beats (at least
//! [`MIN_LOOP_BEATS`]) from the nearest beat, loop in and out, halve, double, exit and reloop;
//! beat jump moves the playhead (and an active loop) by whole beats. Loading onto a deck that
//! sounds fades it out first ([`Decks::load`]).
//!
//! **Sync.** A synced deck follows its master: the other deck, unless that deck is synced too and
//! is not the one that started playing first. Each block the follower's tempo is set so its
//! beats run at the master's rate (from both grids, where the decks are). Its phase is put right
//! where that can't be heard: a follower that starts while its master plays, or is synced while
//! stopped, moves its playhead by its phase error first; one synced while both play and more
//! than [`SYNC_SNAP`] beats out jumps into phase (crossfaded). From then on a gentle trim of
//! −[`SYNC_GAIN`]·error (at most ±[`SYNC_MAX`]) holds the lock. A jog nudge on a follower
//! suspends the trim, and the phase it leaves is the one held. While either deck loops less than
//! a beat only the tempo follows. Sync off keeps the rate it had; the pitch fader then takes over
//! once it reaches that tempo.
//!
//! **Mixer.** Each deck runs through its [`DjStrip`] (trim, isolator EQ, filter, fader), then the
//! crossfader (per its curve and the deck's side), the master volume and a Maximizer at −1 dBTP
//! (true peak), into the main bus before the master faders; while the decks sound, the output
//! after the master faders goes through a second one ([`Decks::limit_output`]).
//!
//! Nothing here allocates, locks or frees on the audio thread: the heads' buffers and
//! stretchers come with the [`PreparedDeck`], and one taken off a deck goes back to the host to
//! be dropped ([`Decks::load`], [`Decks::take_retired`], [`Decks::drain`]).

use crate::dj_strip::{DjStrip, EqBand, FaderCurve, XfadeCurve, XfadeSide};
use soundcraft_dsp::{Plugin, db_to_gain};
use soundcraft_model::SourceAudio;
use soundcraft_stretch::{Quality, Stretcher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Decks in the layer.
pub const DECKS: usize = 2;
/// Hot cues per deck.
pub const HOT_CUES: usize = 8;
/// Crossfade at jumps and pauses, in seconds.
pub const XFADE_S: f64 = 0.002;
/// Longest crossfade in frames (2 ms up to 256 kHz).
const MAX_XF: usize = 512;
/// Largest span a head renders at once.
const HEAD_BLOCK: usize = 4096;
/// Longest stretcher warm-up (its output latency at 192 kHz).
const MAX_WARM: usize = 24_576;
/// The most a jog nudge bends the rate (fraction).
pub const JOG_BEND: f64 = 0.1;
/// How long a jog nudge holds after its last event, in seconds.
pub const JOG_HOLD_S: f64 = 0.06;
/// How far a jog search of `delta` 1 moves a deck without a beat grid, in seconds.
const JOG_SEARCH_S: f64 = 0.5;
/// Phase trim per beat of error (a bend of −gain × error).
pub const SYNC_GAIN: f64 = 0.8;
/// Largest phase-trim bend (fraction).
pub const SYNC_MAX: f64 = 0.02;
/// Beats out of phase beyond which syncing a playing deck jumps it into phase.
pub const SYNC_SNAP: f64 = 0.02;
/// How far (beats) a synced deck's phase error may move between blocks before it counts as a
/// jump (put right at once) rather than drift (trimmed).
pub const SYNC_JUMP: f64 = 0.05;
/// How near the held tempo (fraction) the pitch fader must come to take over after sync off.
const TAKEOVER: f64 = 0.0005;
/// How far (samples) a warmed-up spare may have drifted and still take over a jump as it is.
const SHADOW_TOLERANCE: f64 = 24.0;
/// The shortest loop, in beats.
pub const MIN_LOOP_BEATS: f64 = 1.0 / 16.0;
/// The pitch ranges a deck's pitch fader can have (fractions: ±8, ±16, ±50 %).
pub const PITCH_RANGES: [f64; 3] = [0.08, 0.16, 0.5];
/// The widest a synced deck's tempo goes from its track's own.
const SYNC_PITCH: f64 = 0.5;
/// Playback rates a head accepts.
const RATE_LIMITS: (f64, f64) = (0.1, 4.0);
/// How long the layer keeps rendering after the decks fall silent (filter and limiter tails).
const QUIET_S: f64 = 0.5;
/// Most beats a grid holds.
pub const MAX_GRID_BEATS: usize = 200_000;
/// The Maximizers' ceiling over the decks (dBTP).
pub const CEILING_DB: f32 = -1.0;

/// A deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeckId {
    A,
    B,
}

impl DeckId {
    pub const ALL: [DeckId; DECKS] = [DeckId::A, DeckId::B];

    pub fn index(self) -> usize {
        match self {
            DeckId::A => 0,
            DeckId::B => 1,
        }
    }

    pub fn other(self) -> DeckId {
        match self {
            DeckId::A => DeckId::B,
            DeckId::B => DeckId::A,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            DeckId::A => "a",
            DeckId::B => "b",
        }
    }

    pub fn parse(s: &str) -> Option<DeckId> {
        match s {
            "a" | "A" => Some(DeckId::A),
            "b" | "B" => Some(DeckId::B),
            _ => None,
        }
    }
}

/// A track's beat grid: the source sample of each beat, the first a downbeat. Beats before the
/// first and after the last are extrapolated at the first and last beat's length.
#[derive(Debug, Clone, PartialEq)]
pub struct BeatGrid {
    beats: Vec<f64>,
}

impl BeatGrid {
    /// A grid of `beats` (source samples): at least two, finite and strictly increasing.
    pub fn new(beats: Vec<f64>) -> Option<BeatGrid> {
        let ok = (2..=MAX_GRID_BEATS).contains(&beats.len())
            && beats.iter().all(|b| b.is_finite())
            && beats.windows(2).all(|w| matches!(w, [a, b] if b - a >= 1.0));
        ok.then_some(BeatGrid { beats })
    }

    /// A constant tempo: a beat every `period` samples from `first`, through `frames` samples.
    pub fn constant(first: f64, period: f64, frames: f64) -> Option<BeatGrid> {
        if !(first.is_finite() && period.is_finite() && period >= 1.0 && frames.is_finite()) {
            return None;
        }
        let n = ((frames - first) / period).ceil().max(1.0) + 1.0;
        let n = if n.is_finite() { (n as usize).clamp(2, MAX_GRID_BEATS) } else { 2 };
        BeatGrid::new((0..n).map(|i| first + i as f64 * period).collect())
    }

    /// The beats (source samples).
    pub fn beats(&self) -> &[f64] {
        &self.beats
    }

    /// The first downbeat (source sample).
    pub fn first(&self) -> f64 {
        self.beats.first().copied().unwrap_or(0.0)
    }

    /// The segment `[i, i + 1]` holding `pos` (the end segments for positions outside).
    fn segment(&self, pos: f64) -> (usize, f64, f64) {
        let n = self.beats.len();
        let i = self.beats.partition_point(|&b| b <= pos).saturating_sub(1).min(n.saturating_sub(2));
        let a = self.beats.get(i).copied().unwrap_or(0.0);
        let b = self.beats.get(i + 1).copied().unwrap_or(a + 1.0);
        (i, a, (b - a).max(1.0))
    }

    /// The beat (fractional, 0 at the first) at source sample `pos`.
    pub fn beat_at(&self, pos: f64) -> f64 {
        if !pos.is_finite() {
            return 0.0;
        }
        let (i, a, len) = self.segment(pos);
        i as f64 + (pos - a) / len
    }

    /// The source sample of (fractional) beat `beat`.
    pub fn pos_at(&self, beat: f64) -> f64 {
        if !beat.is_finite() {
            return self.first();
        }
        let n = self.beats.len();
        let i = if beat <= 0.0 { 0 } else { (beat.floor() as usize).min(n.saturating_sub(2)) };
        let a = self.beats.get(i).copied().unwrap_or(0.0);
        let b = self.beats.get(i + 1).copied().unwrap_or(a + 1.0);
        a + (beat - i as f64) * (b - a)
    }

    /// The length of the beat at `pos`, in source samples.
    pub fn period_at(&self, pos: f64) -> f64 {
        self.segment(pos).2
    }
}

/// A track loaded on a deck: its audio (at the session's rate), beat grid and what is known of
/// it. Shared between the host (which shows it) and the deck playing it.
#[derive(Debug)]
pub struct DeckTrack {
    pub source: Arc<SourceAudio>,
    pub grid: Option<BeatGrid>,
    /// The track's own tempo (its grid's at the first downbeat).
    pub bpm: Option<f64>,
    /// Camelot key, when known.
    pub key: Option<String>,
    /// Hot cues it comes with (source samples): mixforge's cues as A–D.
    pub cues: [Option<f64>; HOT_CUES],
    pub title: String,
    pub artist: String,
    pub path: String,
    /// Where the grid came from (`given`, `library`, `mixforge`, `tag`), if it has one.
    pub grid_from: Option<&'static str>,
    /// Its sections (intro, build, drop…) from mixforge's analysis, in order; none when unknown.
    pub sections: Vec<Section>,
}

/// A section of a track: where it starts and ends (source samples) and mixforge's label for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub start: f64,
    pub end: f64,
    pub label: String,
}

impl DeckTrack {
    /// Frames of audio.
    pub fn frames(&self) -> usize {
        self.source.buffer.channels.iter().map(Vec::len).max().unwrap_or(0)
    }

    /// Where a deck stands when the track is loaded: its first downbeat (when inside the audio),
    /// else the start.
    pub fn start(&self) -> f64 {
        self.grid.as_ref().map(BeatGrid::first).filter(|f| *f >= 0.0 && *f < self.frames() as f64).unwrap_or(0.0)
    }
}

/// The rates a stretcher ran at over its last frames out, so `Σ rate` over its output latency
/// gives that latency in source frames: how far what it puts out lags what it was fed.
#[derive(Debug, Clone, Copy)]
struct Lag {
    frames: [u32; 64],
    rates: [f64; 64],
    next: usize,
    len: usize,
}

impl Lag {
    const SLOTS: usize = 64;

    fn new() -> Lag {
        Lag { frames: [0; Lag::SLOTS], rates: [1.0; Lag::SLOTS], next: 0, len: 0 }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn push(&mut self, n: usize, rate: f64) {
        if let (Some(f), Some(r)) = (self.frames.get_mut(self.next), self.rates.get_mut(self.next)) {
            *f = u32::try_from(n).unwrap_or(u32::MAX);
            *r = rate;
        }
        self.next = (self.next + 1) % Lag::SLOTS;
        self.len = (self.len + 1).min(Lag::SLOTS);
    }

    /// Σ rate over the last `span` output frames (frames older than the ring at its oldest rate).
    fn sum(&self, span: usize) -> f64 {
        let mut left = span as f64;
        let mut sum = 0.0;
        let mut last = 1.0;
        for k in 0..self.len {
            let i = (self.next + Lag::SLOTS - 1 - k) % Lag::SLOTS;
            let (f, r) = (self.frames.get(i).map_or(0.0, |f| f64::from(*f)), self.rates.get(i).copied().unwrap_or(1.0));
            let take = f.min(left);
            sum += take * r;
            left -= take;
            last = r;
            if left <= 0.0 {
                break;
            }
        }
        sum + left.max(0.0) * last
    }
}

/// A loop a key-locked head plays by wrapping what it feeds its stretcher: source frames
/// `[a, a + len)` over and over, the last `xf` before the end crossfaded (input side) into those
/// before `a`, so the wrap is seamless and the stretcher never restarts.
#[derive(Debug, Clone, Copy, PartialEq)]
struct InLoop {
    a: f64,
    /// Its length in source frames, fractional (a beat loop is rarely whole frames).
    len: f64,
    xf: f64,
}

impl InLoop {
    fn end(&self) -> f64 {
        self.a + self.len
    }

    /// The source position fed or heard for virtual position `v`.
    fn map_f(&self, v: f64) -> f64 {
        if v < self.end() || self.len <= 0.0 { v } else { self.a + (v - self.a).rem_euclid(self.len) }
    }
}

/// Copies source frames `[start, start + n)` of every channel into `dst` (through the input loop
/// `lp`, when there is one), silence outside the audio.
fn fill(src: &[Vec<f32>], start: i64, n: usize, dst: &mut [Vec<f32>], lp: Option<InLoop>) {
    let nsrc = src.len();
    let at = |ch: Option<&Vec<f32>>, k: i64| usize::try_from(k).ok().and_then(|k| ch.and_then(|v| v.get(k))).copied().unwrap_or(0.0);
    let at_f = |ch: Option<&Vec<f32>>, p: f64| ch.map_or(0.0, |v| hermite(v, p));
    for (c, d) in dst.iter_mut().enumerate() {
        let ch = if nsrc == 0 { None } else { src.get(c % nsrc) };
        for (i, x) in d.iter_mut().take(n).enumerate() {
            let v = start.saturating_add(i as i64);
            *x = match lp {
                None => at(ch, v),
                Some(l) => {
                    let m = l.map_f(v as f64);
                    let from = l.end() - l.xf;
                    if l.xf > 0.0 && m >= from && m < l.end() {
                        let s = (std::f64::consts::FRAC_PI_2 * (m - from + 1.0) / (l.xf + 1.0)).sin();
                        let g = (s * s) as f32;
                        at_f(ch, m) * (1.0 - g) + at_f(ch, m - l.len) * g
                    } else {
                        at_f(ch, m)
                    }
                }
            };
        }
    }
}

/// A key-locked head's stretcher, its buffers, and where it reads.
struct Stretch {
    st: Stretcher,
    input: Vec<Vec<f32>>,
    out: Vec<Vec<f32>>,
    /// The (virtual) source position fed next, fractional.
    feed: f64,
    lag: Lag,
    lp: Option<InLoop>,
    /// Frames still to run unheard before the head is ready, and at what rate.
    warm_left: usize,
    warm_rate: f64,
    /// Frames the stretcher produced (heard or not) since last read.
    stretched: usize,
}

impl Stretch {
    /// Builds a stretcher for `channels` (allocates).
    fn new(channels: usize, sample_rate: f32) -> Option<Stretch> {
        let ch = channels.max(1);
        let st = Stretcher::new(ch, sample_rate, HEAD_BLOCK, Quality::Default).ok()?;
        let max_in = st.max_input();
        Some(Stretch {
            input: vec![vec![0.0; max_in]; ch],
            out: vec![vec![0.0; HEAD_BLOCK]; ch],
            st,
            feed: 0.0,
            lag: Lag::new(),
            lp: None,
            warm_left: 0,
            warm_rate: 1.0,
            stretched: 0,
        })
    }

    /// The source position heard now: what was fed, less the stretcher's latency in source
    /// frames (its output latency at the rates it ran at), through the input loop.
    fn heard(&self) -> f64 {
        let v = self.feed - self.st.input_latency() as f64 - self.lag.sum(self.st.output_latency());
        self.lp.map_or(v, |l| l.map_f(v))
    }

    fn warm_span(&self) -> usize {
        self.st.output_latency().min(MAX_WARM)
    }

    /// Restarts at source position `at`, primed with the audio before it. Ready once it has run
    /// unheard for its output latency ([`Stretch::warm`]); the next frame out is then `at`, whole.
    fn start(&mut self, src: &[Vec<f32>], at: f64, rate: f64, lp: Option<InLoop>) {
        self.st.reset();
        self.lag.clear();
        self.lp = lp;
        self.feed = at + self.st.input_latency() as f64;
        let pre = (self.st.input_latency() * 2 + 1024).min(self.st.max_input());
        fill(src, self.feed.floor() as i64 - pre as i64, pre, &mut self.input, lp);
        let _ = self.st.seek(&self.input, pre, rate);
        self.warm_left = self.warm_span();
        self.warm_rate = rate;
    }

    /// Runs up to `budget` frames of the warm-up still to go; returns how many ran.
    fn warm(&mut self, src: &[Vec<f32>], budget: usize, rate: f64) -> usize {
        if rate.is_finite() && rate > 0.0 {
            self.warm_rate = rate;
        }
        let mut done = 0;
        while self.warm_left > 0 && done < budget {
            let n = self.warm_left.min(HEAD_BLOCK).min(budget - done);
            self.run(src, n, self.warm_rate);
            self.warm_left -= n;
            done += n;
        }
        done
    }

    /// Produces `n` frames (at most [`HEAD_BLOCK`]) into `out` at `rate`.
    fn run(&mut self, src: &[Vec<f32>], n: usize, rate: f64) -> bool {
        let n = n.min(HEAD_BLOCK);
        let a = self.feed.floor() as i64;
        let end = self.feed + n as f64 * rate;
        let want = usize::try_from((end.floor() as i64).saturating_sub(a)).unwrap_or(0);
        let n_in = want.min(self.st.max_input());
        fill(src, a, n_in, &mut self.input, self.lp);
        for c in self.out.iter_mut() {
            c.iter_mut().take(n).for_each(|x| *x = 0.0);
        }
        let ok = self.st.process(&self.input, n_in, &mut self.out, n, 1.0).is_ok();
        self.feed = if n_in == want { end } else { a as f64 + n_in as f64 };
        self.lag.push(n, rate);
        self.stretched += n;
        ok
    }
}

/// 4-point Hermite interpolation of `s` at `pos` (exact on whole samples; silence outside).
fn hermite(s: &[f32], pos: f64) -> f32 {
    if !pos.is_finite() {
        return 0.0;
    }
    let i = pos.floor();
    let t = (pos - i) as f32;
    let i = i as i64;
    let at = |k: i64| usize::try_from(k).ok().and_then(|k| s.get(k)).copied().unwrap_or(0.0);
    let x0 = at(i);
    if t == 0.0 {
        return x0;
    }
    let (xm, x1, x2) = (at(i.saturating_sub(1)), at(i.saturating_add(1)), at(i.saturating_add(2)));
    let c1 = 0.5 * (x1 - xm);
    let c2 = xm - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
    let c3 = 0.5 * (x2 - xm) + 1.5 * (x0 - x1);
    ((c3 * t + c2) * t + c1) * t + x0
}

/// A read head: varispeed, or key-locked through its stretcher.
struct Head {
    /// The varispeed position (source samples).
    pos: f64,
    keylock: bool,
    /// Started for where it plays (since the deck last stopped or jumped).
    ready: bool,
    stretch: Option<Stretch>,
}

impl Head {
    fn position(&self) -> f64 {
        match (&self.stretch, self.keylock) {
            (Some(s), true) => s.heard(),
            _ => self.pos,
        }
    }

    /// The input loop it plays (key-locked only).
    fn in_loop(&self) -> Option<InLoop> {
        self.stretch.as_ref().filter(|_| self.keylock).and_then(|s| s.lp)
    }

    /// Frames still to warm up before it can be heard.
    fn warming(&self) -> usize {
        self.stretch.as_ref().filter(|_| self.keylock).map_or(0, |s| s.warm_left)
    }

    /// Starts the head at source position `at`, key-locked when asked (and it can be), through
    /// input loop `lp`. Unless `lazy`, a key-locked head warms up now.
    fn start(&mut self, src: &[Vec<f32>], at: f64, rate: f64, keylock: bool, lp: Option<InLoop>, lazy: bool) {
        self.keylock = keylock && self.stretch.is_some();
        self.pos = at;
        if self.keylock
            && let Some(s) = self.stretch.as_mut()
        {
            s.start(src, at, rate, lp);
            if !lazy {
                s.warm(src, usize::MAX, rate);
            }
        }
        self.ready = true;
    }

    /// Runs `n` frames unheard: warming up, or keeping in step with the playing head.
    fn shadow(&mut self, src: &[Vec<f32>], n: usize, rate: f64) {
        let warmed = if self.warming() > 0 { self.stretch.as_mut().map_or(0, |s| s.warm(src, n, rate)) } else { 0 };
        if warmed < n {
            self.render(src, n - warmed, rate, &mut [], 0, |_| 0.0);
        }
    }

    /// Adds `n` frames (at most [`HEAD_BLOCK`]) at `rate` into `out` from frame `off`, times
    /// `gain(k)` for frame `k`.
    fn render(&mut self, src: &[Vec<f32>], n: usize, rate: f64, out: &mut [Vec<f32>], off: usize, gain: impl Fn(usize) -> f32) {
        let n = n.min(HEAD_BLOCK);
        if src.is_empty() || n == 0 {
            return;
        }
        if self.keylock
            && let Some(s) = self.stretch.as_mut()
        {
            // A warm-up a lazy start left finishes before anything is heard.
            s.warm(src, usize::MAX, rate);
            if s.run(src, n, rate) {
                let nout = s.out.len().max(1);
                for (ch, dst) in out.iter_mut().enumerate() {
                    let Some(o) = s.out.get(ch % nout) else { continue };
                    for (k, v) in o.iter().take(n).enumerate() {
                        if let Some(d) = dst.get_mut(off + k) {
                            *d += v * gain(k);
                        }
                    }
                }
            }
            return;
        }
        let p0 = self.pos;
        for (ch, dst) in out.iter_mut().enumerate() {
            let Some(s) = src.get(ch % src.len()) else { continue };
            for k in 0..n {
                if let Some(d) = dst.get_mut(off + k) {
                    *d += hermite(s, p0 + k as f64 * rate) * gain(k);
                }
            }
        }
        self.pos = p0 + n as f64 * rate;
    }

    /// Frames its stretcher produced since last asked.
    fn take_stretched(&mut self) -> usize {
        self.stretch.as_mut().map_or(0, |s| std::mem::take(&mut s.stretched))
    }
}

/// The next [`PreparedDeck::load`].
static NEXT_LOAD: AtomicU64 = AtomicU64::new(1);

/// A track ready to play on a deck: the track and its two read heads (stretchers and buffers
/// included). Built off the audio thread; the deck it replaces comes back to be dropped there.
pub struct PreparedDeck {
    track: Arc<DeckTrack>,
    heads: [Head; 2],
    load: u64,
}

impl PreparedDeck {
    /// Builds the heads for `track` at `sample_rate` (allocates; call off the audio thread).
    pub fn new(track: Arc<DeckTrack>, sample_rate: f32) -> PreparedDeck {
        let ch = track.source.buffer.channels.len().max(1);
        let head = || Head { pos: 0.0, keylock: false, ready: false, stretch: Stretch::new(ch, sample_rate) };
        PreparedDeck { heads: [head(), head()], track, load: NEXT_LOAD.fetch_add(1, Ordering::Relaxed) }
    }

    pub fn track(&self) -> &Arc<DeckTrack> {
        &self.track
    }

    /// Identifies this load in [`DeckState::load`].
    pub fn load(&self) -> u64 {
        self.load
    }

    /// Whether master tempo (key lock) can be used: the stretchers were built.
    pub fn can_key_lock(&self) -> bool {
        self.heads.iter().all(|h| h.stretch.is_some())
    }

    /// Lengths and capacities of its heads' buffers (they never change once built).
    #[doc(hidden)]
    pub fn storage(&self, v: &mut Vec<(usize, usize)>) {
        for s in self.heads.iter().filter_map(|h| h.stretch.as_ref()) {
            v.extend(s.input.iter().chain(s.out.iter()).map(|c| (c.len(), c.capacity())));
        }
    }
}

/// How a jog event moves a deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JogMode {
    /// A temporary rate bend while the events keep coming.
    Nudge,
    /// Moves the playhead.
    Search,
}

/// What a hot cue button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotCueAction {
    Set,
    /// Pressed: a jump while playing; while stopped, plays from the cue while held.
    Jump,
    /// Released: ends a preview the jump started (back to the cue, paused).
    Release,
    Clear,
}

/// A loop control.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoopAction {
    /// A loop of this many beats from the nearest beat (on the grid with quantise).
    Beats(f64),
    In,
    Out,
    Halve,
    Double,
    Exit,
    Reloop,
}

/// A command for the decks (applied at the start of the next block).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeckCmd {
    Play(DeckId),
    Pause(DeckId),
    /// The cue button pressed (`down`) or released.
    Cue {
        deck: DeckId,
        down: bool,
    },
    /// Moves the playhead to source sample `at`.
    Seek {
        deck: DeckId,
        at: f64,
    },
    /// A jog event: `delta` −1..=1 (nudge: the bend's share of [`JOG_BEND`]; search: beats, or
    /// half-seconds without a grid).
    Jog {
        deck: DeckId,
        delta: f64,
        mode: JogMode,
    },
    /// The pitch fader (a fraction, within the range), and optionally its range. With `takeover`
    /// (a hardware fader, which may sit anywhere) it takes over from a held tempo only once it
    /// reaches it; without (an on-screen fader, drawn at the tempo) it applies at once.
    Pitch {
        deck: DeckId,
        value: f64,
        range: Option<f64>,
        takeover: bool,
    },
    MasterTempo {
        deck: DeckId,
        on: bool,
    },
    Sync {
        deck: DeckId,
        on: bool,
    },
    HotCue {
        deck: DeckId,
        n: usize,
        action: HotCueAction,
        /// The edit's number (set and clear): the deck reports the last it applied
        /// ([`DeckState::hot_seq`]), so the UI knows where the edit landed. 0 for none.
        seq: u64,
    },
    Loop {
        deck: DeckId,
        action: LoopAction,
    },
    BeatJump {
        deck: DeckId,
        beats: f64,
    },
    Eq {
        deck: DeckId,
        band: EqBand,
        db: f32,
    },
    Filter {
        deck: DeckId,
        value: f32,
    },
    Trim {
        deck: DeckId,
        db: f32,
    },
    Fader {
        deck: DeckId,
        value: f32,
        curve: Option<FaderCurve>,
    },
    Crossfader {
        value: f32,
        curve: Option<XfadeCurve>,
    },
    Assign {
        deck: DeckId,
        side: XfadeSide,
    },
    /// Master volume (dB, at most 0).
    Master {
        db: f32,
    },
    Quantise(bool),
    /// Pauses both decks (the arrangement started).
    StopAll,
}

/// What a deck plays, as published for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DeckState {
    /// The load playing ([`PreparedDeck::load`]), 0 for none.
    pub load: u64,
    pub playing: bool,
    /// Playing while the cue button is held.
    pub previewing: bool,
    /// The playhead (source samples).
    pub position: f64,
    /// The playback rate this block.
    pub rate: f64,
    /// The pitch fader (fraction) and its range.
    pub pitch: f64,
    pub range: f64,
    /// Tempo now: the grid's at the playhead times the rate (0 without a grid).
    pub bpm: f64,
    pub key_lock: bool,
    pub synced: bool,
    /// The other deck follows this one.
    pub master: bool,
    /// Beats this deck is ahead of its master (wrapped to ±½), while both play.
    pub phase_error: Option<f64>,
    pub cue: f64,
    pub hot: [Option<f64>; HOT_CUES],
    /// The last hot cue edit (set or clear) applied: its `seq` ([`DeckCmd::HotCue`]).
    pub hot_seq: u64,
    pub loop_in: Option<f64>,
    pub loop_out: Option<f64>,
    pub loop_on: bool,
    /// Post-fader peaks (L, R) of the last block.
    pub level: [f32; 2],
    pub trim_db: f32,
    pub eq_db: [f32; 3],
    pub filter: f32,
    pub fader: f32,
    pub fader_curve: FaderCurve,
    pub side: XfadeSide,
}

/// The decks and the mixer, as published for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DecksState {
    pub decks: [DeckState; DECKS],
    pub xfader: f32,
    pub xfader_curve: XfadeCurve,
    pub master_db: f32,
    /// Master peaks (L, R) after the Maximizer, of the last block.
    pub master_level: [f32; 2],
    pub quantise: bool,
}

/// Words per deck in a [`DecksState`]'s encoding.
const DECK_WORDS: usize = 28;
/// Words in a [`DecksState`]'s encoding (for lock-free publishing).
pub const STATE_WORDS: usize = DECKS * DECK_WORDS + 5;

fn opt(v: Option<f64>) -> u64 {
    v.unwrap_or(f64::NAN).to_bits()
}

fn unopt(w: u64) -> Option<f64> {
    let v = f64::from_bits(w);
    v.is_finite().then_some(v)
}

fn f32w(v: f32) -> u64 {
    u64::from(v.to_bits())
}

fn wf32(w: u64) -> f32 {
    f32::from_bits(u32::try_from(w & 0xFFFF_FFFF).unwrap_or(0))
}

fn fader_code(c: FaderCurve) -> u64 {
    match c {
        FaderCurve::Linear => 0,
        FaderCurve::Smooth => 1,
        FaderCurve::Sharp => 2,
    }
}

impl DecksState {
    /// The state as words (see [`STATE_WORDS`]), for a seqlock.
    pub fn encode(&self) -> [u64; STATE_WORDS] {
        let mut w = [0u64; STATE_WORDS];
        let mut i = 0;
        let mut put = |v: u64| {
            if let Some(x) = w.get_mut(i) {
                *x = v;
            }
            i += 1;
        };
        for d in &self.decks {
            let flags = u64::from(d.playing)
                | u64::from(d.previewing) << 1
                | u64::from(d.key_lock) << 2
                | u64::from(d.synced) << 3
                | u64::from(d.master) << 4
                | u64::from(d.loop_on) << 5
                | fader_code(d.fader_curve) << 6
                | d.side.code() << 8;
            for v in
                [d.load, flags, d.position.to_bits(), d.rate.to_bits(), d.pitch.to_bits(), d.range.to_bits(), d.bpm.to_bits(), opt(d.phase_error)]
            {
                put(v);
            }
            put(d.cue.to_bits());
            for h in d.hot {
                put(opt(h));
            }
            put(d.hot_seq);
            for v in [opt(d.loop_in), opt(d.loop_out), f32w(d.level[0]), f32w(d.level[1]), f32w(d.trim_db)] {
                put(v);
            }
            for e in d.eq_db {
                put(f32w(e));
            }
            put(f32w(d.filter));
            put(f32w(d.fader));
        }
        let mixer = u64::from(self.quantise) | u64::from(self.xfader_curve == XfadeCurve::Sharp) << 1;
        for v in [f32w(self.xfader), mixer, f32w(self.master_db), f32w(self.master_level[0]), f32w(self.master_level[1])] {
            put(v);
        }
        w
    }

    /// The state [`DecksState::encode`] gave `w`.
    pub fn decode(w: &[u64; STATE_WORDS]) -> DecksState {
        let mut i = 0;
        let mut get = || {
            let v = w.get(i).copied().unwrap_or(0);
            i += 1;
            v
        };
        let mut out = DecksState::default();
        for d in out.decks.iter_mut() {
            d.load = get();
            let flags = get();
            d.playing = flags & 1 != 0;
            d.previewing = flags & 2 != 0;
            d.key_lock = flags & 4 != 0;
            d.synced = flags & 8 != 0;
            d.master = flags & 16 != 0;
            d.loop_on = flags & 32 != 0;
            d.fader_curve = match (flags >> 6) & 3 {
                1 => FaderCurve::Smooth,
                2 => FaderCurve::Sharp,
                _ => FaderCurve::Linear,
            };
            d.side = XfadeSide::from_code((flags >> 8) & 3);
            d.position = f64::from_bits(get());
            d.rate = f64::from_bits(get());
            d.pitch = f64::from_bits(get());
            d.range = f64::from_bits(get());
            d.bpm = f64::from_bits(get());
            d.phase_error = unopt(get());
            d.cue = f64::from_bits(get());
            for h in d.hot.iter_mut() {
                *h = unopt(get());
            }
            d.hot_seq = get();
            d.loop_in = unopt(get());
            d.loop_out = unopt(get());
            d.level = [wf32(get()), wf32(get())];
            d.trim_db = wf32(get());
            for e in d.eq_db.iter_mut() {
                *e = wf32(get());
            }
            d.filter = wf32(get());
            d.fader = wf32(get());
        }
        out.xfader = wf32(get());
        let mixer = get();
        out.quantise = mixer & 1 != 0;
        out.xfader_curve = if mixer & 2 != 0 { XfadeCurve::Sharp } else { XfadeCurve::Smooth };
        out.master_db = wf32(get());
        out.master_level = [wf32(get()), wf32(get())];
        out
    }
}

/// Equal-gain fade-in gain of frame `i` of an `n`-frame crossfade (the fade-out is `1 −` it).
fn fade_in(i: usize, n: usize) -> f32 {
    if i >= n {
        return 1.0;
    }
    let s = (std::f64::consts::FRAC_PI_2 * (i + 1) as f64 / (n + 1) as f64).sin();
    (s * s) as f32
}

fn fade_out(i: usize, n: usize) -> f32 {
    1.0 - fade_in(i, n)
}

/// A head fading out over `total` frames (`left` to go), from gain `scale`.
#[derive(Debug, Clone, Copy)]
struct Out {
    left: usize,
    total: usize,
    scale: f32,
}

impl Out {
    fn gain(&self, k: usize) -> f32 {
        self.scale * fade_out(self.total - self.left + k, self.total)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct LoopSpan {
    a: Option<f64>,
    b: Option<f64>,
    on: bool,
}

/// One deck's transport.
pub(crate) struct Deck {
    loaded: Option<Box<PreparedDeck>>,
    /// A track waiting for the playing one to fade out ([`Decks::load`] while it plays).
    next: Option<Box<PreparedDeck>>,
    /// A track taken off the deck, for the host to drop ([`Decks::take_retired`]).
    retired: Option<Box<PreparedDeck>>,
    active: usize,
    playing: bool,
    /// Playing while a cue or hot cue is held; its release returns to `preview_from`.
    previewing: bool,
    preview_from: f64,
    /// The playhead while stopped (while playing, the playing head's position).
    playhead: f64,
    cue: f64,
    hot: [Option<f64>; HOT_CUES],
    /// The last hot cue edit applied (its `seq`).
    hot_seq: u64,
    lp: LoopSpan,
    /// A quantised jump waiting for its beat: (at, to), source samples.
    pending: Option<(f64, f64)>,
    /// The spare is warming up and running in step for `pending` (key lock).
    shadowing: bool,
    /// A jump to make as soon as the fades under way end.
    now: Option<f64>,
    /// Each head's fade-out, and the playing head's fade-in (frames left, total).
    outs: [Option<Out>; 2],
    fin: Option<(usize, usize)>,
    pitch: f64,
    range: f64,
    keylock: bool,
    synced: bool,
    /// After sync off, the pitch fader takes over only once it reaches the tempo held:
    /// (pitch held, which side of it the fader was last).
    takeover: Option<(f64, Option<bool>)>,
    /// When it started playing ([`Decks::clock`]), for which synced deck leads.
    started: u64,
    bend: f64,
    bend_left: usize,
    /// A nudge moved a synced deck: its phase against the master is kept where the nudge left it.
    nudged: bool,
    sync_offset: f64,
    /// The phase error (beats) of the last block it was trimmed against: a jump in it (a seek, its
    /// master starting) is put right with one crossfaded jump, not the trim.
    last_err: Option<f64>,
    /// Where the pitch fader was last moved to (fraction), for soft takeover.
    fader_at: Option<f64>,
    /// This block's rate, phase correction and phase error.
    rate: f64,
    correction: f64,
    phase_err: Option<f64>,
    master: bool,
}

impl Deck {
    fn new() -> Deck {
        Deck {
            loaded: None,
            next: None,
            retired: None,
            active: 0,
            playing: false,
            previewing: false,
            preview_from: 0.0,
            playhead: 0.0,
            cue: 0.0,
            hot: [None; HOT_CUES],
            hot_seq: 0,
            lp: LoopSpan::default(),
            pending: None,
            shadowing: false,
            now: None,
            outs: [None; 2],
            fin: None,
            pitch: 0.0,
            range: 0.08,
            keylock: false,
            synced: false,
            takeover: None,
            started: 0,
            bend: 0.0,
            bend_left: 0,
            nudged: false,
            sync_offset: 0.0,
            last_err: None,
            fader_at: None,
            rate: 1.0,
            correction: 0.0,
            phase_err: None,
            master: false,
        }
    }

    fn grid(&self) -> Option<&BeatGrid> {
        self.loaded.as_ref().and_then(|l| l.track.grid.as_ref())
    }

    fn frames(&self) -> f64 {
        self.loaded.as_ref().map_or(0.0, |l| l.track.frames() as f64)
    }

    /// A crossfade or fade-out is under way.
    fn busy(&self) -> bool {
        self.fin.is_some() || self.outs.iter().any(Option::is_some)
    }

    /// Something is heard: playing, or fading out.
    fn sounding(&self) -> bool {
        self.playing || self.outs.iter().any(Option::is_some)
    }

    /// The playhead (source samples).
    fn pos(&self) -> f64 {
        if self.playing { self.loaded.as_ref().and_then(|l| l.heads.get(self.active)).map_or(self.playhead, Head::position) } else { self.playhead }
    }

    /// The rate to start a head at.
    fn rate_now(&self) -> f64 {
        if self.rate.is_finite() && self.rate > 0.0 { self.rate } else { 1.0 + self.pitch }
    }

    /// `p` on the nearest beat with `quantise` (and a grid).
    fn snap(&self, p: f64, quantise: bool) -> f64 {
        match self.grid().filter(|_| quantise) {
            Some(g) => g.pos_at(g.beat_at(p).round()),
            None => p,
        }
    }

    /// `p` moved by `beats` on the grid.
    fn shifted(&self, p: f64, beats: f64) -> f64 {
        self.grid().map_or(p, |g| g.pos_at(g.beat_at(p) + beats))
    }

    /// Where a move to `target` lands while playing with `quantise`: on the playhead's beat
    /// fraction, the beat nearest `target` with it, so the phase (and any sync) holds.
    fn phase_kept(&self, target: f64, quantise: bool) -> f64 {
        match self.grid().filter(|_| quantise && self.playing) {
            Some(g) => {
                let b = g.beat_at(self.pos());
                let phi = b - b.floor();
                g.pos_at((g.beat_at(target) - phi).round() + phi)
            }
            None => target,
        }
    }

    /// The loop length in beats, while a loop is on.
    fn loop_beats(&self) -> Option<f64> {
        let (Some(a), Some(b)) = (self.lp.a, self.lp.b) else { return None };
        if !self.lp.on {
            return None;
        }
        Some(self.grid().map_or(f64::INFINITY, |g| g.beat_at(b) - g.beat_at(a)))
    }

    /// The input loop a key-locked head plays the active loop through, when it can.
    fn in_loop(&self, xf: usize) -> Option<InLoop> {
        let (Some(a), Some(b)) = (self.lp.a, self.lp.b) else { return None };
        let (len, xf) = (b - a, xf as f64);
        (self.keylock && self.lp.on && len > 2.0 * xf + 2.0).then_some(InLoop { a, len, xf })
    }

    fn unready(&mut self) {
        if let Some(l) = self.loaded.as_mut() {
            l.heads.iter_mut().for_each(|h| h.ready = false);
        }
        self.shadowing = false;
    }

    fn load(&mut self, deck: Box<PreparedDeck>) -> Option<Box<PreparedDeck>> {
        let start = deck.track.start();
        self.hot = deck.track.cues;
        let old = self.loaded.replace(deck);
        self.active = 0;
        self.playing = false;
        self.previewing = false;
        self.playhead = start;
        self.cue = start;
        self.preview_from = start;
        self.lp = LoopSpan::default();
        self.pending = None;
        self.now = None;
        self.outs = [None; 2];
        self.fin = None;
        self.bend = 0.0;
        self.bend_left = 0;
        self.phase_err = None;
        self.unready();
        old
    }

    /// The head to start: the playing one, unless it is fading out (then the other, or the one
    /// nearer the end of its fade, cut).
    fn free_head(&mut self) -> usize {
        let a = self.active.min(1);
        let o = 1 - a;
        match (self.outs[a], self.outs[o]) {
            (None, _) => a,
            (Some(_), None) => o,
            (Some(x), Some(y)) => {
                let h = if x.left <= y.left { a } else { o };
                self.outs[h] = None;
                h
            }
        }
    }

    fn play(&mut self, clock: u64, xf: usize) {
        if self.playing {
            // Play while a cue is held: it plays on when the cue is released.
            self.previewing = false;
            return;
        }
        if self.loaded.is_none() {
            return;
        }
        self.active = self.free_head();
        let (at, rate, keylock, lp) = (self.playhead, self.rate_now(), self.keylock, self.in_loop(xf));
        let Some(ld) = self.loaded.as_deref_mut() else { return };
        let PreparedDeck { track, heads, .. } = ld;
        if let Some(h) = heads.get_mut(self.active)
            && !(h.ready && h.keylock == (keylock && h.stretch.is_some()) && h.in_loop() == lp && (h.position() - at).abs() < 0.5)
        {
            h.start(&track.source.buffer.channels, at, rate, keylock, lp, false);
        }
        self.playing = true;
        self.started = clock;
    }

    fn pause(&mut self, xf: usize) {
        if !self.playing {
            return;
        }
        let p = self.pos();
        // The playing head fades out from where it is in any fade-in; a head already fading out
        // goes on fading.
        let scale = self.fin.map_or(1.0, |(left, total)| fade_in(total - left, total));
        if let Some(o) = self.outs.get_mut(self.active) {
            *o = Some(Out { left: xf, total: xf, scale });
        }
        self.fin = None;
        self.playing = false;
        self.previewing = false;
        self.playhead = p;
        self.pending = None;
        self.now = None;
        self.unready();
    }

    /// Sets the cue point at the playhead (on the beat with quantise), there.
    fn set_cue_here(&mut self, quantise: bool) {
        let p = self.snap(self.playhead, quantise).clamp(0.0, self.frames());
        self.cue = p;
        self.playhead = p;
        self.unready();
    }

    /// Starts a preview (a cue or hot cue held), which its release ends at `from`.
    fn preview(&mut self, from: f64, clock: u64, xf: usize) {
        self.play(clock, xf);
        self.previewing = true;
        self.preview_from = from;
    }

    /// A cue or hot cue released: a preview returns where it started and pauses.
    fn release(&mut self, xf: usize) {
        if self.previewing {
            self.pause(xf);
            self.playhead = self.preview_from;
        }
    }

    /// Moves the playhead to `target`: a crossfaded jump while playing.
    fn jump(&mut self, target: f64, xf: usize) {
        let target = target.clamp(0.0, self.frames());
        if !self.playing {
            self.playhead = target;
            self.unready();
        } else if self.busy() {
            self.now = Some(target);
        } else {
            self.fire(target - self.pos(), xf);
        }
    }

    /// Starts the spare `delta` samples from where the playing head is and crossfades to it.
    /// A spare already running in step for this jump takes over as it is.
    fn fire(&mut self, delta: f64, xf: usize) {
        let (p, rate, keylock) = (self.pos(), self.rate_now(), self.keylock);
        let spare = 1 - self.active.min(1);
        let target = p + delta;
        let lp = self.in_loop(xf).filter(|l| target < l.end());
        let shadowed = std::mem::take(&mut self.shadowing);
        let Some(ld) = self.loaded.as_deref_mut() else { return };
        let PreparedDeck { track, heads, .. } = ld;
        if let Some(h) = heads.get_mut(spare) {
            let in_step = shadowed && h.ready && h.keylock == keylock && h.warming() == 0 && (h.position() - target).abs() <= SHADOW_TOLERANCE;
            if !in_step {
                h.start(&track.source.buffer.channels, target, rate, keylock, lp, false);
            }
        }
        if let Some(o) = self.outs.get_mut(self.active) {
            *o = Some(Out { left: xf, total: xf, scale: 1.0 });
        }
        self.fin = Some((xf, xf));
        self.active = spare;
    }

    /// The next jump due at a known position: (at, delta, one-shot).
    fn next_event(&self) -> Option<(f64, f64, bool)> {
        let p = self.pos();
        let mut best = self.pending.map(|(at, to)| (at, to - at, true));
        // A key-locked head plays the loop itself (input side); others jump at its end.
        let looped_inside = self.loaded.as_ref().and_then(|l| l.heads.get(self.active)).is_some_and(|h| h.in_loop().is_some());
        if self.lp.on
            && !looped_inside
            && let (Some(a), Some(b)) = (self.lp.a, self.lp.b)
            && b > a
        {
            let seam = if p < b {
                (b, a - b, false)
            } else {
                // Past the loop's end (it was just set or shortened): back inside it now.
                let len = b - a;
                let k = ((p - a) / len).floor().max(1.0);
                (p, -k * len, false)
            };
            if best.is_none_or(|e| seam.0 < e.0) {
                best = Some(seam);
            }
        }
        best
    }

    /// A key-locked deck's loop changed while playing: a head with the right input loop takes
    /// over (once), inside the loop.
    fn relink(&mut self, xf: usize) {
        if !self.playing || !self.keylock {
            return;
        }
        let want = self.in_loop(xf);
        let have = self.loaded.as_ref().and_then(|l| l.heads.get(self.active)).and_then(Head::in_loop);
        if want == have {
            return;
        }
        let p = self.pos();
        let target = match (want, self.lp.a, self.lp.b) {
            (Some(_), Some(a), Some(b)) if p >= b && b > a => a + (p - a).rem_euclid(b - a),
            _ => p,
        };
        if self.busy() {
            self.now = Some(target);
        } else {
            self.fire(target - p, xf);
        }
    }

    /// Starts the spare warming up for the pending quantised jump (key lock), so the jump needs
    /// no restart: it is started where the jump will land, ahead by its warm-up, and runs in
    /// step from then on ([`Head::shadow`]).
    fn prewarm(&mut self) {
        if !(self.playing && self.keylock) || self.busy() {
            return;
        }
        let Some((at, to)) = self.pending else {
            self.shadowing = false;
            return;
        };
        if self.shadowing {
            return;
        }
        let (p, rate) = (self.pos(), self.rate_now());
        let spare = 1 - self.active.min(1);
        let Some(ld) = self.loaded.as_deref_mut() else { return };
        let PreparedDeck { track, heads, .. } = ld;
        let Some(h) = heads.get_mut(spare) else { return };
        let span = h.stretch.as_ref().map_or(0, Stretch::warm_span) as f64;
        h.start(&track.source.buffer.channels, p + (to - at) + span * rate, rate, true, None, true);
        self.shadowing = true;
    }

    /// Adds `n` frames of the deck into `out` (stereo). A deck that reaches the end of its
    /// track pauses there.
    fn render(&mut self, out: &mut [Vec<f32>], n: usize, xf: usize) {
        self.prewarm();
        let mut i = 0;
        let mut guard = 0;
        while i < n {
            guard += 1;
            if guard > 64 + n {
                break;
            }
            if self.playing
                && !self.busy()
                && let Some(t) = self.now.take()
            {
                self.fire(t - self.pos(), xf);
                continue;
            }
            let mut seg = (n - i).min(HEAD_BLOCK);
            if self.playing
                && !self.busy()
                && let Some((at, delta, once)) = self.next_event()
            {
                let r = self.rate.max(1e-3);
                let fire_at = at - xf as f64 * r;
                let p = self.pos();
                if p >= fire_at {
                    if once {
                        self.pending = None;
                    }
                    self.fire(delta, xf);
                    continue;
                }
                let m = ((fire_at - p) / r).ceil();
                if m < seg as f64 {
                    seg = (m as usize).max(1);
                }
            }
            for o in self.outs.iter().flatten() {
                seg = seg.min(o.left.max(1));
            }
            if let Some((left, _)) = self.fin {
                seg = seg.min(left.max(1));
            }
            if !self.sounding() {
                break;
            }
            let (rate, outs, fin, active, playing, shadowing) = (self.rate, self.outs, self.fin, self.active.min(1), self.playing, self.shadowing);
            let Some(ld) = self.loaded.as_deref_mut() else { break };
            let PreparedDeck { track, heads, .. } = ld;
            let src = &track.source.buffer.channels;
            for (h, o) in heads.iter_mut().zip(outs.iter()) {
                if let Some(o) = o {
                    h.render(src, seg, rate, out, i, |k| o.gain(k));
                }
            }
            if playing && let Some(h) = heads.get_mut(active) {
                match fin {
                    Some((left, total)) => h.render(src, seg, rate, out, i, |k| fade_in(total - left + k, total)),
                    None => h.render(src, seg, rate, out, i, |_| 1.0),
                }
            }
            if playing
                && shadowing
                && outs.get(1 - active).is_some_and(Option::is_none)
                && let Some(h) = heads.get_mut(1 - active)
            {
                h.shadow(src, seg, rate);
            }
            for o in self.outs.iter_mut() {
                if let Some(x) = o {
                    x.left = x.left.saturating_sub(seg);
                    if x.left == 0 {
                        *o = None;
                    }
                }
            }
            if let Some((left, total)) = self.fin {
                let left = left.saturating_sub(seg);
                self.fin = (left > 0).then_some((left, total));
            }
            i += seg;
        }
        if self.playing && self.pos() >= self.frames() {
            self.pause(xf);
            self.playhead = self.frames();
        }
        // A track waiting to load takes the deck once the one before is silent.
        if !self.sounding()
            && self.retired.is_none()
            && let Some(n) = self.next.take()
        {
            self.retired = self.load(n);
        }
    }

    fn hot_cue(&mut self, n: usize, action: HotCueAction, seq: u64, quantise: bool, clock: u64, xf: usize) {
        if self.loaded.is_none() {
            return;
        }
        let p = self.pos();
        match action {
            HotCueAction::Set => {
                let at = self.snap(p, quantise);
                if let Some(h) = self.hot.get_mut(n) {
                    *h = Some(at);
                }
                self.hot_seq = self.hot_seq.max(seq);
            }
            HotCueAction::Clear => {
                if let Some(h) = self.hot.get_mut(n) {
                    *h = None;
                }
                self.hot_seq = self.hot_seq.max(seq);
            }
            HotCueAction::Release => self.release(xf),
            HotCueAction::Jump => {
                let Some(to) = self.hot.get(n).copied().flatten() else { return };
                if let (Some(a), Some(b)) = (self.lp.a, self.lp.b)
                    && (to < a || to >= b)
                {
                    self.lp.on = false;
                }
                if !self.playing {
                    // Stopped: it plays from the cue while held (CDJ).
                    self.playhead = to.clamp(0.0, self.frames());
                    self.unready();
                    self.preview(to, clock, xf);
                    return;
                }
                match self.grid().filter(|_| quantise) {
                    // On the next beat of what plays, so the beat stays where it was.
                    Some(g) => {
                        let at = g.pos_at(g.beat_at(p).ceil());
                        self.pending = Some((at, to));
                        self.shadowing = false;
                    }
                    None => self.jump(to, xf),
                }
                self.relink(xf);
            }
        }
    }

    fn set_loop(&mut self, action: LoopAction, quantise: bool, xf: usize) {
        if self.loaded.is_none() {
            return;
        }
        let p = self.pos();
        match action {
            LoopAction::Beats(n) => {
                let Some(g) = self.grid() else { return };
                let n = if n.is_finite() { n.clamp(MIN_LOOP_BEATS, 512.0) } else { return };
                let b0 = if quantise { g.beat_at(p).round() } else { g.beat_at(p) };
                let (a, b) = (g.pos_at(b0), g.pos_at(b0 + n));
                self.lp = LoopSpan { a: Some(a), b: Some(b), on: true };
                self.wrap_into_loop();
            }
            LoopAction::In => {
                let a = self.snap(p, quantise);
                self.lp = LoopSpan { a: Some(a), b: self.lp.b.filter(|b| *b > a), on: false };
            }
            LoopAction::Out => {
                let Some(a) = self.lp.a else { return };
                let b = self.snap(p, quantise);
                if b > a + 1.0 {
                    self.lp = LoopSpan { a: Some(a), b: Some(b), on: true };
                    self.wrap_into_loop();
                }
            }
            LoopAction::Halve | LoopAction::Double => {
                let (Some(a), Some(b)) = (self.lp.a, self.lp.b) else { return };
                let k = if action == LoopAction::Halve { 0.5 } else { 2.0 };
                let nb = match self.grid() {
                    Some(g) => {
                        let len = (g.beat_at(b) - g.beat_at(a)) * k;
                        (len >= MIN_LOOP_BEATS - 1e-9).then(|| g.pos_at(g.beat_at(a) + len))
                    }
                    None => Some(a + (b - a) * k),
                };
                if let Some(nb) = nb.filter(|nb| *nb > a + 1.0) {
                    self.lp.b = Some(nb);
                    if self.lp.on {
                        self.wrap_into_loop();
                    }
                }
            }
            LoopAction::Exit => self.lp.on = false,
            LoopAction::Reloop => {
                let (Some(a), Some(b)) = (self.lp.a, self.lp.b) else { return };
                if b > a {
                    self.lp.on = true;
                    self.jump(a, xf);
                }
            }
        }
        self.relink(xf);
    }

    /// A loop just set or shortened with a stopped playhead past its end: back inside it,
    /// keeping the phase in the loop. (While playing, the next block wraps it, crossfaded: see
    /// [`Deck::next_event`].)
    fn wrap_into_loop(&mut self) {
        let (Some(a), Some(b)) = (self.lp.a, self.lp.b) else { return };
        let p = self.pos();
        if !self.playing && p >= b && b > a {
            self.playhead = a + (p - a).rem_euclid(b - a);
            self.unready();
        }
    }

    fn beat_jump(&mut self, n: f64, xf: usize) {
        if self.loaded.is_none() || !n.is_finite() {
            return;
        }
        let p = self.pos();
        let target = match self.grid() {
            Some(_) => self.shifted(p, n),
            None => p + n * self.loaded.as_ref().map_or(0.0, |l| f64::from(l.track.source.buffer.sample_rate)) * JOG_SEARCH_S,
        };
        let delta = target - p;
        let moves_loop = self.lp.on;
        if moves_loop && let (Some(a), Some(b)) = (self.lp.a, self.lp.b) {
            // An active loop moves with it.
            self.lp.a = Some(a + delta);
            self.lp.b = Some(b + delta);
        }
        self.jump(target, xf);
        if moves_loop {
            self.relink(xf);
        }
    }

    fn jog(&mut self, delta: f64, mode: JogMode, quantise: bool, sr: f64, xf: usize) {
        if self.loaded.is_none() || !delta.is_finite() {
            return;
        }
        let delta = delta.clamp(-1.0, 1.0);
        match mode {
            JogMode::Nudge if self.playing => {
                self.bend = delta * JOG_BEND;
                self.bend_left = (JOG_HOLD_S * sr) as usize;
                if self.synced {
                    self.nudged = true;
                }
            }
            _ => {
                let p = self.pos();
                let target = match self.grid() {
                    Some(_) => self.shifted(p, delta),
                    None => p + delta * JOG_SEARCH_S * sr,
                };
                let target = self.phase_kept(target, quantise);
                self.jump(target, xf);
            }
        }
    }

    fn master_tempo(&mut self, on: bool, xf: usize) {
        if self.keylock == on {
            return;
        }
        self.keylock = on;
        if self.playing {
            // The other kind of head takes over where it is, crossfaded.
            let p = self.pos();
            if self.busy() {
                self.now = Some(p);
            } else {
                self.fire(0.0, xf);
            }
        } else {
            self.unready();
        }
    }

    fn state(&self, sr: f64) -> DeckState {
        let p = self.pos();
        let bpm = self.grid().map_or(0.0, |g| 60.0 * sr / g.period_at(p) * self.rate);
        DeckState {
            load: self.loaded.as_ref().map_or(0, |l| l.load),
            playing: self.playing,
            previewing: self.previewing,
            position: p,
            rate: self.rate,
            pitch: self.pitch,
            range: self.range,
            bpm,
            key_lock: self.keylock,
            synced: self.synced,
            master: self.master,
            phase_error: self.phase_err,
            cue: self.cue,
            hot: self.hot,
            hot_seq: self.hot_seq,
            loop_in: self.lp.a,
            loop_out: self.lp.b,
            loop_on: self.lp.on,
            ..DeckState::default()
        }
    }

    /// Every buffer the deck's tracks own (lengths and capacities).
    fn storage(&self, v: &mut Vec<(usize, usize)>) {
        for d in self.loaded.iter().chain(self.next.iter()) {
            d.storage(v);
        }
    }
}

/// What sync needs to know of a deck at the start of a block.
#[derive(Debug, Clone, Copy, Default)]
struct Snap {
    grid: bool,
    playing: bool,
    synced: bool,
    started: u64,
    beat: f64,
    period: f64,
    base: f64,
    /// Beats of the loop that plays, while one is on.
    loop_beats: Option<f64>,
}

/// The two decks and the DJ mixer.
pub struct Decks {
    sr: f64,
    max_block: usize,
    xf: usize,
    decks: [Deck; DECKS],
    strips: [DjStrip; DECKS],
    bufs: [Vec<Vec<f32>>; DECKS],
    mix: Vec<Vec<f32>>,
    xfader: f32,
    xcurve: XfadeCurve,
    /// Each deck's crossfader gain as of the end of the last block.
    xg: [f32; DECKS],
    master_db: f32,
    g_master: f32,
    limiter: Option<Box<dyn Plugin>>,
    /// The output stage's limiter, after the master faders ([`Decks::limit_output`]).
    out_limiter: Option<Box<dyn Plugin>>,
    /// The output stage is limiting the main output, or letting go of it (see
    /// [`Decks::limit_output`]); frames it has run on silence since the decks fell silent.
    out_active: bool,
    out_drain: usize,
    /// This block's decks mix was kept out of the main bus, for the output stage to play through
    /// its handover.
    held: bool,
    quantise: bool,
    /// Counts plays, so the deck that started first is known.
    clock: u64,
    /// Frames left to render after the decks fell silent.
    quiet: usize,
    levels: [[f32; 2]; DECKS],
    master_level: [f32; 2],
}

/// Adds the first `n` frames of `src` into `main`, channel for channel (non-finite as silence).
fn add_into(main: &mut [Vec<f32>], src: &[Vec<f32>], n: usize) {
    for (dst, s) in main.iter_mut().zip(src.iter()).take(2) {
        for (d, v) in dst.iter_mut().zip(s.iter()).take(n) {
            *d += if v.is_finite() { *v } else { 0.0 };
        }
    }
}

/// A Maximizer at the decks' ceiling, true peak (allocates).
fn ceiling_limiter(sr: f32, max_block: usize) -> Option<Box<dyn Plugin>> {
    soundcraft_dsp::create("maximizer").map(|mut m| {
        m.set_param("threshold", 0.0);
        m.set_param("ceiling", CEILING_DB);
        m.set_param("true_peak", 1.0);
        m.prepare(sr, max_block, 2);
        m
    })
}

impl Decks {
    /// The decks and mixer for blocks of up to `max_block` frames at `sample_rate` (allocates).
    pub fn new(sample_rate: f32, max_block: usize) -> Decks {
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 { sample_rate } else { 48_000.0 };
        let max_block = max_block.clamp(16, 16_384);
        let buf = || vec![vec![0.0f32; max_block]; 2];
        Decks {
            sr: f64::from(sr),
            max_block,
            xf: ((XFADE_S * f64::from(sr)).round() as usize).clamp(1, MAX_XF),
            decks: [Deck::new(), Deck::new()],
            strips: [DjStrip::new(sr, XfadeSide::A), DjStrip::new(sr, XfadeSide::B)],
            bufs: [buf(), buf()],
            mix: buf(),
            xfader: 0.0,
            xcurve: XfadeCurve::Smooth,
            xg: [std::f32::consts::FRAC_1_SQRT_2; DECKS],
            master_db: 0.0,
            g_master: 1.0,
            limiter: ceiling_limiter(sr, max_block),
            out_limiter: ceiling_limiter(sr, max_block),
            out_active: false,
            out_drain: 0,
            held: false,
            quantise: true,
            clock: 0,
            quiet: 0,
            levels: [[0.0; 2]; DECKS],
            master_level: [0.0; 2],
        }
    }

    /// Puts `deck` on deck `id`, stopped at its first downbeat. While the deck sounds, the new
    /// track waits: the playing one fades out first ([`XFADE_S`]) and is then swapped. Returns
    /// what is to be dropped off the audio thread now (a track that was waiting, replaced);
    /// tracks taken off later come from [`Decks::take_retired`].
    pub fn load(&mut self, id: DeckId, deck: Box<PreparedDeck>) -> Option<Box<PreparedDeck>> {
        let xf = self.xf;
        let Some(d) = self.decks.get_mut(id.index()) else { return Some(deck) };
        if d.sounding() || d.retired.is_some() {
            d.pause(xf);
            return d.next.replace(deck);
        }
        d.load(deck)
    }

    /// A track taken off a deck since last asked, for the host to drop off the audio thread.
    pub fn take_retired(&mut self) -> Option<Box<PreparedDeck>> {
        self.decks.iter_mut().find_map(|d| d.retired.take())
    }

    /// Every track the decks hold (loaded, waiting and retired), taken out: the engine is going
    /// away and they are to be dropped off the audio thread.
    pub fn drain(&mut self) -> impl Iterator<Item = Box<PreparedDeck>> + '_ {
        self.decks.iter_mut().flat_map(|d| [d.loaded.take(), d.next.take(), d.retired.take()]).flatten()
    }

    /// Sets the decks' rates from their faders and sync without rendering: what a callback with
    /// nothing to hear does, so a stopped deck shows the tempo it will play at.
    pub fn settle(&mut self) {
        self.update(0);
    }

    /// True while a deck plays or its sound rings out.
    pub fn awake(&self) -> bool {
        self.quiet > 0 || self.decks.iter().any(Deck::sounding)
    }

    /// True while a deck plays.
    pub fn playing(&self) -> bool {
        self.decks.iter().any(|d| d.playing)
    }

    fn deck(&mut self, id: DeckId) -> Option<&mut Deck> {
        self.decks.get_mut(id.index())
    }

    fn strip(&mut self, id: DeckId) -> Option<&mut DjStrip> {
        self.strips.get_mut(id.index())
    }

    /// The phase (beats, wrapped to ±½) deck `x` is ahead of its master by, when it follows
    /// one (and, with `playing`, only while the master plays).
    fn phase_to_master(&self, x: usize, playing: bool) -> Option<f64> {
        let (sx, sy) = (self.snap(x), self.snap(1 - x));
        let follows = sx.synced && sx.grid && sy.grid && (!sy.synced || sy.playing);
        if !follows || (playing && !sy.playing) {
            return None;
        }
        let d = sx.beat - sy.beat;
        Some(d - d.round())
    }

    /// Before deck `x` starts while its master plays: its playhead moves by its phase error, so
    /// it starts in phase (a stopped deck's move is inaudible).
    fn align_stopped(&mut self, x: usize, playing_master: bool) {
        let Some(e) = self.phase_to_master(x, playing_master) else { return };
        if let Some(d) = self.decks.get_mut(x)
            && !d.playing
        {
            d.playhead = d.shifted(d.playhead, -e).clamp(0.0, d.frames());
            d.sync_offset = 0.0;
            d.nudged = false;
            d.unready();
        }
    }

    /// Applies a command (no allocation: call it on the audio thread).
    pub fn apply(&mut self, cmd: DeckCmd) {
        let (q, xf, sr) = (self.quantise, self.xf, self.sr);
        if matches!(cmd, DeckCmd::Play(_) | DeckCmd::Cue { down: true, .. } | DeckCmd::HotCue { action: HotCueAction::Jump, .. }) {
            self.clock += 1;
        }
        let clock = self.clock;
        match cmd {
            DeckCmd::Play(id) => {
                self.align_stopped(id.index(), true);
                if let Some(d) = self.deck(id) {
                    d.play(clock, xf);
                }
            }
            DeckCmd::Pause(id) => {
                if let Some(d) = self.deck(id) {
                    d.pause(xf);
                }
            }
            DeckCmd::Cue { deck, down } => {
                let Some(d) = self.deck(deck).filter(|d| d.loaded.is_some()) else { return };
                if !down {
                    d.release(xf);
                } else if d.playing && !d.previewing {
                    d.pause(xf);
                    d.playhead = d.cue;
                } else if !d.playing {
                    // A preview plays the real cue spot (aligning is for Play).
                    d.set_cue_here(q);
                    let from = d.cue;
                    d.preview(from, clock, xf);
                }
            }
            DeckCmd::Seek { deck, at } => {
                if let Some(d) = self.deck(deck).filter(|_| at.is_finite()) {
                    if let (Some(a), Some(b)) = (d.lp.a, d.lp.b)
                        && (at < a || at >= b)
                    {
                        d.lp.on = false;
                    }
                    d.pending = None;
                    let at = d.phase_kept(at, q);
                    d.jump(at, xf);
                    d.relink(xf);
                }
            }
            DeckCmd::Jog { deck, delta, mode } => {
                if let Some(d) = self.deck(deck) {
                    d.jog(delta, mode, q, sr, xf);
                }
            }
            DeckCmd::Pitch { deck, value, range, takeover } => {
                // The deck the other follows sets the tempo with its fader (the follower keeps up);
                // moving a following deck's fader turns its sync off, and the fader takes over
                // once it reaches the tempo held (soft takeover).
                let other_leads = self.decks.get(1 - deck.index()).is_some_and(|o| o.master);
                if let Some(d) = self.deck(deck) {
                    if let Some(r) = range.filter(|r| PITCH_RANGES.iter().any(|x| (x - r).abs() < 1e-9)) {
                        d.range = r;
                    }
                    if value.is_finite() && d.synced && other_leads {
                        d.takeover = Some((d.pitch, d.fader_at.map(|f| f > d.pitch)));
                        d.synced = false;
                        d.correction = 0.0;
                        d.phase_err = None;
                    }
                    if value.is_finite() {
                        d.fader_at = Some(value.clamp(-d.range, d.range));
                        if !takeover {
                            d.takeover = None;
                        }
                    }
                    if value.is_finite() && !(d.synced && other_leads) {
                        let v = value.clamp(-d.range, d.range);
                        match d.takeover {
                            None => d.pitch = v,
                            Some((held, side)) => {
                                let now = v > held;
                                if (v - held).abs() <= TAKEOVER || side.is_some_and(|s| s != now) {
                                    d.pitch = v;
                                    d.takeover = None;
                                } else {
                                    d.takeover = Some((held, Some(now)));
                                }
                            }
                        }
                    }
                }
            }
            DeckCmd::MasterTempo { deck, on } => {
                if let Some(d) = self.deck(deck) {
                    d.master_tempo(on, xf);
                }
            }
            DeckCmd::Sync { deck, on } => {
                let x = deck.index();
                let Some(d) = self.deck(deck) else { return };
                if !on {
                    if d.synced {
                        // Which side of the held tempo the fader was left on, so one move across it
                        // takes over.
                        d.takeover = Some((d.pitch, d.fader_at.map(|f| f > d.pitch)));
                    }
                    d.synced = false;
                    d.correction = 0.0;
                    d.phase_err = None;
                    return;
                }
                d.synced = true;
                d.sync_offset = 0.0;
                d.last_err = None;
                d.nudged = false;
                d.takeover = None;
                let playing = d.playing;
                if !playing {
                    self.align_stopped(x, false);
                } else if let Some(e) = self.phase_to_master(x, true).filter(|e| e.abs() > SYNC_SNAP)
                    && let Some(d) = self.deck(deck)
                {
                    // Far out: one crossfaded jump into phase rather than a long bend.
                    let p = d.pos();
                    let target = d.shifted(p, -e);
                    d.jump(target, xf);
                }
            }
            DeckCmd::HotCue { deck, n, action, seq } => {
                if let Some(d) = self.deck(deck) {
                    d.hot_cue(n, action, seq, q, clock, xf);
                }
            }
            DeckCmd::Loop { deck, action } => {
                if let Some(d) = self.deck(deck) {
                    d.set_loop(action, q, xf);
                }
            }
            DeckCmd::BeatJump { deck, beats } => {
                if let Some(d) = self.deck(deck) {
                    d.beat_jump(beats, xf);
                }
            }
            DeckCmd::Eq { deck, band, db } => {
                if !db.is_nan()
                    && let Some(e) = self.strip(deck).and_then(|s| s.eq_db.get_mut(band.index()))
                {
                    *e = db.clamp(crate::dj_strip::EQ_KILL_DB, crate::dj_strip::EQ_MAX_DB);
                }
            }
            DeckCmd::Filter { deck, value } => {
                if let Some(s) = self.strip(deck).filter(|_| value.is_finite()) {
                    s.filter = value.clamp(-1.0, 1.0);
                }
            }
            DeckCmd::Trim { deck, db } => {
                if let Some(s) = self.strip(deck).filter(|_| db.is_finite()) {
                    s.trim_db = db.clamp(crate::dj_strip::TRIM_DB.0, crate::dj_strip::TRIM_DB.1);
                }
            }
            DeckCmd::Fader { deck, value, curve } => {
                if let Some(s) = self.strip(deck) {
                    if value.is_finite() {
                        s.fader = value.clamp(0.0, 1.0);
                    }
                    if let Some(c) = curve {
                        s.curve = c;
                    }
                }
            }
            DeckCmd::Crossfader { value, curve } => {
                if value.is_finite() {
                    self.xfader = value.clamp(-1.0, 1.0);
                }
                if let Some(c) = curve {
                    self.xcurve = c;
                }
            }
            DeckCmd::Assign { deck, side } => {
                if let Some(s) = self.strip(deck) {
                    s.side = side;
                }
            }
            DeckCmd::Master { db } => {
                if db.is_finite() {
                    self.master_db = db.clamp(-60.0, 0.0);
                }
            }
            DeckCmd::Quantise(on) => self.quantise = on,
            DeckCmd::StopAll => {
                for d in self.decks.iter_mut() {
                    d.pause(xf);
                }
            }
        }
    }

    fn snap(&self, i: usize) -> Snap {
        let Some(d) = self.decks.get(i) else { return Snap::default() };
        let p = d.pos();
        let base = 1.0 + d.pitch;
        match d.grid() {
            Some(g) => Snap {
                grid: true,
                playing: d.playing,
                synced: d.synced,
                started: d.started,
                beat: g.beat_at(p),
                period: g.period_at(p),
                base,
                loop_beats: d.loop_beats(),
            },
            None => Snap { playing: d.playing, synced: d.synced, started: d.started, base, ..Snap::default() },
        }
    }

    /// Sets each deck's rate for the next `n` frames: pitch, sync (tempo, then a gentle phase
    /// trim) and jog.
    fn update(&mut self, n: usize) {
        let snaps = [self.snap(0), self.snap(1)];
        let mut masters = [false; DECKS];
        let xf = self.xf;
        for x in 0..DECKS {
            let (Some(sx), Some(sy)) = (snaps.get(x), snaps.get(1 - x)) else { continue };
            let Some(d) = self.decks.get_mut(x) else { continue };
            if d.bend_left > 0 {
                d.bend_left = d.bend_left.saturating_sub(n);
            }
            let nudging = d.bend_left > 0;
            if !nudging {
                d.bend = 0.0;
            }
            d.correction = 0.0;
            d.phase_err = None;
            let follows = sx.synced && sx.grid && sy.grid && (!sy.synced || (sy.playing && (!sx.playing || sy.started < sx.started)));
            if follows {
                let base = sy.base * sx.period / sy.period.max(1.0);
                if base.is_finite() {
                    d.pitch = (base - 1.0).clamp(-SYNC_PITCH, SYNC_PITCH);
                }
                // Loops shorter than a beat repeat a fraction of a beat: their phase is not the
                // beat's, so only the tempo follows.
                let short = |s: &Snap| s.loop_beats.is_some_and(|b| b < 1.0);
                if sx.playing && sy.playing && !short(sx) && !short(sy) {
                    let raw = sx.beat - sy.beat;
                    let raw = raw - raw.round();
                    if nudging {
                        // The DJ rides it: no correction, and the phase it ends at is kept.
                        d.nudged = true;
                        d.last_err = None;
                    } else {
                        if std::mem::take(&mut d.nudged) {
                            d.sync_offset = raw;
                        }
                        let e = raw - d.sync_offset;
                        let e = e - e.round();
                        d.phase_err = Some(e);
                        // A jump in the error (a seek, the master just started) is put right at once,
                        // crossfaded, as syncing does; the trim only holds what is already close.
                        let jumped = d.last_err.is_none_or(|l| {
                            let dl = e - l;
                            (dl - dl.round()).abs() > SYNC_JUMP
                        });
                        if d.busy() {
                        } else if jumped && e.abs() > SYNC_SNAP {
                            let target = d.shifted(d.pos(), -e);
                            d.jump(target, xf);
                            d.last_err = Some(0.0);
                        } else {
                            d.correction = (-SYNC_GAIN * e).clamp(-SYNC_MAX, SYNC_MAX);
                            d.last_err = Some(e);
                        }
                    }
                } else {
                    d.last_err = None;
                }
                if let Some(m) = masters.get_mut(1 - x) {
                    *m = true;
                }
            } else {
                d.last_err = None;
            }
            let r = (1.0 + d.pitch) * (1.0 + d.correction) * (1.0 + d.bend);
            d.rate = if r.is_finite() { r.clamp(RATE_LIMITS.0, RATE_LIMITS.1) } else { 1.0 };
        }
        for (d, m) in self.decks.iter_mut().zip(masters) {
            d.master = m;
        }
    }

    /// Adds the decks' mix for the next `frames` frames into `main` (L and R; its first two
    /// channels), the arrangement stopped.
    pub fn render(&mut self, main: &mut [Vec<f32>], frames: usize) {
        self.render_with(main, frames, false);
    }

    /// [`Decks::render`], with the arrangement playing or not. While the output stage lets go of
    /// the output (the arrangement just started), the decks' tail plays through it instead of
    /// into the main bus ([`Decks::limit_output`]), so it stays continuous.
    pub fn render_with(&mut self, main: &mut [Vec<f32>], frames: usize, arrangement: bool) {
        let n = frames.min(self.max_block);
        self.held = false;
        if n == 0 || !(self.awake() || self.decks.iter().any(|d| d.next.is_some())) {
            // Nothing to hear, but the rates still follow the faders and sync, so a stopped deck
            // shows the tempo it will play at.
            self.update(n);
            return;
        }
        self.update(n);
        let xf = self.xf;
        let gains = self.xcurve.gains(self.xfader);
        let gm = db_to_gain(self.master_db);
        for c in self.mix.iter_mut() {
            c.iter_mut().take(n).for_each(|x| *x = 0.0);
        }
        for (k, ((deck, strip), buf)) in self.decks.iter_mut().zip(self.strips.iter_mut()).zip(self.bufs.iter_mut()).enumerate() {
            for c in buf.iter_mut() {
                c.iter_mut().take(n).for_each(|x| *x = 0.0);
            }
            deck.render(buf, n, xf);
            strip.process(buf, n);
            let peaks: [f32; 2] = [0, 1].map(|c| buf.get(c).map_or(0.0, |b| b.iter().take(n).fold(0.0f32, |m, v| m.max(v.abs()))));
            if let Some(l) = self.levels.get_mut(k) {
                *l = peaks;
            }
            let (g0, g1) = (self.xg.get(k).copied().unwrap_or(1.0), strip.side.pick(gains));
            let (m0, m1) = (self.g_master, gm);
            for (dst, src) in self.mix.iter_mut().zip(buf.iter()) {
                for (i, (d, s)) in dst.iter_mut().zip(src.iter()).take(n).enumerate() {
                    let t = (i + 1) as f32 / n as f32;
                    *d += s * (g0 + (g1 - g0) * t) * (m0 + (m1 - m0) * t);
                }
            }
            if let Some(g) = self.xg.get_mut(k) {
                *g = g1;
            }
        }
        self.g_master = gm;
        if let Some(l) = self.limiter.as_mut() {
            l.process(&mut self.mix, n);
        }
        self.master_level = [0, 1].map(|c| self.mix.get(c).map_or(0.0, |b| b.iter().take(n).fold(0.0f32, |m, v| m.max(v.abs()))));
        if self.out_active && arrangement {
            self.held = true;
        } else {
            add_into(main, &self.mix, n);
        }
        if self.decks.iter().any(Deck::sounding) {
            self.quiet = (QUIET_S * self.sr) as usize;
        } else {
            self.quiet = self.quiet.saturating_sub(n);
            if self.quiet == 0 {
                // Silent: the next start rings nothing of this one.
                self.strips.iter_mut().for_each(DjStrip::reset);
                if let Some(l) = self.limiter.as_mut() {
                    l.reset();
                }
                self.levels = [[0.0; 2]; DECKS];
                self.master_level = [0.0; 2];
            }
        }
    }

    /// The output stage: while the decks sound with the arrangement stopped, the main output
    /// (after the master faders) goes through a second Maximizer at the ceiling, so no master
    /// fader or insert takes the decks over it.
    ///
    /// It lets go without a click: once the arrangement plays (or the decks have rung out) it
    /// runs on, on the decks' own tail only (or silence), adding what it still holds to the main
    /// output, until its delay has played out; then it is reset and the output passes untouched.
    pub fn limit_output(&mut self, main: &mut [Vec<f32>], frames: usize, arrangement: bool) {
        let n = frames.min(self.max_block);
        let (awake, sounding) = (self.awake(), self.decks.iter().any(Deck::sounding));
        let Some(l) = self.out_limiter.as_mut().filter(|_| n > 0) else { return };
        if !arrangement && awake {
            let stereo = main.len().min(2);
            if let Some(io) = main.get_mut(..stereo) {
                l.process(io, n);
            }
            self.out_active = true;
            self.out_drain = 0;
            return;
        }
        if !self.out_active {
            return;
        }
        if !std::mem::take(&mut self.held) {
            for c in self.mix.iter_mut() {
                c.iter_mut().take(n).for_each(|x| *x = 0.0);
            }
        }
        l.process(&mut self.mix, n);
        add_into(main, &self.mix, n);
        if sounding {
            self.out_drain = 0;
        } else {
            self.out_drain += n;
        }
        if self.out_drain > l.latency() + n {
            l.reset();
            self.out_active = false;
        }
    }

    /// Frames the decks' stretchers produced since last asked, per deck.
    pub fn take_stretched(&mut self) -> [usize; DECKS] {
        let mut out = [0; DECKS];
        for (o, d) in out.iter_mut().zip(self.decks.iter_mut()) {
            if let Some(l) = d.loaded.as_mut() {
                *o = l.heads.iter_mut().map(Head::take_stretched).sum();
            }
        }
        out
    }

    /// What the decks play, for publishing.
    pub fn state(&self) -> DecksState {
        let mut out = DecksState {
            xfader: self.xfader,
            xfader_curve: self.xcurve,
            master_db: self.master_db,
            master_level: self.master_level,
            quantise: self.quantise,
            ..DecksState::default()
        };
        for (k, o) in out.decks.iter_mut().enumerate() {
            let (Some(d), Some(s)) = (self.decks.get(k), self.strips.get(k)) else { continue };
            *o = DeckState {
                level: self.levels.get(k).copied().unwrap_or_default(),
                trim_db: s.trim_db,
                eq_db: s.eq_db,
                filter: s.filter,
                fader: s.fader,
                fader_curve: s.curve,
                side: s.side,
                ..d.state(self.sr)
            };
        }
        out
    }

    /// Lengths and capacities of the mixer's own buffers (they never change once built).
    #[doc(hidden)]
    pub fn storage(&self) -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        for b in self.bufs.iter().chain(std::iter::once(&self.mix)) {
            v.push((b.len(), b.capacity()));
            v.extend(b.iter().map(|c| (c.len(), c.capacity())));
        }
        v
    }

    /// Lengths and capacities of the buffers of the tracks on (and waiting for) each deck.
    #[doc(hidden)]
    pub fn track_storage(&self) -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        for d in &self.decks {
            d.storage(&mut v);
        }
        v
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use soundcraft_audio_io::AudioBuffer;

    const SR: f64 = 48_000.0;
    const BLOCK: usize = 512;

    /// A stereo track of `secs` from `f(i)`, with a constant grid at `bpm` from `first` samples.
    fn track(secs: f64, bpm: f64, first: f64, f: impl Fn(usize) -> f32) -> Arc<DeckTrack> {
        let n = (secs * SR) as usize;
        let ch: Vec<f32> = (0..n).map(&f).collect();
        let source = Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR as u32, channels: vec![ch.clone(), ch] }));
        let grid = BeatGrid::constant(first, 60.0 * SR / bpm, n as f64);
        Arc::new(DeckTrack {
            source,
            grid,
            bpm: Some(bpm),
            key: None,
            cues: [None; HOT_CUES],
            title: String::new(),
            artist: String::new(),
            path: String::new(),
            grid_from: Some("given"),
            sections: Vec::new(),
        })
    }

    /// Clicks: 1.0 on every beat's sample (beats at whole samples), else 0.
    fn clicks(bpm: f64, first: f64) -> impl Fn(usize) -> f32 {
        let period = 60.0 * SR / bpm;
        move |i| {
            let b = (i as f64 - first) / period;
            if b >= 0.0 && ((b - b.round()).abs() * period) < 0.5 { 1.0 } else { 0.0 }
        }
    }

    /// Bursts: 5 ms of 1 kHz from every beat, silence between.
    fn bursts(bpm: f64, first: f64) -> impl Fn(usize) -> f32 {
        let period = 60.0 * SR / bpm;
        move |i| {
            let t = i as f64 - first;
            let into = t - (t / period).floor() * period;
            if t >= 0.0 && into < 0.005 * SR { (std::f64::consts::TAU * 1000.0 * into / SR).sin() as f32 * 0.8 } else { 0.0 }
        }
    }

    fn decks() -> Decks {
        Decks::new(SR as f32, BLOCK)
    }

    fn prepared(t: &Arc<DeckTrack>) -> Box<PreparedDeck> {
        Box::new(PreparedDeck::new(Arc::clone(t), SR as f32))
    }

    fn load(d: &mut Decks, id: DeckId, t: &Arc<DeckTrack>) {
        assert!(d.load(id, prepared(t)).is_none());
    }

    /// The raw outputs (left) of both decks for `blocks` blocks (rates set as the mixer would).
    fn raw2(d: &mut Decks, blocks: usize) -> [Vec<f32>; 2] {
        let mut out = [Vec::new(), Vec::new()];
        for _ in 0..blocks {
            d.update(BLOCK);
            for (k, o) in out.iter_mut().enumerate() {
                let mut buf = vec![vec![0.0f32; BLOCK]; 2];
                d.decks[k].render(&mut buf, BLOCK, d.xf);
                o.extend_from_slice(&buf[0]);
            }
        }
        out
    }

    fn raw(d: &mut Decks, k: usize, blocks: usize) -> Vec<f32> {
        let [a, b] = raw2(d, blocks);
        if k == 0 { a } else { b }
    }

    /// Where each burst starts: the first sample over 0.2 after 10 ms under 0.05.
    fn onsets(x: &[f32]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut quiet = 0usize;
        for (i, v) in x.iter().enumerate() {
            if v.abs() > 0.2 {
                if quiet >= 480 {
                    out.push(i);
                }
                quiet = 0;
            } else if v.abs() < 0.05 {
                quiet += 1;
            }
        }
        out
    }

    #[test]
    fn grids_map_beats_and_samples_both_ways() {
        let g = BeatGrid::constant(1000.0, 22_500.0, 1_000_000.0).unwrap();
        assert_eq!(g.beat_at(1000.0), 0.0);
        assert_eq!(g.beat_at(1000.0 + 22_500.0 * 2.5), 2.5);
        assert_eq!(g.beat_at(0.0), -1000.0 / 22_500.0);
        assert_eq!(g.pos_at(3.0), 1000.0 + 3.0 * 22_500.0);
        assert_eq!(g.pos_at(-1.0), 1000.0 - 22_500.0);
        let last = g.beats().len() as f64;
        assert!((g.beat_at(g.pos_at(last + 10.5)) - (last + 10.5)).abs() < 1e-9, "extrapolated past the end");
        assert!(BeatGrid::new(vec![1.0]).is_none() && BeatGrid::new(vec![2.0, 1.0]).is_none() && BeatGrid::new(vec![0.0, f64::NAN]).is_none());
        let v = BeatGrid::new(vec![0.0, 100.0, 300.0]).unwrap();
        assert_eq!((v.beat_at(200.0), v.period_at(200.0), v.pos_at(1.5)), (1.5, 200.0, 200.0));
        assert_eq!(hermite(&[1.0, 2.0], f64::from(i32::MAX) * 1e10), 0.0, "far out of range is silence");
    }

    #[test]
    fn play_starts_on_the_exact_sample() {
        let t = track(4.0, 120.0, 0.0, |i| i as f32 / 1_000_000.0);
        let mut d = decks();
        load(&mut d, DeckId::A, &t);
        d.apply(DeckCmd::Seek { deck: DeckId::A, at: 12_345.0 });
        d.apply(DeckCmd::Play(DeckId::A));
        let out = raw(&mut d, 0, 2);
        for (k, v) in out.iter().enumerate() {
            assert_eq!(*v, (12_345 + k) as f32 / 1_000_000.0, "frame {k}");
        }
        assert_eq!(d.state().decks[0].position, 12_345.0 + 2.0 * BLOCK as f64);
    }

    #[test]
    fn cdj_cue_sets_previews_returns_and_pauses() {
        let t = track(8.0, 120.0, 0.0, |_| 0.5);
        let mut d = decks();
        load(&mut d, DeckId::A, &t);
        let a = DeckId::A;
        let st = |d: &Decks| d.state().decks[0];
        // Paused at 1.3 beats: cue down sets the cue on the nearest beat (quantise) and previews.
        d.apply(DeckCmd::Seek { deck: a, at: 31_000.0 });
        d.apply(DeckCmd::Cue { deck: a, down: true });
        assert_eq!((st(&d).cue, st(&d).playing, st(&d).previewing), (24_000.0, true, true));
        raw(&mut d, 0, 10);
        assert_eq!(st(&d).position, 24_000.0 + 10.0 * BLOCK as f64);
        // Release: back to the cue, paused.
        d.apply(DeckCmd::Cue { deck: a, down: false });
        assert_eq!((st(&d).position, st(&d).playing), (24_000.0, false));
        // Without quantise the cue is the playhead itself.
        d.apply(DeckCmd::Quantise(false));
        d.apply(DeckCmd::Seek { deck: a, at: 31_000.0 });
        d.apply(DeckCmd::Cue { deck: a, down: true });
        d.apply(DeckCmd::Cue { deck: a, down: false });
        assert_eq!((st(&d).cue, st(&d).position), (31_000.0, 31_000.0));
        // Playing: cue returns to the cue point and pauses; the release does nothing more.
        d.apply(DeckCmd::Play(a));
        raw(&mut d, 0, 20);
        d.apply(DeckCmd::Cue { deck: a, down: true });
        assert_eq!((st(&d).position, st(&d).playing), (31_000.0, false));
        d.apply(DeckCmd::Cue { deck: a, down: false });
        assert_eq!((st(&d).position, st(&d).playing), (31_000.0, false));
        // Play while previewing: it plays on after the release.
        d.apply(DeckCmd::Cue { deck: a, down: true });
        d.apply(DeckCmd::Play(a));
        d.apply(DeckCmd::Cue { deck: a, down: false });
        assert!(st(&d).playing && !st(&d).previewing);
        raw(&mut d, 0, 2);
        // The pause fades out over 2 ms, never stepping, then is silent.
        d.apply(DeckCmd::Pause(a));
        let tail = raw(&mut d, 0, 1);
        assert!((tail[0] - 0.5).abs() < 0.01, "starts from where it was");
        assert!(tail.windows(2).take(d.xf).all(|w| w[1] <= w[0] && w[0] - w[1] < 0.05), "a smooth fade");
        assert!(tail.iter().skip(d.xf).all(|v| *v == 0.0), "silent after the fade");
    }

    #[test]
    fn a_pause_in_a_jumps_crossfade_fades_both_heads() {
        // Two levels a jump crossfades between: 0.5 before 100 000, 0.25 after.
        let t = track(8.0, 120.0, 0.0, |i| if i < 100_000 { 0.5 } else { 0.25 });
        let mut d = decks();
        load(&mut d, DeckId::A, &t);
        d.apply(DeckCmd::Play(DeckId::A));
        raw(&mut d, 0, 1);
        d.apply(DeckCmd::Seek { deck: DeckId::A, at: 200_000.0 });
        let first = raw(&mut d, 0, 1);
        assert!(first[0] > 0.4 && first[200] == 0.25, "the crossfade");
        d.apply(DeckCmd::Seek { deck: DeckId::A, at: 10_000.0 });
        let x = raw(&mut d, 0, 1);
        // Mid-crossfade back, then paused right away: nothing cuts.
        d.apply(DeckCmd::Seek { deck: DeckId::A, at: 300_000.0 });
        d.decks[0].render(&mut vec![vec![0.0; 40]; 2], 40, d.xf);
        d.apply(DeckCmd::Pause(DeckId::A));
        let tail = raw(&mut d, 0, 1);
        assert!(x[300] == 0.5);
        assert!(tail[0] > 0.2, "{}", tail[0]);
        assert!(tail.windows(2).all(|w| (w[1] - w[0]).abs() < 0.05), "no step");
        assert!(tail.iter().skip(d.xf).all(|v| *v == 0.0));
    }

    #[test]
    fn a_quantised_hot_cue_jump_lands_on_the_beat() {
        // Clicks on every beat (beat 22 500 samples at 128 bpm), first beat at 0.
        let t = track(20.0, 128.0, 0.0, clicks(128.0, 0.0));
        let mut d = decks();
        load(&mut d, DeckId::A, &t);
        let a = DeckId::A;
        // Hot cue 3 on beat 8, set from a little off it (snapped).
        d.apply(DeckCmd::Seek { deck: a, at: 8.0 * 22_500.0 + 700.0 });
        d.apply(DeckCmd::HotCue { deck: a, n: 3, action: HotCueAction::Set, seq: 0 });
        assert_eq!(d.state().decks[0].hot[3], Some(180_000.0));
        // Playing from 1.4 beats, jump: it lands on the next beat (beat 2's moment).
        d.apply(DeckCmd::Seek { deck: a, at: 1.4 * 22_500.0 });
        d.apply(DeckCmd::Play(a));
        d.apply(DeckCmd::HotCue { deck: a, n: 3, action: HotCueAction::Jump, seq: 0 });
        let out = raw(&mut d, 0, 120);
        let first_click = out.iter().position(|v| *v > 0.5).unwrap();
        // Beat 2 would have sounded 0.6 beats in; the cue's click (beat 8) sounds there instead.
        assert_eq!(first_click, (0.6 * 22_500.0) as usize);
        assert_eq!(out[first_click], 1.0, "whole, after the crossfade");
        let next = out.iter().skip(first_click + 1).position(|v| *v > 0.5).unwrap() + first_click + 1;
        assert_eq!(next - first_click, 22_500);
    }

    #[test]
    fn a_hot_cue_pressed_while_paused_previews_and_returns_on_release() {
        let t = track(10.0, 128.0, 0.0, |_| 0.3);
        let mut d = decks();
        load(&mut d, DeckId::A, &t);
        let a = DeckId::A;
        d.apply(DeckCmd::Seek { deck: a, at: 45_000.0 });
        d.apply(DeckCmd::HotCue { deck: a, n: 0, action: HotCueAction::Set, seq: 0 });
        d.apply(DeckCmd::Seek { deck: a, at: 200_000.0 });
        d.apply(DeckCmd::HotCue { deck: a, n: 0, action: HotCueAction::Jump, seq: 0 });
        let s = d.state().decks[0];
        assert!(s.playing && s.previewing && s.position == 45_000.0);
        raw(&mut d, 0, 4);
        d.apply(DeckCmd::HotCue { deck: a, n: 0, action: HotCueAction::Release, seq: 0 });
        let s = d.state().decks[0];
        assert!(!s.playing && s.position == 45_000.0);
    }

    /// Phase error (ms) of deck B against A, from their positions and grids.
    fn phase_ms(d: &Decks, ta: &DeckTrack, tb: &DeckTrack) -> f64 {
        let s = d.state();
        let (ga, gb) = (ta.grid.as_ref().unwrap(), tb.grid.as_ref().unwrap());
        let (ba, bb) = (ga.beat_at(s.decks[0].position), gb.beat_at(s.decks[1].position));
        let e = (bb - ba) - (bb - ba).round();
        e * ga.period_at(s.decks[0].position) / s.decks[0].rate / SR * 1000.0
    }

    fn sync_locks(pitch_a: f64, key_lock: bool) {
        let ta = track(120.0, 128.0, 0.0, bursts(128.0, 0.0));
        let tb = track(120.0, 132.0, 0.037 * SR, bursts(132.0, 0.037 * SR));
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        d.apply(DeckCmd::Pitch { deck: DeckId::A, value: pitch_a, range: Some(0.16), takeover: true });
        d.apply(DeckCmd::MasterTempo { deck: DeckId::B, on: key_lock });
        d.apply(DeckCmd::Play(DeckId::A));
        raw(&mut d, 0, 50);
        // B starts somewhere off the beat, then syncs: far out, so it jumps into phase.
        d.apply(DeckCmd::Seek { deck: DeckId::B, at: 3.37 * 48_000.0 });
        d.apply(DeckCmd::Play(DeckId::B));
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: true });
        let beat_frames = 60.0 / (128.0 * (1.0 + pitch_a)) * SR;
        let blocks_per_beat = beat_frames / BLOCK as f64;
        let (mut worst, mut worst_bend) = (0.0f64, 0.0f64);
        let blocks = (blocks_per_beat * 4.0 * 33.0) as usize;
        let mut audio = [Vec::new(), Vec::new()];
        for b in 0..blocks {
            let [oa, ob] = raw2(&mut d, 1);
            let s = d.state().decks[1];
            worst_bend = worst_bend.max((s.rate / (1.0 + s.pitch) - 1.0).abs());
            if b as f64 > blocks_per_beat * 4.0 {
                worst = worst.max(phase_ms(&d, &ta, &tb).abs());
                audio[0].extend(oa);
                audio[1].extend(ob);
            }
        }
        let s = d.state();
        assert!(s.decks[1].synced && s.decks[0].master && !s.decks[1].master);
        assert!(worst < 1.0, "phase error after 4 beats up to {worst} ms (pitch {pitch_a}, key lock {key_lock})");
        assert!(worst_bend <= SYNC_MAX + 1e-9, "the rate bent {worst_bend}: it should have jumped");
        // On the audio: every burst of B within a millisecond of one of A.
        let (oa, ob) = (onsets(&audio[0]), onsets(&audio[1]));
        assert!(ob.len() > 100, "{}", ob.len());
        let off = ob.iter().map(|b| oa.iter().map(|a| (*a as f64 - *b as f64).abs()).fold(f64::MAX, f64::min)).fold(0.0, f64::max);
        assert!(off < 48.0, "bursts {off} samples apart (key lock {key_lock})");
        // Tempo matched: B's bpm is A's.
        assert!((s.decks[1].bpm - s.decks[0].bpm).abs() < 0.01, "{} vs {}", s.decks[1].bpm, s.decks[0].bpm);
        // Sync off keeps the rate, through blocks; the fader takes over only once it reaches it.
        let rate = s.decks[1].rate;
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: false });
        d.apply(DeckCmd::Pitch { deck: DeckId::B, value: 0.06, range: Some(0.16), takeover: true });
        raw(&mut d, 1, 20);
        assert!((d.state().decks[1].rate - rate).abs() < 1e-3, "{} vs {rate}", d.state().decks[1].rate);
        let held = d.state().decks[1].pitch;
        d.apply(DeckCmd::Pitch { deck: DeckId::B, value: held - 0.01, range: None, takeover: true });
        raw(&mut d, 1, 1);
        assert!((d.state().decks[1].pitch - (held - 0.01)).abs() < 1e-9, "crossed the held tempo: taken over");
    }

    #[test]
    fn sync_locks_phase_within_a_millisecond_in_four_beats_and_holds_32_bars() {
        sync_locks(0.0, false);
    }

    #[test]
    fn sync_locks_with_the_master_pitched_8_percent_either_way() {
        sync_locks(0.08, false);
        sync_locks(-0.08, false);
    }

    #[test]
    fn sync_locks_a_key_locked_deck_on_the_audio() {
        sync_locks(0.0, true);
    }

    #[test]
    fn a_synced_deck_starts_in_phase_and_syncing_a_stopped_deck_aligns_it() {
        let ta = track(60.0, 128.0, 0.0, |_| 0.1);
        let tb = track(60.0, 126.0, 1234.0, |_| 0.1);
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        d.apply(DeckCmd::Play(DeckId::A));
        raw(&mut d, 0, 30);
        d.apply(DeckCmd::Seek { deck: DeckId::B, at: 101_234.0 });
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: true });
        assert!(phase_ms(&d, &ta, &tb).abs() < 0.2, "aligned while stopped: {}", phase_ms(&d, &ta, &tb));
        raw(&mut d, 0, 13);
        d.apply(DeckCmd::Play(DeckId::B));
        raw(&mut d, 0, 1);
        assert!(phase_ms(&d, &ta, &tb).abs() < 0.2, "starts in phase: {}", phase_ms(&d, &ta, &tb));
        let s = d.state().decks[1];
        assert!((s.rate / (1.0 + s.pitch) - 1.0).abs() < 1e-3, "no bend needed");
    }

    #[test]
    fn a_nudge_rides_a_follower_and_the_lock_holds_its_new_phase() {
        let ta = track(60.0, 128.0, 0.0, |_| 0.1);
        let tb = track(60.0, 128.0, 0.0, |_| 0.1);
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: true });
        d.apply(DeckCmd::Play(DeckId::A));
        d.apply(DeckCmd::Play(DeckId::B));
        raw(&mut d, 0, 20);
        assert!(phase_ms(&d, &ta, &tb).abs() < 0.1);
        for _ in 0..30 {
            d.apply(DeckCmd::Jog { deck: DeckId::B, delta: 0.5, mode: JogMode::Nudge });
            raw(&mut d, 0, 1);
            let s = d.state().decks[1];
            assert!((s.rate - 1.05).abs() < 1e-6, "the nudge bends it: {}", s.rate);
        }
        // The bend holds a moment after the last event.
        raw(&mut d, 0, 10);
        let ahead = phase_ms(&d, &ta, &tb);
        assert!(ahead > 10.0, "{ahead}");
        raw(&mut d, 0, 400);
        assert!((phase_ms(&d, &ta, &tb) - ahead).abs() < 0.5, "the lock keeps where the nudge left it");
        assert!((d.state().decks[1].rate - 1.0).abs() < 1e-3);
    }

    #[test]
    fn a_quantised_seek_while_playing_keeps_the_beat_fraction() {
        let t = track(60.0, 128.0, 0.0, |_| 0.1);
        let mut d = decks();
        load(&mut d, DeckId::A, &t);
        d.apply(DeckCmd::Seek { deck: DeckId::A, at: 2.3 * 22_500.0 });
        d.apply(DeckCmd::Play(DeckId::A));
        d.apply(DeckCmd::Seek { deck: DeckId::A, at: 10.7 * 22_500.0 });
        raw(&mut d, 0, 1);
        let b = d.state().decks[0].position / 22_500.0 - BLOCK as f64 / 22_500.0;
        assert!((b - 10.3).abs() < 1e-6, "lands on the nearest beat with the same fraction: {b}");
        // Jog search too; and without quantise a seek goes where it is told.
        d.apply(DeckCmd::Jog { deck: DeckId::A, delta: 0.5, mode: JogMode::Search });
        let before = d.state().decks[0].position / 22_500.0;
        raw(&mut d, 0, 1);
        let after = d.state().decks[0].position / 22_500.0 - BLOCK as f64 / 22_500.0;
        assert!(((after - before) - (after - before).round()).abs() < 1e-6, "{before} -> {after}");
        d.apply(DeckCmd::Quantise(false));
        d.apply(DeckCmd::Seek { deck: DeckId::A, at: 20.7 * 22_500.0 });
        raw(&mut d, 0, 1);
        assert!((d.state().decks[0].position - 20.7 * 22_500.0 - BLOCK as f64).abs() < 1e-6);
    }

    #[test]
    fn a_large_seek_on_a_playing_follower_is_back_in_phase_within_a_beat() {
        let ta = track(60.0, 128.0, 0.0, |_| 0.1);
        let tb = track(60.0, 126.0, 777.0, |_| 0.1);
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        d.apply(DeckCmd::Quantise(false));
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: true });
        d.apply(DeckCmd::Play(DeckId::A));
        d.apply(DeckCmd::Play(DeckId::B));
        raw(&mut d, 0, 40);
        assert!(phase_ms(&d, &ta, &tb).abs() < 1.0);
        d.apply(DeckCmd::Seek { deck: DeckId::B, at: 812_345.0 });
        let beat = (22_500.0 / BLOCK as f64) as usize;
        raw(&mut d, 0, beat);
        assert!(phase_ms(&d, &ta, &tb).abs() < 1.0, "{}", phase_ms(&d, &ta, &tb));
        let s = d.state().decks[1];
        assert!((s.rate / (1.0 + s.pitch) - 1.0).abs() <= SYNC_MAX + 1e-9);
    }

    #[test]
    fn a_master_starting_against_a_playing_follower_puts_them_in_phase() {
        let ta = track(60.0, 128.0, 0.0, |_| 0.1);
        let tb = track(60.0, 130.0, 1234.0, |_| 0.1);
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: true });
        d.apply(DeckCmd::Play(DeckId::B));
        raw(&mut d, 0, 30);
        d.apply(DeckCmd::Seek { deck: DeckId::A, at: 101_111.0 });
        d.apply(DeckCmd::Play(DeckId::A));
        let beat = (22_500.0 / BLOCK as f64) as usize;
        raw(&mut d, 0, beat);
        assert!(phase_ms(&d, &ta, &tb).abs() < 1.0, "{}", phase_ms(&d, &ta, &tb));
        assert!(d.state().decks[0].master);
    }

    #[test]
    fn one_fader_move_across_the_held_tempo_takes_over() {
        let ta = track(60.0, 128.0, 0.0, |_| 0.1);
        let tb = track(60.0, 125.0, 0.0, |_| 0.1);
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        // The fader sits at -2 %; synced, B runs +2.4 %; off, it holds +2.4 %.
        d.apply(DeckCmd::Pitch { deck: DeckId::B, value: -0.02, range: None, takeover: true });
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: true });
        raw(&mut d, 0, 1);
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: false });
        let held = d.state().decks[1].pitch;
        assert!((held - 0.024).abs() < 1e-3);
        // A move still below it does nothing; one jump past it takes over.
        d.apply(DeckCmd::Pitch { deck: DeckId::B, value: 0.01, range: None, takeover: true });
        assert_eq!(d.state().decks[1].pitch, held);
        d.apply(DeckCmd::Pitch { deck: DeckId::B, value: 0.05, range: None, takeover: true });
        assert_eq!(d.state().decks[1].pitch, 0.05);
    }

    #[test]
    fn the_master_s_fader_sets_the_tempo_and_a_follower_s_fader_turns_its_sync_off() {
        let ta = track(60.0, 128.0, 0.0, |_| 0.1);
        let tb = track(60.0, 125.0, 0.0, |_| 0.1);
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        // Both synced, A started first: A leads.
        d.apply(DeckCmd::Sync { deck: DeckId::A, on: true });
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: true });
        d.apply(DeckCmd::Play(DeckId::A));
        raw(&mut d, 0, 2);
        d.apply(DeckCmd::Play(DeckId::B));
        raw(&mut d, 0, 4);
        assert!(d.state().decks[0].master);
        // A's fader moves A's tempo, and B follows it.
        d.apply(DeckCmd::Pitch { deck: DeckId::A, value: 0.04, range: None, takeover: true });
        raw(&mut d, 0, 2);
        let st = d.state();
        assert_eq!(st.decks[0].pitch, 0.04);
        assert!(st.decks[0].synced, "the master stays synced");
        assert!((st.decks[1].pitch - (1.04 * 128.0 / 125.0 - 1.0)).abs() < 1e-3, "{}", st.decks[1].pitch);
        // B's fader, moved from where it shows (its tempo now), turns B's sync off and takes over.
        let held = st.decks[1].pitch;
        d.apply(DeckCmd::Pitch { deck: DeckId::B, value: held, range: None, takeover: true });
        d.apply(DeckCmd::Pitch { deck: DeckId::B, value: held - 0.01, range: None, takeover: true });
        raw(&mut d, 0, 2);
        let st = d.state();
        assert!(!st.decks[1].synced);
        assert!((st.decks[1].pitch - (held - 0.01)).abs() < 1e-9);
        assert!((st.decks[1].rate - (1.0 + held - 0.01)).abs() < 1e-6, "{}", st.decks[1].rate);
        // An on-screen fader (no takeover) applies wherever it is grabbed, even after sync off.
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: true });
        raw(&mut d, 0, 2);
        d.apply(DeckCmd::Pitch { deck: DeckId::B, value: -0.05, range: None, takeover: false });
        raw(&mut d, 0, 1);
        let st = d.state();
        assert!(!st.decks[1].synced);
        assert_eq!(st.decks[1].pitch, -0.05);
    }

    #[test]
    fn a_stopped_deck_shows_the_tempo_its_fader_sets() {
        let ta = track(60.0, 128.0, 0.0, |_| 0.1);
        let tb = track(60.0, 125.0, 0.0, |_| 0.1);
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        // Rendered as the mixer does (not `raw`, which sets the rates itself).
        let mut main = vec![vec![0.0f32; BLOCK]; 2];
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: true });
        d.render(&mut main, BLOCK);
        // Both stopped: A's fader moves A's tempo now, and B (synced) follows it.
        d.apply(DeckCmd::Pitch { deck: DeckId::A, value: -0.03, range: None, takeover: false });
        d.render(&mut main, BLOCK);
        let st = d.state();
        assert!(!st.decks[0].playing && !st.decks[1].playing);
        assert!((st.decks[0].bpm - 128.0 * 0.97).abs() < 0.01, "{}", st.decks[0].bpm);
        assert!((st.decks[1].bpm - 128.0 * 0.97).abs() < 0.01, "{}", st.decks[1].bpm);
    }

    #[test]
    fn a_follower_looping_a_quarter_beat_follows_the_tempo_only() {
        let ta = track(60.0, 128.0, 0.0, |_| 0.1);
        let tb = track(60.0, 125.0, 0.0, |_| 0.1);
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: true });
        d.apply(DeckCmd::Play(DeckId::A));
        d.apply(DeckCmd::Play(DeckId::B));
        raw(&mut d, 0, 20);
        d.apply(DeckCmd::Loop { deck: DeckId::B, action: LoopAction::Beats(0.25) });
        let matched = 128.0 / 125.0;
        for _ in 0..(8.0 * 22_500.0 / BLOCK as f64) as usize {
            raw(&mut d, 0, 1);
            let r = d.state().decks[1].rate;
            assert!((r / matched - 1.0).abs() < 0.01, "{r}");
        }
    }

    #[test]
    fn beat_loops_are_bar_exact_seamless_and_halve_and_double() {
        let t = track(30.0, 128.0, 0.0, clicks(128.0, 0.0));
        let mut d = decks();
        load(&mut d, DeckId::A, &t);
        let a = DeckId::A;
        d.apply(DeckCmd::Seek { deck: a, at: 4.0 * 22_500.0 + 300.0 });
        d.apply(DeckCmd::Play(a));
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Beats(4.0) });
        let s = d.state().decks[0];
        assert_eq!((s.loop_in, s.loop_out, s.loop_on), (Some(90_000.0), Some(180_000.0), true));
        let out = raw(&mut d, 0, 2000);
        let at: Vec<usize> = out.iter().enumerate().filter(|(_, v)| **v > 0.5).map(|(i, _)| i).collect();
        assert!(at.len() > 40);
        assert!(at.windows(2).all(|w| w[1] - w[0] == 22_500), "{:?}", &at[..8]);
        assert!(at.iter().all(|i| out[*i] == 1.0), "seam clicks whole");
        // Seamless: a sine through the seam steps no more than its own slope allows.
        let hz = 441.0;
        let ts = track(30.0, 128.0, 0.0, move |i| (std::f64::consts::TAU * hz * i as f64 / SR).sin() as f32 * 0.5);
        let mut d = decks();
        load(&mut d, a, &ts);
        d.apply(DeckCmd::Seek { deck: a, at: 67_500.0 + 1000.0 });
        d.apply(DeckCmd::Play(a));
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Beats(1.0) });
        assert_eq!(d.state().decks[0].loop_in, Some(67_500.0));
        let out = raw(&mut d, 0, 300);
        let slope = std::f64::consts::TAU * hz / SR * 0.5;
        let worst = out.windows(2).map(|w| (w[1] - w[0]).abs() as f64).fold(0.0, f64::max);
        assert!(worst < 2.0 * slope, "step {worst} vs slope {slope}");
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Halve });
        assert_eq!(d.state().decks[0].loop_out, Some(78_750.0));
        raw(&mut d, 0, 60);
        let p = d.state().decks[0].position;
        assert!((67_500.0..=78_750.0 + BLOCK as f64).contains(&p), "{p}");
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Double });
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Double });
        assert_eq!(d.state().decks[0].loop_out, Some(112_500.0));
        // Halving stops at a sixteenth of a beat.
        for _ in 0..8 {
            d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Halve });
        }
        assert_eq!(d.state().decks[0].loop_out, Some(67_500.0 + 22_500.0 / 16.0));
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Beats(1.0 / 64.0) });
        let s = d.state().decks[0];
        assert!((s.loop_out.unwrap() - s.loop_in.unwrap() - 22_500.0 / 16.0).abs() < 1e-6);
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Beats(2.0) });
        // Exit plays on past the end; reloop goes back to the start.
        let end = d.state().decks[0].loop_out.unwrap();
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Exit });
        raw(&mut d, 0, 200);
        assert!(d.state().decks[0].position > end);
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Reloop });
        raw(&mut d, 0, 1);
        let start = d.state().decks[0].loop_in.unwrap();
        assert!(d.state().decks[0].position - start < 2.0 * BLOCK as f64);
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Exit });
        let p0 = d.state().decks[0].position;
        d.apply(DeckCmd::BeatJump { deck: a, beats: 8.0 });
        raw(&mut d, 0, 1);
        let p1 = d.state().decks[0].position;
        assert!((p1 - p0 - 8.0 * 22_500.0 - BLOCK as f64).abs() < 1e-6, "{p0} -> {p1}");
    }

    #[test]
    fn a_key_locked_loop_wraps_on_the_beat_and_short_loops_stay_in_budget() {
        let t = track(30.0, 128.0, 0.0, bursts(128.0, 0.0));
        let mut d = decks();
        load(&mut d, DeckId::A, &t);
        d.apply(DeckCmd::MasterTempo { deck: DeckId::A, on: true });
        d.apply(DeckCmd::Seek { deck: DeckId::A, at: 4.0 * 22_500.0 });
        d.apply(DeckCmd::Play(DeckId::A));
        d.apply(DeckCmd::Loop { deck: DeckId::A, action: LoopAction::Beats(1.0) });
        let out = raw(&mut d, 0, 800);
        let on = onsets(&out[20_000..]);
        assert!(on.len() > 10);
        assert!(on.windows(2).all(|w| (w[1] as f64 - w[0] as f64 - 22_500.0).abs() < 48.0), "{on:?}");
        let p = d.state().decks[0].position;
        assert!((90_000.0..112_500.0).contains(&p), "{p}");
        // Sixteenth-beat key-locked loops on both decks: no restarts, about a block of stretching
        // per deck per callback.
        let tb = track(30.0, 130.0, 0.0, bursts(130.0, 0.0));
        load(&mut d, DeckId::B, &tb);
        for id in DeckId::ALL {
            d.apply(DeckCmd::MasterTempo { deck: id, on: true });
            d.apply(DeckCmd::Play(id));
            d.apply(DeckCmd::Loop { deck: id, action: LoopAction::Beats(1.0 / 16.0) });
        }
        let mut main = vec![vec![0.0f32; BLOCK]; 2];
        for b in 0..400 {
            d.render(&mut main, BLOCK);
            let st = d.take_stretched();
            if b > 50 {
                assert!(st.iter().all(|s| *s <= 3 * BLOCK), "block {b}: {st:?}");
            }
        }
    }

    #[test]
    fn a_key_locked_quantised_hot_cue_is_prewarmed() {
        let t = track(30.0, 128.0, 0.0, bursts(128.0, 0.0));
        let mut d = decks();
        load(&mut d, DeckId::A, &t);
        d.apply(DeckCmd::MasterTempo { deck: DeckId::A, on: true });
        d.apply(DeckCmd::Seek { deck: DeckId::A, at: 8.0 * 22_500.0 });
        d.apply(DeckCmd::HotCue { deck: DeckId::A, n: 0, action: HotCueAction::Set, seq: 0 });
        d.apply(DeckCmd::Seek { deck: DeckId::A, at: 1.05 * 22_500.0 });
        d.apply(DeckCmd::Play(DeckId::A));
        raw(&mut d, 0, 2);
        d.take_stretched();
        d.apply(DeckCmd::HotCue { deck: DeckId::A, n: 0, action: HotCueAction::Jump, seq: 0 });
        let mut out = Vec::new();
        for _ in 0..120 {
            out.extend(raw(&mut d, 0, 1));
            let st = d.take_stretched()[0];
            assert!(st <= 3 * BLOCK, "spread over the blocks before the jump");
        }
        // The cue's burst plays where beat 2's would have.
        let on = onsets(&out);
        let beat2 = 2.0 * 22_500.0 - (1.05 * 22_500.0 + 2.0 * BLOCK as f64);
        assert!(on.iter().any(|o| (*o as f64 - beat2).abs() < 48.0), "{on:?} vs {beat2}");
        assert!(d.state().decks[0].position > 8.0 * 22_500.0);
    }

    #[test]
    fn master_tempo_keeps_the_key_and_varispeed_does_not() {
        let t = track(10.0, 120.0, 0.0, |i| (std::f64::consts::TAU * 1000.0 * i as f64 / SR).sin() as f32 * 0.5);
        let freq = |x: &[f32]| {
            let ups: Vec<usize> = x.windows(2).enumerate().filter(|(_, w)| w[0] <= 0.0 && w[1] > 0.0).map(|(i, _)| i).collect();
            (ups.len() - 1) as f64 * SR / (ups[ups.len() - 1] - ups[0]) as f64
        };
        for (key_lock, want) in [(true, 1000.0), (false, 1080.0)] {
            let mut d = decks();
            load(&mut d, DeckId::A, &t);
            d.apply(DeckCmd::Pitch { deck: DeckId::A, value: 0.08, range: None, takeover: true });
            d.apply(DeckCmd::MasterTempo { deck: DeckId::A, on: key_lock });
            d.apply(DeckCmd::Play(DeckId::A));
            let out = raw(&mut d, 0, 200);
            let f = freq(&out[20_000..]);
            assert!((f - want).abs() < want * 0.005, "key lock {key_lock}: {f} Hz");
            let p = d.state().decks[0].position;
            assert!((p - 200.0 * BLOCK as f64 * 1.08).abs() < 2_000.0, "{p}");
        }
    }

    #[test]
    fn loading_onto_a_playing_deck_fades_it_out_then_swaps() {
        let (ta, tb) = (track(10.0, 128.0, 0.0, |_| 0.5), track(10.0, 128.0, 0.0, |_| 0.25));
        let mut d = decks();
        let first = prepared(&ta);
        let (id_a, next) = (first.load(), prepared(&tb));
        let id_b = next.load();
        assert!(d.load(DeckId::A, first).is_none());
        d.apply(DeckCmd::Play(DeckId::A));
        raw(&mut d, 0, 2);
        assert!(d.load(DeckId::A, next).is_none(), "held, not dropped");
        assert_eq!(d.state().decks[0].load, id_a, "the playing track stays until it is silent");
        let out = raw(&mut d, 0, 1);
        assert!((out[0] - 0.5).abs() < 0.01 && out.windows(2).all(|w| (w[1] - w[0]).abs() < 0.05), "faded, not cut");
        assert_eq!(d.state().decks[0].load, id_b);
        assert!(!d.state().decks[0].playing);
        assert_eq!(d.take_retired().map(|r| r.load()), Some(id_a));
        assert!(d.take_retired().is_none());
    }

    #[test]
    fn two_hot_decks_stay_under_minus_one_dbtp() {
        let loud = |hz: f64| move |i: usize| if (hz * i as f64 / SR).fract() < 0.5 { 1.0 } else { -1.0 };
        let (ta, tb) = (track(5.0, 128.0, 0.0, loud(97.0)), track(5.0, 132.0, 0.0, loud(3_313.0)));
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        for id in DeckId::ALL {
            d.apply(DeckCmd::Trim { deck: id, db: 12.0 });
            d.apply(DeckCmd::Eq { deck: id, band: EqBand::Low, db: 6.0 });
            d.apply(DeckCmd::Eq { deck: id, band: EqBand::High, db: 6.0 });
            d.apply(DeckCmd::Assign { deck: id, side: XfadeSide::Thru });
            d.apply(DeckCmd::Play(id));
        }
        let mut meter = soundcraft_dsp::meter::LoudnessMeter::new(SR as f32, 2);
        let mut main = vec![vec![0.0f32; BLOCK]; 2];
        for _ in 0..300 {
            main.iter_mut().for_each(|c| c.fill(0.0));
            d.render(&mut main, BLOCK);
            meter.process(&main, BLOCK);
        }
        let tp = meter.true_peak_db();
        assert!(tp <= CEILING_DB + 0.05 && tp > -6.0, "true peak {tp} dBTP");
    }

    #[test]
    fn the_crossfader_and_faders_set_what_reaches_the_master() {
        let t = track(3.0, 120.0, 0.0, |i| (i as f32 * 0.05).sin() * 0.1);
        let mut d = decks();
        load(&mut d, DeckId::A, &t);
        d.apply(DeckCmd::Play(DeckId::A));
        let level = |d: &mut Decks| {
            let mut main = vec![vec![0.0f32; BLOCK]; 2];
            for _ in 0..20 {
                main.iter_mut().for_each(|c| c.fill(0.0));
                d.render(&mut main, BLOCK);
            }
            main[0].iter().fold(0.0f32, |m, v| m.max(v.abs()))
        };
        let full = {
            d.apply(DeckCmd::Crossfader { value: -1.0, curve: None });
            level(&mut d)
        };
        d.apply(DeckCmd::Crossfader { value: 0.0, curve: None });
        assert!((level(&mut d) / full - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.02);
        d.apply(DeckCmd::Crossfader { value: 1.0, curve: None });
        assert!(level(&mut d) < 1e-4);
        d.apply(DeckCmd::Crossfader { value: 0.0, curve: Some(XfadeCurve::Sharp) });
        assert!((level(&mut d) / full - 1.0).abs() < 0.02);
        d.apply(DeckCmd::Fader { deck: DeckId::A, value: 0.0, curve: None });
        assert!(level(&mut d) < 1e-4);
    }

    #[test]
    fn rendering_never_grows_or_reallocates_storage() {
        let ta = track(30.0, 128.0, 0.0, clicks(128.0, 0.0));
        let tb = track(30.0, 126.0, 1234.0, |i| (i as f32 * 0.01).sin());
        let tc = track(30.0, 124.0, 0.0, |_| 0.1);
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        let (mixer, tracks) = (d.storage(), d.track_storage());
        let incoming = prepared(&tc);
        let mut incoming_storage = Vec::new();
        incoming.storage(&mut incoming_storage);
        let mut main = vec![vec![0.0f32; BLOCK]; 2];
        let cmds = [
            DeckCmd::Play(DeckId::A),
            DeckCmd::Play(DeckId::B),
            DeckCmd::Sync { deck: DeckId::B, on: true },
            DeckCmd::MasterTempo { deck: DeckId::A, on: true },
            DeckCmd::MasterTempo { deck: DeckId::B, on: true },
            DeckCmd::Loop { deck: DeckId::A, action: LoopAction::Beats(0.25) },
            DeckCmd::Jog { deck: DeckId::B, delta: 0.5, mode: JogMode::Nudge },
            DeckCmd::HotCue { deck: DeckId::B, n: 0, action: HotCueAction::Set, seq: 0 },
            DeckCmd::BeatJump { deck: DeckId::B, beats: -4.0 },
            DeckCmd::HotCue { deck: DeckId::B, n: 0, action: HotCueAction::Jump, seq: 0 },
            DeckCmd::Seek { deck: DeckId::B, at: 300_000.0 },
            DeckCmd::Filter { deck: DeckId::A, value: -0.7 },
            DeckCmd::Loop { deck: DeckId::A, action: LoopAction::Double },
            DeckCmd::Loop { deck: DeckId::A, action: LoopAction::Exit },
            DeckCmd::Cue { deck: DeckId::A, down: true },
            DeckCmd::Cue { deck: DeckId::A, down: true },
            DeckCmd::Cue { deck: DeckId::A, down: false },
        ];
        for c in cmds {
            d.apply(c);
            for _ in 0..40 {
                d.render(&mut main, BLOCK);
            }
        }
        assert_eq!((d.storage(), d.track_storage()), (mixer.clone(), tracks), "key-locked restarts and loops keep every buffer");
        // A load onto a playing deck: held, swapped, the old one retired whole.
        d.apply(DeckCmd::Play(DeckId::B));
        assert!(d.load(DeckId::B, incoming).is_none());
        for _ in 0..4 {
            d.render(&mut main, BLOCK);
        }
        let mut now = Vec::new();
        d.decks[1].storage(&mut now);
        assert_eq!(now, incoming_storage);
        assert!(d.take_retired().is_some());
        assert_eq!(d.storage(), mixer);
    }
}
