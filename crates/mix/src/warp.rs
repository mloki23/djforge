//! Realtime beat-locked playback of warped clips ([`soundcraft_model::WarpMap`]).
//!
//! Each block, the timeline position is turned into a session beat by the tempo map, the beat
//! into a source position by the clip's warp map, and the source span between the block's start
//! and end is fed through a pitch-preserving [`Stretcher`] that produces exactly the block's
//! frames. Tempo changes and ramps therefore follow the session beat for beat.
//!
//! Latency: the stretcher delays its output by `input_latency` (source frames) plus
//! `output_latency` (output frames). The player reads ahead by both, so the audio heard at
//! timeline sample `t` is the source at [`soundcraft_model::WarpMap::source_at_time`].

use soundcraft_model::{Clip, ClipContent, ClipId, Session, SourceId, TrackId};
use soundcraft_stretch::{Quality, Stretcher};
use soundcraft_time::{Range, Samples};
use std::collections::HashMap;

/// Per-clip playback state, kept across blocks by the strip that plays the clip.
///
/// Building one allocates (the stretcher's FFT plans and buffers) and dropping one frees, so a
/// realtime host builds them with [`PreparedWarp::new`] and drops retired ones off the audio
/// thread (see [`crate::MixEngine::adopt_warps`]).
pub struct WarpPlayer {
    st: Stretcher,
    /// Built off the audio thread and handed over, rather than created lazily while rendering.
    pub(crate) prepared: bool,
    /// Timeline sample the next block is expected to start at (continuity check).
    next_pos: Samples,
    /// Source position fed next (fractional).
    next_src: f64,
    /// The audio it last played: another source (a re-split clip) restarts it, so no stretcher
    /// contents of the old audio are heard.
    source: Option<SourceId>,
    input: Vec<Vec<f32>>,
    out: Vec<Vec<f32>>,
}

/// Which audio of a clip a warp player plays: a stem index (`STEMS` order) or
/// [`SOURCE_PLAYER`], the clip's own source.
pub type PlayerStem = u8;

/// The [`PlayerStem`] of the player for an unsplit clip's source.
pub const SOURCE_PLAYER: PlayerStem = u8::MAX;

/// Warp players by clip id and [`PlayerStem`] (a split clip has one per stem).
pub(crate) type WarpPlayers = HashMap<(u64, PlayerStem), WarpPlayer>;

/// The audio a clip plays, one entry per warp player it needs: `(SOURCE_PLAYER, source)` for an
/// unsplit audio clip, or `(i, stem source)` for each of a split clip's four stems. Empty for
/// MIDI and video clips.
pub fn played_sources(c: &Clip) -> impl Iterator<Item = (PlayerStem, SourceId)> + '_ {
    let source = match (&c.content, &c.stems) {
        (ClipContent::Audio { source, .. }, None) => Some((SOURCE_PLAYER, *source)),
        _ => None,
    };
    let stems = match &c.content {
        ClipContent::Audio { .. } => c.stems.as_ref(),
        _ => None,
    };
    source.into_iter().chain(stems.into_iter().flat_map(|set| set.sources.iter().enumerate().map(|(i, s)| (i as PlayerStem, *s))))
}

/// Largest block a player handles at once; longer requests are split.
const PLAYER_BLOCK: usize = 4096;

impl WarpPlayer {
    /// Builds a player for `channels` source channels (allocates).
    pub(crate) fn build(channels: usize, sample_rate: f32, prepared: bool) -> Option<WarpPlayer> {
        let st = Stretcher::new(channels.max(1), sample_rate, PLAYER_BLOCK, Quality::Default).ok()?;
        let max_in = st.max_input();
        Some(WarpPlayer {
            input: vec![vec![0.0; max_in]; channels.max(1)],
            out: vec![vec![0.0; PLAYER_BLOCK]; channels.max(1)],
            st,
            prepared,
            next_pos: Samples::MIN,
            next_src: 0.0,
            source: None,
        })
    }

    /// Source channels the player was built for.
    #[cfg(test)]
    pub(crate) fn channels(&self) -> usize {
        self.input.len()
    }

    /// True when the player was built off the audio thread and handed over.
    pub fn is_prepared(&self) -> bool {
        self.prepared
    }
}

