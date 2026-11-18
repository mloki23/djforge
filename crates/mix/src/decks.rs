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
//!   first frames come out whole.
//! - **Play** starts on the playhead's exact sample, hard; **pause** fades the playing head out
//!   over [`XFADE_S`] (the playhead stays where pause was pressed).
//!
//! **Transport.** CDJ cue: while paused, cue down sets the cue point at the playhead (on the beat
//! with quantise) and plays from it while held; release returns there and pauses. While
//! playing, cue down returns to the cue point and pauses. Jog nudge bends the rate by up to
//! [`JOG_BEND`] while jog events keep arriving ([`JOG_HOLD_S`] after the last); jog search moves
//! the playhead. Hot cues: eight, set (on the beat with quantise), jumped to (with quantise,
//! on the next beat of the playing deck, so the beat stays on time) and cleared. Loops: N beats
//! from the nearest beat, loop in and out, halve, double, exit and reloop; beat jump moves the
//! playhead (and an active loop) by whole beats.
//!
//! **Sync.** A synced deck follows its master: the other deck, unless that deck is synced too and
//! is not the one that started playing first. Each block the follower's tempo is set so its
//! beats run at the master's rate (from both grids, where the decks are), and while both play
//! its phase error (in beats, wrapped to ±½) is corrected by a rate bend of −[`SYNC_GAIN`]·error
//! (at most ±[`SYNC_MAX`]): a half-beat error is gone in about two beats, and the lock is held
//! from then on. Sync off keeps the rate it had.
//!
//! **Mixer.** Each deck runs through its [`DjStrip`] (trim, isolator EQ, filter, fader), then the
//! crossfader (per its curve and the deck's side), the master volume and a Maximizer at −1 dBTP
//! (true peak), into the main bus before the master faders.
//!
//! Nothing here allocates, locks or frees on the audio thread: the heads' buffers and
//! stretchers come with the [`PreparedDeck`], and a replaced one goes back to the host to be
//! dropped ([`Decks::load`] returns it).

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
/// Phase correction per beat of error (a bend of −gain × error).
pub const SYNC_GAIN: f64 = 3.0;
/// Largest phase-correction bend (fraction).
pub const SYNC_MAX: f64 = 0.3;
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
/// The Maximizer's ceiling over the decks (dBTP).
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
    /// Where the grid came from (`given`, `mixforge`, `tag`), if it has one.
    pub grid_from: Option<&'static str>,
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

/// Copies source frames `[start, start + n)` of every channel into `dst`, silence outside.
fn fill(src: &[Vec<f32>], start: i64, n: usize, dst: &mut [Vec<f32>]) {
    let nsrc = src.len();
    for (c, d) in dst.iter_mut().enumerate() {
        let ch = if nsrc == 0 { None } else { src.get(c % nsrc) };
        for (i, x) in d.iter_mut().take(n).enumerate() {
            let idx = start.saturating_add(i as i64);
            *x = usize::try_from(idx).ok().and_then(|k| ch.and_then(|v| v.get(k))).copied().unwrap_or(0.0);
        }
    }
}

/// A key-locked head's stretcher, its buffers, and where it reads.
struct Stretch {
    st: Stretcher,
    input: Vec<Vec<f32>>,
    out: Vec<Vec<f32>>,
    /// The source position fed next (fractional).
    feed: f64,
    lag: Lag,
}

impl Stretch {
    /// Builds a stretcher for `channels` (allocates).
    fn new(channels: usize, sample_rate: f32) -> Option<Stretch> {
        let ch = channels.max(1);
        let st = Stretcher::new(ch, sample_rate, HEAD_BLOCK, Quality::Default).ok()?;
        let max_in = st.max_input();
        Some(Stretch { input: vec![vec![0.0; max_in]; ch], out: vec![vec![0.0; HEAD_BLOCK]; ch], st, feed: 0.0, lag: Lag::new() })
    }

    /// The source position heard now: what was fed, less the stretcher's latency in source
    /// frames (its output latency at the rates it ran at).
    fn heard(&self) -> f64 {
        self.feed - self.st.input_latency() as f64 - self.lag.sum(self.st.output_latency())
    }