/// A warp player built off the audio thread for a clip, handed over with the session snapshot.
pub struct PreparedWarp {
    pub(crate) track: TrackId,
    pub(crate) clip: ClipId,
    pub(crate) stem: PlayerStem,
    pub(crate) player: WarpPlayer,
}

impl PreparedWarp {
    /// Builds the stretcher now (allocates; call off the audio thread). `stem` says which of
    /// the clip's audio it plays (see [`played_sources`]).
    pub fn new(track: TrackId, clip: ClipId, stem: PlayerStem, channels: usize, sample_rate: f32) -> Option<PreparedWarp> {
        Some(PreparedWarp { track, clip, stem, player: WarpPlayer::build(channels, sample_rate, true)? })
    }

    /// The clip the player is for.
    pub fn clip(&self) -> ClipId {
        self.clip
    }

    /// Which of the clip's audio the player plays.
    pub fn stem(&self) -> PlayerStem {
        self.stem
    }
}

/// Storage for warp players, built off the audio thread and handed to
/// [`crate::MixEngine::adopt_room`] with each session snapshot (and with each later batch of
/// players), so the audio thread never grows a player table or the retired list.
pub struct WarpRoom {
    pub(crate) tables: Vec<(TrackId, WarpPlayers)>,
    pub(crate) retired: Vec<WarpPlayer>,
}

impl WarpRoom {
    /// A table for every track of `s`, sized for all the players its clips play through
    /// ([`crate::warp_clip_plays`], one per [`played_sources`] entry), and room to retire
    /// `retire` players (the batch sent with it). Allocates.
    pub fn for_session(s: &Session, retire: usize) -> WarpRoom {
        let tables = s
            .tracks
            .iter()
            .map(|t| {
                let need: usize = t.clips().iter().filter(|c| crate::warp_clip_plays(t, c)).map(|c| played_sources(c).count()).sum();
                (t.id, HashMap::with_capacity(need))
            })
            .collect();
        WarpRoom { tables, retired: Vec::with_capacity(retire) }
    }

    /// Only room to retire `retire` players (a later batch for the same snapshot). Allocates.
    pub fn retiring(retire: usize) -> WarpRoom {
        WarpRoom { tables: Vec::new(), retired: Vec::with_capacity(retire) }
    }

    /// True when the room owns no heap storage, so dropping it frees nothing (the audio thread
    /// may drop it rather than send it back).
    pub fn is_bare(&self) -> bool {
        self.tables.capacity() == 0 && self.retired.capacity() == 0
    }

    /// The players a room handed back still holds (those that no longer played).
    pub fn players(&self) -> usize {
        self.tables.iter().map(|(_, t)| t.len()).sum::<usize>() + self.retired.len()
    }
}

/// Copies source frames `[start, start + n)` of every channel into `dst`, silence outside the file.
fn fill_input(src: &[Vec<f32>], start: i64, n: usize, dst: &mut [Vec<f32>]) {
    let nsrc = src.len();
    for (c, d) in dst.iter_mut().enumerate() {
        let ch = if nsrc == 0 { None } else { src.get(c % nsrc) };
        for (i, x) in d.iter_mut().take(n).enumerate() {
            let idx = start.saturating_add(i as i64);
            *x = if idx < 0 { 0.0 } else { ch.and_then(|v| v.get(idx as usize)).copied().unwrap_or(0.0) };
        }
    }
}

/// Adds the warped clip's audio for `[pos, pos + frames)` into `buf`: `source` (the clip's own
/// or one of its stems) through the player keyed `stem`, with the clip's offset, warp, fades and
/// gain. `stem_gain`, when given, is a further gain per frame of the block.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_warped(
    s: &Session,
    clip: &Clip,
    source: SourceId,
    stem: PlayerStem,
    stem_gain: Option<&[f32]>,
    pos: Samples,
    frames: usize,
    buf: &mut [Vec<f32>],
    players: &mut WarpPlayers,
) {
    let (ClipContent::Audio { .. }, Some(warp)) = (&clip.content, &clip.warp) else { return };
    if !warp.is_usable() {
        return;
    }
    let block = Range { start: pos, end: pos.saturating_add(frames as i64) };
    let Some(isect) = clip.range().intersect(&block) else { return };
    let Some(audio) = s.pool.get(source) else { return };
    let src = &audio.buffer.channels;
    if src.is_empty() {
        return;
    }
    let sr = s.sample_rate.as_f64() as f32;
    let player = match players.entry((clip.id.0, stem)) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        // A stem's player is always prepared off the audio thread. Until it arrives (it comes
        // with the snapshot for clips near the playhead, a UI frame or two later for others)
        // the stem is left out rather than building a stretcher mid-mix, which could glitch
        // the whole output. A source's player is still built as a fallback, as before stems.
        std::collections::hash_map::Entry::Vacant(_) if stem != SOURCE_PLAYER && crate::on_audio_thread() => return,
        std::collections::hash_map::Entry::Vacant(e) => match WarpPlayer::build(src.len(), sr, false) {
            Some(p) => e.insert(p),
            None => return,
        },
    };
    if player.source != Some(source) {
        player.source = Some(source);
        player.next_pos = Samples::MIN;
    }
    let lat_in = player.st.input_latency() as f64;
    let lat_out = player.st.output_latency() as i64;
    let feed = |t: Samples| warp.source_at_time(&s.tempo, s.sample_rate, clip.start, t.saturating_add(lat_out)) + lat_in;

    let mut t = isect.start;
    while t < isect.end {
        let n = usize::try_from(isect.end - t).unwrap_or(0).min(PLAYER_BLOCK);
        if n == 0 {
            break;
        }
        let t1 = t.saturating_add(n as i64);
        let p0 = feed(t);
        let p1 = feed(t1);
        if player.next_pos != t || (player.next_src - p0).abs() > 1.0 {
            // A jump (seek, first block, edit): restart and pre-roll the source leading up to p0.
            player.st.reset();
            let pre = (player.st.input_latency() * 2 + 1024).min(player.st.max_input());
            let start = p0.floor() as i64 - pre as i64;
            fill_input(src, start, pre, &mut player.input);
            let rate = ((p1 - p0) / n as f64).max(0.01);
            let _ = player.st.seek(&player.input, pre, rate);
            player.next_src = p0;
        }
        let a = player.next_src.floor() as i64;
        let b = p1.floor() as i64;
        let n_in = usize::try_from(b - a).unwrap_or(0).min(player.st.max_input());
        fill_input(src, a, n_in, &mut player.input);
        for c in player.out.iter_mut() {
            c.iter_mut().take(n).for_each(|x| *x = 0.0);
        }
        if player.st.process(&player.input, n_in, &mut player.out, n, 1.0).is_err() {
            return;
        }
        // Clip gain, envelope and fades, evaluated every 32 samples and interpolated.
        let base = usize::try_from(t - pos).unwrap_or(0);
        let nout = player.out.len();
        const STEP: usize = 32;
        let mut i = 0;
        while i < n {
            let seg = (i + STEP).min(n);
            let rel0 = t.saturating_add(i as i64).saturating_sub(clip.start);
            let rel1 = t.saturating_add(seg.saturating_sub(1) as i64).saturating_sub(clip.start).min(clip.length);
            let (g0, g1) = (clip.gain_at(rel0), clip.gain_at(rel1));
            let span = (seg - i).saturating_sub(1).max(1) as f32;
            for (ch, dst) in buf.iter_mut().enumerate() {
                let Some(o) = player.out.get(ch % nout) else { continue };
                for k in i..seg {
                    let g = g0 + (g1 - g0) * (k - i) as f32 / span;
                    // Times exactly 1 for an unsplit clip, which leaves its audio bit-identical.
                    let sg = stem_gain.map_or(1.0, |sg| sg.get(base + k).copied().unwrap_or(0.0));
                    if let (Some(d), Some(v)) = (dst.get_mut(base + k), o.get(k)) {
                        *d += v * g * sg;
                    }
                }
            }
            i = seg;
        }
        player.next_pos = t1;
        player.next_src = if n_in == usize::try_from(b - a).unwrap_or(0) { p1 } else { a as f64 + n_in as f64 };
        t = t1;
    }
}