    /// Restarts at source position `at`: primed with the audio before it, then run unheard for
    /// its output latency, so the next frame out is `at`, whole.
    fn start(&mut self, src: &[Vec<f32>], at: f64, rate: f64) {
        self.st.reset();
        self.lag.clear();
        self.feed = at + self.st.input_latency() as f64;
        let pre = (self.st.input_latency() * 2 + 1024).min(self.st.max_input());
        fill(src, self.feed.floor() as i64 - pre as i64, pre, &mut self.input);
        let _ = self.st.seek(&self.input, pre, rate);
        let mut left = self.st.output_latency().min(MAX_WARM);
        while left > 0 {
            let n = left.min(HEAD_BLOCK);
            self.run(src, n, rate);
            left -= n;
        }
    }

    /// Produces `n` frames (at most [`HEAD_BLOCK`]) into `out` at `rate`.
    fn run(&mut self, src: &[Vec<f32>], n: usize, rate: f64) -> bool {
        let n = n.min(HEAD_BLOCK);
        let a = self.feed.floor() as i64;
        let end = self.feed + n as f64 * rate;
        let want = usize::try_from((end.floor() as i64).saturating_sub(a)).unwrap_or(0);
        let n_in = want.min(self.st.max_input());
        fill(src, a, n_in, &mut self.input);
        for c in self.out.iter_mut() {
            c.iter_mut().take(n).for_each(|x| *x = 0.0);
        }
        let ok = self.st.process(&self.input, n_in, &mut self.out, n, 1.0).is_ok();
        self.feed = if n_in == want { end } else { a as f64 + n_in as f64 };
        self.lag.push(n, rate);
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
    let (xm, x1, x2) = (at(i - 1), at(i + 1), at(i + 2));
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

    /// Starts the head at source position `at`, key-locked when asked (and it can be).
    fn start(&mut self, src: &[Vec<f32>], at: f64, rate: f64, keylock: bool) {
        self.keylock = keylock && self.stretch.is_some();
        self.pos = at;
        if self.keylock
            && let Some(s) = self.stretch.as_mut()
        {
            s.start(src, at, rate);
        }
        self.ready = true;
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
    Jump,
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
    /// The pitch fader (a fraction, within the range; turns sync off), and optionally its range.
    Pitch {
        deck: DeckId,
        value: f64,
        range: Option<f64>,
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
const DECK_WORDS: usize = 27;
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

/// A head fading out over `total` frames (`left` to go), and whether the playing head fades in
/// over the same frames (a jump) or plays whole (a pause leaves it stopped).
#[derive(Debug, Clone, Copy)]
struct Fade {
    head: usize,
    left: usize,
    total: usize,
    fade_in: bool,
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
    active: usize,
    playing: bool,
    previewing: bool,
    /// The playhead while stopped (while playing, the playing head's position).
    playhead: f64,
    cue: f64,
    hot: [Option<f64>; HOT_CUES],
    lp: LoopSpan,
    /// A quantised jump waiting for its beat: (at, to), source samples.
    pending: Option<(f64, f64)>,
    /// A jump to make as soon as the crossfade under way ends.
    now: Option<f64>,
    fade: Option<Fade>,
    pitch: f64,
    range: f64,
    keylock: bool,
    synced: bool,
    /// When it started playing ([`Decks::clock`]), for which synced deck leads.
    started: u64,
    bend: f64,
    bend_left: usize,
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
            active: 0,
            playing: false,
            previewing: false,
            playhead: 0.0,
            cue: 0.0,
            hot: [None; HOT_CUES],
            lp: LoopSpan::default(),
            pending: None,
            now: None,
            fade: None,
            pitch: 0.0,
            range: 0.08,
            keylock: false,
            synced: false,
            started: 0,
            bend: 0.0,
            bend_left: 0,
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

    fn unready(&mut self) {
        if let Some(l) = self.loaded.as_mut() {
            l.heads.iter_mut().for_each(|h| h.ready = false);
        }
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
        self.lp = LoopSpan::default();
        self.pending = None;
        self.now = None;
        self.fade = None;
        self.bend = 0.0;
        self.bend_left = 0;
        self.phase_err = None;
        self.unready();
        old
    }

    fn play(&mut self, clock: u64) {
        if self.playing {
            // Play while the cue is held: it plays on when the cue is released.
            self.previewing = false;
            return;
        }
        let (at, rate, keylock) = (self.playhead, self.rate_now(), self.keylock);
        let Some(ld) = self.loaded.as_deref_mut() else { return };
        let PreparedDeck { track, heads, .. } = ld;
        if let Some(h) = heads.get_mut(self.active)
            && !(h.ready && h.keylock == (keylock && h.stretch.is_some()) && (h.position() - at).abs() < 0.5)
        {
            h.start(&track.source.buffer.channels, at, rate, keylock);
        }
        self.playing = true;
        self.started = clock;
    }

    fn pause(&mut self, xf: usize) {
        if !self.playing {
            return;
        }
        let p = self.pos();
        self.fade = Some(Fade { head: self.active, left: xf, total: xf, fade_in: false });
        self.active = 1 - self.active.min(1);
        if let Some(h) = self.loaded.as_mut().and_then(|l| l.heads.get_mut(self.active)) {
            h.ready = false;
        }
        self.playing = false;
        self.previewing = false;
        self.playhead = p;
        self.pending = None;
        self.now = None;
    }

    fn cue_button(&mut self, down: bool, quantise: bool, clock: u64, xf: usize) {
        if self.loaded.is_none() {
            return;
        }
        if down {
            if self.playing && !self.previewing {
                self.pause(xf);
                self.playhead = self.cue;
                self.unready();
            } else if !self.playing {
                let p = self.snap(self.playhead, quantise).clamp(0.0, self.frames());
                self.cue = p;
                self.playhead = p;
                self.play(clock);
                self.previewing = true;
            }
        } else if self.previewing {
            self.pause(xf);
            self.playhead = self.cue;
        }
    }

    /// Moves the playhead to `target`: a crossfaded jump while playing.
    fn jump(&mut self, target: f64, xf: usize) {
        let target = target.clamp(0.0, self.frames());
        if !self.playing {
            self.playhead = target;
            self.unready();
        } else if self.fade.is_some() {
            self.now = Some(target);
        } else {
            self.fire(target - self.pos(), xf);
        }
    }

    /// Starts the spare `delta` samples from where the playing head is and crossfades to it.
    fn fire(&mut self, delta: f64, xf: usize) {
        let (p, rate, keylock) = (self.pos(), self.rate_now(), self.keylock);
        let Some(ld) = self.loaded.as_deref_mut() else { return };
        let PreparedDeck { track, heads, .. } = ld;
        let spare = 1 - self.active.min(1);
        if let Some(h) = heads.get_mut(spare) {
            h.start(&track.source.buffer.channels, p + delta, rate, keylock);
        }
        self.fade = Some(Fade { head: self.active, left: xf, total: xf, fade_in: true });
        self.active = spare;
    }

    /// The next jump due at a known position: (at, delta, one-shot).
    fn next_event(&self) -> Option<(f64, f64, bool)> {
        let p = self.pos();
        let mut best = self.pending.map(|(at, to)| (at, to - at, true));
        if self.lp.on
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

    /// Adds `n` frames of the deck into `out` (stereo). Returns nothing; a deck that reaches the
    /// end of its track pauses there.
    fn render(&mut self, out: &mut [Vec<f32>], n: usize, xf: usize) {
        let mut i = 0;
        let mut guard = 0;
        while i < n {
            guard += 1;
            if guard > 64 + n {
                break;
            }
            if self.playing
                && self.fade.is_none()
                && let Some(t) = self.now.take()
            {
                self.fire(t - self.pos(), xf);
                continue;
            }
            let mut seg = (n - i).min(HEAD_BLOCK);
            if self.playing
                && self.fade.is_none()
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
            if let Some(f) = self.fade {
                seg = seg.min(f.left.max(1));
            }
            if !self.playing && self.fade.is_none() {
                break;
            }
            let rate = self.rate;
            let Some(ld) = self.loaded.as_deref_mut() else { break };
            let PreparedDeck { track, heads, .. } = ld;
            let src = &track.source.buffer.channels;
            if let Some(f) = self.fade
                && let Some(h) = heads.get_mut(f.head)
            {
                let k0 = f.total - f.left;
                h.render(src, seg, rate, out, i, |k| fade_out(k0 + k, f.total));
            }
            if self.playing
                && let Some(h) = heads.get_mut(self.active)
            {
                match self.fade.filter(|f| f.fade_in) {
                    Some(f) => {
                        let k0 = f.total - f.left;
                        h.render(src, seg, rate, out, i, |k| fade_in(k0 + k, f.total));
                    }
                    None => h.render(src, seg, rate, out, i, |_| 1.0),
                }
            }
            if let Some(f) = self.fade.as_mut() {
                f.left = f.left.saturating_sub(seg);
                if f.left == 0 {
                    self.fade = None;
                }
            }
            i += seg;
        }
        if self.playing && self.pos() >= self.frames() {
            self.pause(xf);
            self.playhead = self.frames();
        }
    }

    fn hot_cue(&mut self, n: usize, action: HotCueAction, quantise: bool, xf: usize) {
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
            }
            HotCueAction::Clear => {
                if let Some(h) = self.hot.get_mut(n) {
                    *h = None;
                }
            }
            HotCueAction::Jump => {
                let Some(to) = self.hot.get(n).copied().flatten() else { return };
                if let (Some(a), Some(b)) = (self.lp.a, self.lp.b)
                    && (to < a || to >= b)
                {
                    self.lp.on = false;
                }
                match self.grid().filter(|_| quantise && self.playing) {
                    // On the next beat of what plays, so the beat stays where it was.
                    Some(g) => {
                        let at = g.pos_at(g.beat_at(p).ceil());
                        self.pending = Some((at, to));
                    }
                    None => self.jump(to, xf),
                }
            }
        }
    }

    fn set_loop(&mut self, action: LoopAction, quantise: bool, xf: usize) {
        if self.loaded.is_none() {
            return;
        }
        let p = self.pos();
        let beats = |d: &Deck, from: f64, n: f64| d.grid().map(|g| g.pos_at(g.beat_at(from) + n));
        match action {
            LoopAction::Beats(n) => {
                let Some(g) = self.grid() else { return };
                let n = if n.is_finite() { n.clamp(1.0 / 32.0, 512.0) } else { return };
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
                        let len = g.beat_at(b) - g.beat_at(a);
                        beats(self, a, len * k)
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
            Some(g) => g.pos_at(g.beat_at(p) + n),
            None => p + n * self.loaded.as_ref().map_or(0.0, |l| l.track.source.buffer.sample_rate as f64) * JOG_SEARCH_S,
        };
        let delta = target - p;
        if self.lp.on
            && let (Some(a), Some(b)) = (self.lp.a, self.lp.b)
        {
            // An active loop moves with it.
            self.lp.a = Some(a + delta);
            self.lp.b = Some(b + delta);
        }
        self.jump(target, xf);
    }

    fn jog(&mut self, delta: f64, mode: JogMode, sr: f64, xf: usize) {
        if self.loaded.is_none() || !delta.is_finite() {
            return;
        }
        let delta = delta.clamp(-1.0, 1.0);
        match mode {
            JogMode::Nudge if self.playing => {
                self.bend = delta * JOG_BEND;
                self.bend_left = (JOG_HOLD_S * sr) as usize;
            }
            _ => {
                let p = self.pos();
                let target = match self.grid() {
                    Some(g) => g.pos_at(g.beat_at(p) + delta),
                    None => p + delta * JOG_SEARCH_S * sr,
                };
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
            if self.fade.is_some() {
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
            loop_in: self.lp.a,
            loop_out: self.lp.b,
            loop_on: self.lp.on,
            ..DeckState::default()
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
    quantise: bool,
    /// Counts plays, so the deck that started first is known.
    clock: u64,
    /// Frames left to render after the decks fell silent.
    quiet: usize,
    levels: [[f32; 2]; DECKS],
    master_level: [f32; 2],
}

impl Decks {
    /// The decks and mixer for blocks of up to `max_block` frames at `sample_rate` (allocates).
    pub fn new(sample_rate: f32, max_block: usize) -> Decks {
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 { sample_rate } else { 48_000.0 };
        let max_block = max_block.clamp(16, 16_384);
        let limiter = soundcraft_dsp::create("maximizer").map(|mut m| {
            m.set_param("threshold", 0.0);
            m.set_param("ceiling", CEILING_DB);
            m.set_param("true_peak", 1.0);
            m.prepare(sr, max_block, 2);
            m
        });
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
            limiter,
            quantise: true,
            clock: 0,
            quiet: 0,
            levels: [[0.0; 2]; DECKS],
            master_level: [0.0; 2],
        }
    }

    /// Puts `deck` on deck `id` (stopped, at its first downbeat), returning the one it replaces
    /// for the host to drop off the audio thread.
    pub fn load(&mut self, id: DeckId, deck: Box<PreparedDeck>) -> Option<Box<PreparedDeck>> {
        let i = id.index();
        if let Some(s) = self.strips.get_mut(i) {
            s.reset();
        }
        match self.decks.get_mut(i) {
            Some(d) => d.load(deck),
            None => Some(deck),
        }
    }

    /// True while a deck plays or its sound rings out.
    pub fn awake(&self) -> bool {
        self.quiet > 0 || self.decks.iter().any(|d| d.playing || d.fade.is_some())
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

    /// Applies a command (no allocation: call it on the audio thread).
    pub fn apply(&mut self, cmd: DeckCmd) {
        let (q, xf, sr) = (self.quantise, self.xf, self.sr);
        if matches!(cmd, DeckCmd::Play(_) | DeckCmd::Cue { down: true, .. }) {
            self.clock += 1;
        }
        let clock = self.clock;
        match cmd {
            DeckCmd::Play(id) => {
                if let Some(d) = self.deck(id) {
                    d.play(clock);
                }
            }
            DeckCmd::Pause(id) => {
                if let Some(d) = self.deck(id) {
                    d.pause(xf);
                }
            }
            DeckCmd::Cue { deck, down } => {
                if let Some(d) = self.deck(deck) {
                    d.cue_button(down, q, clock, xf);
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
                    d.jump(at, xf);
                }
            }
            DeckCmd::Jog { deck, delta, mode } => {
                if let Some(d) = self.deck(deck) {
                    d.jog(delta, mode, sr, xf);
                }
            }
            DeckCmd::Pitch { deck, value, range } => {
                if let Some(d) = self.deck(deck) {
                    if let Some(r) = range.filter(|r| PITCH_RANGES.iter().any(|x| (x - r).abs() < 1e-9)) {
                        d.range = r;
                    }
                    if value.is_finite() {
                        d.pitch = value.clamp(-d.range, d.range);
                        d.synced = false;
                    }
                }
            }
            DeckCmd::MasterTempo { deck, on } => {
                if let Some(d) = self.deck(deck) {
                    d.master_tempo(on, xf);
                }
            }
            DeckCmd::Sync { deck, on } => {
                if let Some(d) = self.deck(deck) {
                    d.synced = on;
                    if !on {
                        d.correction = 0.0;
                        d.phase_err = None;
                    }
                }
            }
            DeckCmd::HotCue { deck, n, action } => {
                if let Some(d) = self.deck(deck) {
                    d.hot_cue(n, action, q, xf);
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
        match d.grid() {
            Some(g) => Snap {
                grid: true,
                playing: d.playing,
                synced: d.synced,
                started: d.started,
                beat: g.beat_at(p),
                period: g.period_at(p),
                base: 1.0 + d.pitch,
            },
            None => Snap { playing: d.playing, synced: d.synced, started: d.started, base: 1.0 + d.pitch, ..Snap::default() },
        }
    }

    /// Sets each deck's rate for the next `n` frames: pitch, sync (tempo and phase) and jog.
    fn update(&mut self, n: usize) {
        let snaps = [self.snap(0), self.snap(1)];
        let mut masters = [false; DECKS];
        for x in 0..DECKS {
            let (Some(sx), Some(sy)) = (snaps.get(x), snaps.get(1 - x)) else { continue };
            let Some(d) = self.decks.get_mut(x) else { continue };
            if d.bend_left > 0 {
                d.bend_left = d.bend_left.saturating_sub(n);
            }
            if d.bend_left == 0 {
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
                if sx.playing && sy.playing {
                    let diff = sx.beat - sy.beat;
                    let e = diff - diff.round();
                    d.phase_err = Some(e);
                    d.correction = (-SYNC_GAIN * e).clamp(-SYNC_MAX, SYNC_MAX);
                }
                if let Some(m) = masters.get_mut(1 - x) {
                    *m = true;
                }
            }
            let r = (1.0 + d.pitch) * (1.0 + d.correction) * (1.0 + d.bend);
            d.rate = if r.is_finite() { r.clamp(RATE_LIMITS.0, RATE_LIMITS.1) } else { 1.0 };
        }
        for (d, m) in self.decks.iter_mut().zip(masters) {
            d.master = m;
        }
    }

    /// Adds the decks' mix for the next `frames` frames into `main` (L and R; its first two
    /// channels).
    pub fn render(&mut self, main: &mut [Vec<f32>], frames: usize) {
        let n = frames.min(self.max_block);
        if n == 0 || !self.awake() {
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
            let level = self.levels.get_mut(k);
            let peaks: [f32; 2] = [0, 1].map(|c| buf.get(c).map_or(0.0, |b| b.iter().take(n).fold(0.0f32, |m, v| m.max(v.abs()))));
            if let Some(l) = level {
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
        for (dst, src) in main.iter_mut().zip(self.mix.iter()).take(2) {
            for (d, s) in dst.iter_mut().zip(src.iter()).take(n) {
                *d += if s.is_finite() { *s } else { 0.0 };
            }
        }
        if self.decks.iter().any(|d| d.playing || d.fade.is_some()) {
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

    /// Lengths and capacities of every buffer the decks own (they never change once built).
    #[doc(hidden)]
    pub fn storage(&self) -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        for b in self.bufs.iter().chain(std::iter::once(&self.mix)) {
            v.push((b.len(), b.capacity()));
            v.extend(b.iter().map(|c| (c.len(), c.capacity())));
        }
        for d in &self.decks {
            for h in d.loaded.iter().flat_map(|l| l.heads.iter()) {
                if let Some(s) = &h.stretch {
                    v.extend(s.input.iter().chain(s.out.iter()).map(|c| (c.len(), c.capacity())));
                }
            }
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

    fn decks() -> Decks {
        Decks::new(SR as f32, BLOCK)
    }

    fn load(d: &mut Decks, id: DeckId, t: &Arc<DeckTrack>) {
        assert!(d.load(id, Box::new(PreparedDeck::new(Arc::clone(t), SR as f32))).is_none());
    }

    /// The raw output of deck `k` for `blocks` blocks (rates set as the mixer would).
    fn raw(d: &mut Decks, k: usize, blocks: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..blocks {
            d.update(BLOCK);
            let mut buf = vec![vec![0.0f32; BLOCK]; 2];
            d.decks[k].render(&mut buf, BLOCK, d.xf);
            // The other deck runs too.
            let mut other = vec![vec![0.0f32; BLOCK]; 2];
            d.decks[1 - k].render(&mut other, BLOCK, d.xf);
            out.extend_from_slice(&buf[0]);
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
        let t = track(8.0, 120.0, 0.0, |i| (i as f32 * 0.01).sin());
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
        // The pause fades out: the tail after a pause is short and decays.
        d.apply(DeckCmd::Pause(a));
        let tail = raw(&mut d, 0, 1);
        assert!(tail.iter().skip(d.xf).all(|v| *v == 0.0), "silent after the fade");
        assert!(tail[0].abs() > 0.0 || tail[1].abs() > 0.0);
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
        d.apply(DeckCmd::HotCue { deck: a, n: 3, action: HotCueAction::Set });
        assert_eq!(d.state().decks[0].hot[3], Some(180_000.0));
        // Playing from 1.4 beats, jump: it lands on the next beat (beat 2's moment).
        d.apply(DeckCmd::Seek { deck: a, at: 1.4 * 22_500.0 });
        d.apply(DeckCmd::Play(a));
        d.apply(DeckCmd::HotCue { deck: a, n: 3, action: HotCueAction::Jump });
        let out = raw(&mut d, 0, 120);
        let first_click = out.iter().position(|v| *v > 0.5).unwrap();
        // Beat 2 would have sounded 0.6 beats in; the cue's click (beat 8) sounds there instead.
        assert_eq!(first_click, (0.6 * 22_500.0) as usize);
        assert_eq!(out[first_click], 1.0, "whole, after the crossfade");
        // The beats go on from beat 8, on time.
        let next = out.iter().skip(first_click + 1).position(|v| *v > 0.5).unwrap() + first_click + 1;
        assert_eq!(next - first_click, 22_500);
    }

    /// Phase error (ms) of deck B against A, from their positions and grids.
    fn phase_ms(d: &Decks, ta: &DeckTrack, tb: &DeckTrack) -> f64 {
        let s = d.state();
        let (ga, gb) = (ta.grid.as_ref().unwrap(), tb.grid.as_ref().unwrap());
        let (ba, bb) = (ga.beat_at(s.decks[0].position), gb.beat_at(s.decks[1].position));
        let e = (bb - ba) - (bb - ba).round();
        // In time: a beat of A as it plays.
        e * ga.period_at(s.decks[0].position) / s.decks[0].rate / SR * 1000.0
    }

    fn sync_locks(pitch_a: f64, key_lock: bool) {
        let ta = track(120.0, 128.0, 0.0, |i| (i as f32 * 0.02).sin() * 0.3);
        let tb = track(120.0, 132.0, 0.037 * SR, |i| (i as f32 * 0.03).sin() * 0.3);
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        d.apply(DeckCmd::Pitch { deck: DeckId::A, value: pitch_a, range: Some(0.16) });
        d.apply(DeckCmd::MasterTempo { deck: DeckId::B, on: key_lock });
        d.apply(DeckCmd::Play(DeckId::A));
        raw(&mut d, 0, 50);
        // B starts somewhere off the beat, then syncs.
        d.apply(DeckCmd::Seek { deck: DeckId::B, at: 3.37 * 48_000.0 });
        d.apply(DeckCmd::Play(DeckId::B));
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: true });
        let beat_frames = 60.0 / (128.0 * (1.0 + pitch_a)) * SR;
        let blocks_per_beat = beat_frames / BLOCK as f64;
        let mut worst_after = 0.0f64;
        let blocks = (blocks_per_beat * 4.0 * 33.0) as usize;
        for b in 0..blocks {
            raw(&mut d, 1, 1);
            let ms = phase_ms(&d, &ta, &tb);
            if b as f64 > blocks_per_beat * 4.0 {
                worst_after = worst_after.max(ms.abs());
            }
        }
        let s = d.state();
        assert!(s.decks[1].synced && s.decks[0].master && !s.decks[1].master);
        assert!(worst_after < 1.0, "phase error after 4 beats up to {worst_after} ms (pitch {pitch_a}, key lock {key_lock})");
        // Tempo matched: B's bpm is A's.
        assert!((s.decks[1].bpm - s.decks[0].bpm).abs() < 0.01, "{} vs {}", s.decks[1].bpm, s.decks[0].bpm);
        // Sync off keeps the rate.
        let rate = s.decks[1].rate;
        d.apply(DeckCmd::Sync { deck: DeckId::B, on: false });
        raw(&mut d, 1, 1);
        assert!((d.state().decks[1].rate - rate).abs() < 1e-4);
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
    fn sync_locks_a_key_locked_deck() {
        sync_locks(0.0, true);
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
        // Every click a beat apart, through every seam: the loop is bar-exact.
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
        // Halve and double.
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Halve });
        assert_eq!(d.state().decks[0].loop_out, Some(78_750.0));
        raw(&mut d, 0, 60);
        let p = d.state().decks[0].position;
        assert!((67_500.0..=78_750.0 + BLOCK as f64).contains(&p), "{p}");
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Double });
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Double });
        assert_eq!(d.state().decks[0].loop_out, Some(112_500.0));
        // Exit plays on past the end; reloop goes back to the start.
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Exit });
        raw(&mut d, 0, 200);
        assert!(d.state().decks[0].position > 112_500.0);
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Reloop });
        raw(&mut d, 0, 1);
        assert!(d.state().decks[0].position - 67_500.0 < 2.0 * BLOCK as f64);
        // Beat jump moves by whole beats (and the loop with it).
        d.apply(DeckCmd::Loop { deck: a, action: LoopAction::Exit });
        let p0 = d.state().decks[0].position;
        d.apply(DeckCmd::BeatJump { deck: a, beats: 8.0 });
        raw(&mut d, 0, 1);
        let p1 = d.state().decks[0].position;
        assert!((p1 - p0 - 8.0 * 22_500.0 - BLOCK as f64).abs() < 1e-6, "{p0} -> {p1}");
    }

    /// The frequency of `x` by zero crossings.
    fn freq(x: &[f32]) -> f64 {
        let ups: Vec<usize> = x.windows(2).enumerate().filter(|(_, w)| w[0] <= 0.0 && w[1] > 0.0).map(|(i, _)| i).collect();
        let (Some(a), Some(b)) = (ups.first(), ups.last()) else { return 0.0 };
        (ups.len() - 1) as f64 * SR / (b - a) as f64
    }

    #[test]
    fn master_tempo_keeps_the_key_and_varispeed_does_not() {
        let t = track(10.0, 120.0, 0.0, |i| (std::f64::consts::TAU * 1000.0 * i as f64 / SR).sin() as f32 * 0.5);
        for (key_lock, want) in [(true, 1000.0), (false, 1080.0)] {
            let mut d = decks();
            load(&mut d, DeckId::A, &t);
            d.apply(DeckCmd::Pitch { deck: DeckId::A, value: 0.08, range: None });
            d.apply(DeckCmd::MasterTempo { deck: DeckId::A, on: key_lock });
            d.apply(DeckCmd::Play(DeckId::A));
            let out = raw(&mut d, 0, 200);
            let f = freq(&out[20_000..]);
            assert!((f - want).abs() < want * 0.005, "key lock {key_lock}: {f} Hz");
            // The tempo moves either way.
            let p = d.state().decks[0].position;
            assert!((p - 200.0 * BLOCK as f64 * 1.08).abs() < 2_000.0, "{p}");
        }
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
        let mut d = decks();
        load(&mut d, DeckId::A, &ta);
        load(&mut d, DeckId::B, &tb);
        let before = d.storage();
        let mut main = vec![vec![0.0f32; BLOCK]; 2];
        let cmds = [
            DeckCmd::Play(DeckId::A),
            DeckCmd::Play(DeckId::B),
            DeckCmd::Sync { deck: DeckId::B, on: true },
            DeckCmd::MasterTempo { deck: DeckId::A, on: true },
            DeckCmd::Loop { deck: DeckId::A, action: LoopAction::Beats(0.25) },
            DeckCmd::Jog { deck: DeckId::B, delta: 0.5, mode: JogMode::Nudge },
            DeckCmd::HotCue { deck: DeckId::B, n: 0, action: HotCueAction::Set },
            DeckCmd::BeatJump { deck: DeckId::B, beats: -4.0 },
            DeckCmd::HotCue { deck: DeckId::B, n: 0, action: HotCueAction::Jump },
            DeckCmd::Filter { deck: DeckId::A, value: -0.7 },
            DeckCmd::Loop { deck: DeckId::A, action: LoopAction::Double },
            DeckCmd::Cue { deck: DeckId::A, down: true },
            DeckCmd::Cue { deck: DeckId::A, down: true },
            DeckCmd::Cue { deck: DeckId::A, down: false },
            DeckCmd::StopAll,
        ];
        for c in cmds {
            d.apply(c);
            for _ in 0..40 {
                d.render(&mut main, BLOCK);
            }
        }
        assert_eq!(d.storage(), before);
    }
}
