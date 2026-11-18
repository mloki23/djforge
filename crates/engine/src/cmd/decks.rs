//! Live deck commands (`deck.*`, `decks.state`): two CDJ-style decks and a DJ mixer played live
//! over the master bus (see `soundcraft_mix::decks`). Nothing here touches the document: deck
//! state is live state, never saved and not undoable. Each command checks its parameters and
//! queues a [`DeckRequest`] for the host's audio engine; what the decks play comes back in
//! [`Engine::deck_status`].
//!
//! `deck.load` reads, decodes and analyses its file on a thread of its own ([`DeckLoadJob`]);
//! [`Engine::poll_deck_loads`] (the host calls it each frame) hands the prepared deck on.

use super::dj::{Beats, analysed_key, grid_of};
use super::*;
use crate::{DeckRequest, TransportRequest, cmd};
use serde_json::json;
use soundcraft_mix::{
    BeatGrid, DeckCmd, DeckId, DeckTrack, EqBand, FaderCurve, HOT_CUES, HotCueAction, JogMode, LoopAction, PITCH_RANGES, PreparedDeck, XfadeCurve,
    XfadeSide,
};
use soundcraft_model::SourceAudio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const LOAD: &str = "deck.load";

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            noundo "deck.load",
            "Load Track to Deck",
            [],
            None,
            "{path, deck?: a|b (default: a deck that is not playing, A first), force?: bool (load onto a playing deck: it fades out first), bpm? + first_beat? (seconds of a downbeat)} — reads and decodes the audio file on a thread of its own and loads it on the deck, stopped at its first downbeat; the last load issued for a deck wins. Refuses a playing deck unless `force`. Its beat grid: the given bpm, else mixforge's analysis (when the app has it), else the MP3's BPM tag (first downbeat at 0), else none (no sync, loops or quantise). Hot cues A–D (Mix in, Bass hit, Mix out, Bass gone): the open DJ set's when it plays the file, else mixforge's mix points. On an MP3, mixforge's grid and the cues move by its priming to its gapless decode. Returns {deck, path, loading: true}; the result ({deck, title, grid, bpm, key, cues_from: session|mixforge|null, priming_ms, analysis_error}) goes to the status line",
            always,
            load
        ),
        cmd!(noundo "deck.play", "Play Deck", [], None, "{deck: a|b} — plays the deck from its playhead (on its exact sample); the arrangement stops", always, |e, p| transport(e, p, "deck.play")),
        cmd!(noundo "deck.pause", "Pause Deck", [], None, "{deck: a|b} — pauses the deck (a 2 ms fade); the playhead stays", always, |e, p| transport(e, p, "deck.pause")),
        cmd!(
            noundo "deck.cue",
            "Deck Cue",
            [],
            None,
            "{deck: a|b, down?: bool (default true), tap?: bool (a press and release)} — the CDJ cue button. Stopped: down sets the cue point at the playhead (on the nearest beat with quantise) and plays from it while held; up returns there and pauses. Playing: down returns to the cue point and pauses",
            always,
            |e, p| transport(e, p, "deck.cue")
        ),
        cmd!(noundo "deck.seek", "Seek Deck", [], None, "{deck: a|b, at: seconds into the track} — moves the playhead (crossfaded while playing); leaves a loop it moves out of", always, |e, p| transport(e, p, "deck.seek")),
        cmd!(
            noundo "deck.jog",
            "Jog Deck",
            [],
            None,
            "{deck: a|b, delta: -1..=1, mode?: nudge|search (default nudge)} — a jog wheel event. nudge (while playing): bends the rate by delta × 10 % for 60 ms after the event (send events while the wheel turns); search, or nudge while stopped: moves the playhead by delta beats (half-seconds without a grid)",
            always,
            |e, p| transport(e, p, "deck.jog")
        ),
        cmd!(
            noundo "deck.pitch",
            "Deck Pitch",
            [],
            None,
            "{deck: a|b, value?: percent (within the range), range?: 8|16|50 (percent)} — the pitch fader: rate = 1 + value/100. Moving it turns sync off",
            always,
            |e, p| transport(e, p, "deck.pitch")
        ),
        cmd!(noundo "deck.master_tempo", "Deck Master Tempo", [], None, "{deck: a|b, on: bool} — master tempo (key lock): on, tempo changes keep the key (time stretch); off, varispeed (the pitch follows)", always, |e, p| transport(e, p, "deck.master_tempo")),
        cmd!(
            noundo "deck.sync",
            "Deck Sync",
            [],
            None,
            "{deck: a|b, on: bool} — beat sync to the other deck (the master: the one that started first when both sync): its tempo, then its beat phase while both play (corrected over about two beats, then held). Off keeps the rate it has. Needs beat grids on both",
            always,
            |e, p| transport(e, p, "deck.sync")
        ),
        cmd!(
            noundo "deck.hotcue",
            "Deck Hot Cue",
            [],
            None,
            "{deck: a|b, n: 1..=8, action?: set|jump|release|clear (default jump)} — hot cue n (A–H): set it at the playhead (on the nearest beat with quantise), jump to it (while playing, with quantise on the next beat, so the beat stays on time; while stopped it plays from the cue until `release`) or clear it",
            always,
            |e, p| transport(e, p, "deck.hotcue")
        ),
        cmd!(
            noundo "deck.loop",
            "Deck Loop",
            [],
            None,
            "{deck: a|b, beats?: 1/4..32 (a number, or \"1/4\" \"1/2\"), action?: in|out|halve|double|exit|reloop} — a loop of `beats` from the nearest beat (on the grid with quantise), or the loop control `action`; loops are bar-exact and seamless. One of `beats` or `action`",
            always,
            |e, p| transport(e, p, "deck.loop")
        ),
        cmd!(noundo "deck.beatjump", "Deck Beat Jump", [], None, "{deck: a|b, beats: -64..=64 (non-zero)} — moves the playhead (and an active loop) by whole beats", always, |e, p| transport(e, p, "deck.beatjump")),
        cmd!(noundo "deck.eq", "Deck EQ", [], None, "{deck: a|b, band: low|mid|high, db: -80..=6 or \"kill\"} — the isolator EQ (crossovers 200 Hz and 2.5 kHz); -80 dB or less is a kill", always, |e, p| mixer(e, p, "deck.eq")),
        cmd!(noundo "deck.filter", "Deck Filter", [], None, "{deck: a|b, value: -1..=1} — the filter knob: 0 off, left a low-pass (to 80 Hz), right a high-pass (to 10 kHz), resonant", always, |e, p| mixer(e, p, "deck.filter")),
        cmd!(noundo "deck.trim", "Deck Trim", [], None, "{deck: a|b, db: -24..=12} — the channel's input gain", always, |e, p| mixer(e, p, "deck.trim")),
        cmd!(noundo "deck.fader", "Deck Channel Fader", [], None, "{deck: a|b, value?: 0..=1, curve?: linear|smooth|sharp} — the channel fader and its curve", always, |e, p| mixer(e, p, "deck.fader")),
        cmd!(noundo "deck.xfader", "Crossfader", [], None, "{value?: -1 (A)..=1 (B), curve?: smooth|sharp} — the crossfader: smooth is constant power, sharp cuts at the ends", always, |e, p| mixer(e, p, "deck.xfader")),
        cmd!(noundo "deck.assign", "Deck Crossfader Assign", [], None, "{deck: a|b, side: a|b|thru} — which side of the crossfader the deck plays through (thru: always heard)", always, |e, p| mixer(e, p, "deck.assign")),
        cmd!(noundo "deck.master_volume", "Deck Master Volume", [], None, "{db: -60..=0} — the decks' master volume, into the Maximizer at -1 dBTP and the master bus", always, |e, p| mixer(e, p, "deck.master_volume")),
        cmd!(noundo "deck.quantise", "Deck Quantise", [], None, "{on: bool} — quantise for cues, hot cues and loops (on by default)", always, |e, p| mixer(e, p, "deck.quantise")),
        cmd!(
            query "decks.state",
            "Decks State",
            [],
            None,
            "{} — per deck: its track (title, artist, path, key, bpm, grid, length), playing, previewing, position (seconds and samples), bpm now, rate, pitch and range (percent), master tempo, synced, master, phase error (beats and ms), cue, hot cues, loop and levels, plus its mixer channel; and the crossfader, master volume, master level and quantise, as the audio engine last reported",
            always,
            state
        ),
    ]
}

/// Refuses a deck command the host can't carry out: no audio engine.
fn need_audio(e: &Engine, cmd: &str) -> Result<()> {
    if e.live_audio {
        Ok(())
    } else {
        Err(bad(cmd, "the decks need the app's audio engine: run this in djforge (or `djforge --control` and `djforge-cli mcp --connect`)"))
    }
}

fn deck_param(cmd: &str, p: &Value) -> Result<DeckId> {
    str_param(p, "deck").and_then(DeckId::parse).ok_or_else(|| bad(cmd, "`deck` must be a or b"))
}

fn bool_param(cmd: &str, p: &Value, key: &str, default: Option<bool>) -> Result<bool> {
    match p.get(key) {
        Some(Value::Bool(b)) => Ok(*b),
        None | Some(Value::Null) => default.ok_or_else(|| bad(cmd, format!("`{key}` must be true or false"))),
        Some(_) => Err(bad(cmd, format!("`{key}` must be true or false"))),
    }
}

/// A finite number in `range`, or `None` when absent.
fn num(cmd: &str, p: &Value, key: &str, range: std::ops::RangeInclusive<f64>) -> Result<Option<f64>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite() && range.contains(x))
            .map(Some)
            .ok_or_else(|| bad(cmd, format!("`{key}` must be a number {}..={}", range.start(), range.end()))),
    }
}

fn need_num(cmd: &str, p: &Value, key: &str, range: std::ops::RangeInclusive<f64>) -> Result<f64> {
    num(cmd, p, key, range.clone())?.ok_or_else(|| bad(cmd, format!("`{key}` required ({}..={})", range.start(), range.end())))
}

/// The deck `p` names, which must have a track.
fn loaded(e: &Engine, cmd: &str, p: &Value) -> Result<DeckId> {
    let d = deck_param(cmd, p)?;
    if e.deck_tracks.get(d.index()).is_some_and(Option::is_some) {
        Ok(d)
    } else {
        Err(bad(cmd, format!("deck {} is empty: load a track first (deck.load)", d.id().to_uppercase())))
    }
}

fn grid_of_deck(e: &Engine, d: DeckId) -> bool {
    e.deck_tracks.get(d.index()).and_then(Option::as_ref).is_some_and(|t| t.grid.is_some())
}

fn send(e: &mut Engine, cmd: DeckCmd) -> Value {
    e.transport_requests.push(TransportRequest::Deck(DeckRequest(cmd)));
    json!({"queued": format!("{cmd:?}")})
}

/// The deck transport commands (play, cue, seek, jog, pitch, sync, cues, loops).
fn transport(e: &mut Engine, p: &Value, cmd: &str) -> Result<Value> {
    need_audio(e, cmd)?;
    let deck = loaded(e, cmd, p)?;
    let rate = e.session().sample_rate.as_f64();
    let c = match cmd {
        "deck.play" => DeckCmd::Play(deck),
        "deck.pause" => DeckCmd::Pause(deck),
        "deck.cue" if bool_param(cmd, p, "tap", Some(false))? => {
            e.transport_requests.push(TransportRequest::Deck(DeckRequest(DeckCmd::Cue { deck, down: true })));
            DeckCmd::Cue { deck, down: false }
        }
        "deck.cue" => DeckCmd::Cue { deck, down: bool_param(cmd, p, "down", Some(true))? },
        "deck.seek" => DeckCmd::Seek { deck, at: need_num(cmd, p, "at", 0.0..=86_400.0)? * rate },
        "deck.jog" => {
            let mode = match str_param(p, "mode") {
                None | Some("nudge") => JogMode::Nudge,
                Some("search") => JogMode::Search,
                Some(_) => return Err(bad(cmd, "`mode` must be nudge or search")),
            };
            DeckCmd::Jog { deck, delta: need_num(cmd, p, "delta", -1.0..=1.0)?, mode }
        }
        "deck.pitch" => {
            let range = match num(cmd, p, "range", 0.0..=100.0)? {
                None => None,
                Some(r) => {
                    Some(PITCH_RANGES.iter().copied().find(|x| (x * 100.0 - r).abs() < 1e-6).ok_or_else(|| bad(cmd, "`range` must be 8, 16 or 50"))?)
                }
            };
            let value = num(cmd, p, "value", -50.0..=50.0)?;
            if value.is_none() && range.is_none() {
                return Err(bad(cmd, "give `value` (percent) or `range`"));
            }
            DeckCmd::Pitch { deck, value: value.map_or(f64::NAN, |v| v / 100.0), range }
        }
        "deck.master_tempo" => DeckCmd::MasterTempo { deck, on: bool_param(cmd, p, "on", None)? },
        "deck.sync" => {
            let on = bool_param(cmd, p, "on", None)?;
            if on && !(grid_of_deck(e, deck) && grid_of_deck(e, deck.other())) {
                return Err(bad(cmd, "sync needs a track with a beat grid on both decks"));
            }
            DeckCmd::Sync { deck, on }
        }
        "deck.hotcue" => {
            let n = p
                .get("n")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| (1..=HOT_CUES).contains(n))
                .ok_or_else(|| bad(cmd, format!("`n` must be 1..={HOT_CUES}")))?;
            let action = match str_param(p, "action") {
                None | Some("jump") => HotCueAction::Jump,
                Some("set") => HotCueAction::Set,
                Some("clear") => HotCueAction::Clear,
                Some("release") => HotCueAction::Release,
                Some(_) => return Err(bad(cmd, "`action` must be set, jump, release or clear")),
            };
            DeckCmd::HotCue { deck, n: n - 1, action }
        }
        "deck.loop" => DeckCmd::Loop { deck, action: loop_action(e, cmd, p, deck)? },
        "deck.beatjump" => {
            let beats = need_num(cmd, p, "beats", -64.0..=64.0)?;
            if beats == 0.0 {
                return Err(bad(cmd, "`beats` must not be 0"));
            }
            DeckCmd::BeatJump { deck, beats }
        }
        _ => return Err(bad(cmd, "not a deck transport command")),
    };
    if matches!(c, DeckCmd::Play(_) | DeckCmd::Cue { down: true, .. }) && e.transport.playing {
        e.messages.push("The arrangement stopped: a deck plays".into());
    }
    Ok(send(e, c))
}

fn loop_action(e: &Engine, cmd: &str, p: &Value, deck: DeckId) -> Result<LoopAction> {
    let beats = match p.get("beats") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(match s.as_str() {
            "1/4" => 0.25,
            "1/2" => 0.5,
            other => other.parse::<f64>().map_err(|_| bad(cmd, "`beats` must be 1/4, 1/2 or a number 0.25..=32"))?,
        }),
        Some(v) => Some(v.as_f64().ok_or_else(|| bad(cmd, "`beats` must be 1/4, 1/2 or a number 0.25..=32"))?),
    };
    let action = str_param(p, "action");
    match (beats, action) {
        (Some(b), None) => {
            if !(b.is_finite() && (0.25..=32.0).contains(&b)) {
                return Err(bad(cmd, "`beats` must be 0.25..=32"));
            }
            if !grid_of_deck(e, deck) {
                return Err(bad(cmd, "beat loops need a beat grid: load the track with a bpm"));
            }
            Ok(LoopAction::Beats(b))
        }
        (None, Some(a)) => match a {
            "in" => Ok(LoopAction::In),
            "out" => Ok(LoopAction::Out),
            "halve" => Ok(LoopAction::Halve),
            "double" => Ok(LoopAction::Double),
            "exit" => Ok(LoopAction::Exit),
            "reloop" => Ok(LoopAction::Reloop),
            _ => Err(bad(cmd, "`action` must be in, out, halve, double, exit or reloop")),
        },
        _ => Err(bad(cmd, "give one of `beats` or `action`")),
    }
}

/// The DJ mixer commands.
fn mixer(e: &mut Engine, p: &Value, cmd: &str) -> Result<Value> {
    need_audio(e, cmd)?;
    let c = match cmd {
        "deck.eq" => {
            let deck = deck_param(cmd, p)?;
            let band = str_param(p, "band").and_then(EqBand::parse).ok_or_else(|| bad(cmd, "`band` must be low, mid or high"))?;
            let db = match p.get("db") {
                Some(Value::String(s)) if s == "kill" => soundcraft_mix::dj_strip::EQ_KILL_DB,
                _ => need_num(cmd, p, "db", -80.0..=6.0)? as f32,
            };
            DeckCmd::Eq { deck, band, db }
        }
        "deck.filter" => DeckCmd::Filter { deck: deck_param(cmd, p)?, value: need_num(cmd, p, "value", -1.0..=1.0)? as f32 },
        "deck.trim" => DeckCmd::Trim { deck: deck_param(cmd, p)?, db: need_num(cmd, p, "db", -24.0..=12.0)? as f32 },
        "deck.fader" => {
            let deck = deck_param(cmd, p)?;
            let curve = match str_param(p, "curve") {
                None => None,
                Some(c) => Some(FaderCurve::parse(c).ok_or_else(|| bad(cmd, "`curve` must be linear, smooth or sharp"))?),
            };
            let value = num(cmd, p, "value", 0.0..=1.0)?;
            if value.is_none() && curve.is_none() {
                return Err(bad(cmd, "give `value` (0..=1) or `curve`"));
            }
            DeckCmd::Fader { deck, value: value.map_or(f32::NAN, |v| v as f32), curve }
        }
        "deck.xfader" => {
            let curve = match str_param(p, "curve") {
                None => None,
                Some(c) => Some(XfadeCurve::parse(c).ok_or_else(|| bad(cmd, "`curve` must be smooth or sharp"))?),
            };
            let value = num(cmd, p, "value", -1.0..=1.0)?;
            if value.is_none() && curve.is_none() {
                return Err(bad(cmd, "give `value` (-1..=1) or `curve`"));
            }
            DeckCmd::Crossfader { value: value.map_or(f32::NAN, |v| v as f32), curve }
        }
        "deck.assign" => DeckCmd::Assign {
            deck: deck_param(cmd, p)?,
            side: str_param(p, "side").and_then(XfadeSide::parse).ok_or_else(|| bad(cmd, "`side` must be a, b or thru"))?,
        },
        "deck.master_volume" => DeckCmd::Master { db: need_num(cmd, p, "db", -60.0..=0.0)? as f32 },
        "deck.quantise" => DeckCmd::Quantise(bool_param(cmd, p, "on", None)?),
        _ => return Err(bad(cmd, "not a deck mixer command")),
    };
    Ok(send(e, c))
}

// ---- loading -------------------------------------------------------------------------------

/// A track read, decoded, gridded and prepared for a deck.
struct Loaded {
    track: Arc<DeckTrack>,
    prepared: Box<PreparedDeck>,
    analysis_error: Option<String>,
    /// Where hot cues A–D came from: `session` (the open DJ set), `mixforge` (its mix points).
    cues_from: Option<&'static str>,
    /// How far an MP3's grid and cues moved to its gapless decode (ms).
    priming_ms: f64,
}

/// A `deck.load` running on a thread of its own.
pub struct DeckLoadJob {
    deck: DeckId,
    path: String,
    /// The deck's load it is (the last issued wins).
    generation: u64,
    force: bool,
    /// The session rate it decodes at.
    rate: u32,
    /// Set when a later load for the deck supersedes it: its analysis stops early.
    cancel: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<Result<Loaded>>,
}

impl DeckLoadJob {
    pub(crate) fn finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// A later load for its deck was issued: this one is to be dropped.
    pub(crate) fn superseded(&self, e: &Engine) -> bool {
        e.deck_generation.get(self.deck.index()).is_some_and(|g| *g != self.generation)
    }

    /// Hands the loaded track to the engine (for the host) and reports it; refused when the
    /// session's rate changed meanwhile, or the deck started playing (unless forced).
    pub(crate) fn finish(self, e: &mut Engine) -> Result<Value> {
        let DeckLoadJob { deck, path, force, rate, handle, .. } = self;
        let l = handle.join().unwrap_or_else(|_| Err(EngineError::Io(format!("{path}: loading the file failed"))))?;
        if e.session().sample_rate.hz() != rate {
            return Err(bad(LOAD, format!("{path}: the session's sample rate changed while it loaded; load it again")));
        }
        if !force && e.deck_status.decks.get(deck.index()).is_some_and(|s| s.playing) {
            return Err(bad(LOAD, format!("Deck {} is playing: pause it first (or load with force)", deck.id().to_uppercase())));
        }
        let t = &l.track;
        let out = json!({"deck": deck.id(), "path": t.path, "title": t.title, "artist": t.artist, "grid": t.grid_from, "bpm": t.bpm,
            "key": t.key, "analysis_error": l.analysis_error, "cues_from": l.cues_from, "priming_ms": l.priming_ms,
            "can_key_lock": l.prepared.can_key_lock()});
        if let Some(slot) = e.deck_tracks.get_mut(deck.index()) {
            *slot = Some(Arc::clone(&l.track));
        }
        e.note_deck_load(deck, l.prepared.load());
        e.deck_loads.push((deck, l.prepared));
        Ok(out)
    }
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("Audio").to_string()
}

fn file_stem(path: &str) -> String {
    std::path::Path::new(path).file_stem().and_then(|n| n.to_str()).unwrap_or("Audio").to_string()
}

/// What a load reads its track with, worked out on the UI thread.
struct LoadPlan {
    given: Option<(f64, f64)>,
    /// Cues A–D the open DJ session gives the file (seconds, mixforge's timeline).
    session_cues: Option<[Option<f64>; 4]>,
}

/// Reads and decodes `path` at `rate`, finds its grid (given, mixforge, the BPM tag) and hot
/// cues (the open set's, else mixforge's mix points) and builds the deck's heads. Runs on the
/// job's thread; `cancel` stops mixforge's sidecar early.
///
/// An MP3 decodes gaplessly here while mixforge's times are on its decoder's timeline, which
/// plays the MP3's priming first: mixforge's grid and the cues (also on its timeline) move that
/// much earlier.
fn read_track(path: &str, rate: u32, analyse: Option<&PrepAnalyse>, plan: LoadPlan, cancel: &AtomicBool) -> Result<Loaded> {
    let bytes = crate::io::read_file(path)?;
    let decoded = crate::io::decode_import(&file_name(path), &bytes, rate)?;
    let is_mp3 = std::path::Path::new(path).extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("mp3"));
    let tags = if is_mp3 { soundcraft_audio_io::mp3tags::read_basic(&bytes) } else { Default::default() };
    let priming = if is_mp3 {
        match soundcraft_audio_io::mp3gain::gapless_priming(&bytes) {
            Some((n, r)) if r > 0 => f64::from(n) / f64::from(r),
            _ => 0.0,
        }
    } else {
        0.0
    };
    drop(bytes);
    let (buf, tag_key) = decoded.into_parts();
    let frames = i64::try_from(buf.frames()).unwrap_or(i64::MAX);
    let sr = f64::from(rate);
    let (mut key, mut planned, mut analysis_error) = (tag_key, None, None);
    let (beats, grid_from) = match plan.given {
        Some((bpm, first)) => (Some(Beats::Constant { bpm, first }), Some("given")),
        None => {
            let analysed = match analyse {
                Some(f) => f(path, &|| cancel.load(Ordering::Relaxed)),
                None => Err("no mixforge analysis is set up".to_string()),
            };
            match analysed.and_then(|a| {
                planned = super::deck_cues::planned(&a);
                if let Some(k) = analysed_key(&a) {
                    key = Some(k);
                }
                grid_of(&a).ok_or_else(|| "mixforge's analysis has no usable bpm/first_downbeat".to_string())
            }) {
                Ok((bpm, first)) => (Some(Beats::Constant { bpm, first: first - priming }), Some("mixforge")),
                Err(why) => {
                    analysis_error = Some(why);
                    match tags.bpm.filter(|b| b.is_finite() && (20.0..=400.0).contains(b)) {
                        Some(bpm) => (Some(Beats::Constant { bpm, first: 0.0 }), Some("tag")),
                        None => (None, None),
                    }
                }
            }
        }
    };
    let grid = beats.and_then(|b| BeatGrid::new(b.secs(frames, sr).iter().map(|s| s * sr).collect()));
    let grid_from = grid_from.filter(|_| grid.is_some());
    let bpm = grid.as_ref().map(|g| 60.0 * sr / g.period_at(g.first()));
    let (cues, cues_from) = match (plan.session_cues, planned) {
        (Some(c), _) => (Some(c), Some("session")),
        (None, Some(c)) => (Some(c), Some("mixforge")),
        (None, None) => (None, None),
    };
    let duration = frames as f64 / sr;
    let mut hot = [None; HOT_CUES];
    for (h, c) in hot.iter_mut().zip(cues.into_iter().flatten()) {
        *h = c.map(|s| s - priming).filter(|s| s.is_finite() && (0.0..duration).contains(s)).map(|s| s * sr);
    }
    let cues_from = cues_from.filter(|_| hot.iter().any(Option::is_some));
    let key = key.or_else(|| tags.key.as_deref().and_then(soundcraft_model::camelot_of));
    let track = Arc::new(DeckTrack {
        source: Arc::new(SourceAudio::new(buf)),
        grid,
        bpm,
        key,
        cues: hot,
        title: tags.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| file_stem(path)),
        artist: tags.artist.clone().unwrap_or_default(),
        path: path.to_string(),
        grid_from,
    });
    let prepared = Box::new(PreparedDeck::new(Arc::clone(&track), rate as f32));
    Ok(Loaded { track, prepared, analysis_error, cues_from, priming_ms: priming * 1000.0 })
}

fn load(e: &mut Engine, p: &Value) -> Result<Value> {
    need_audio(e, LOAD)?;
    let path = str_param(p, "path").filter(|s| !s.is_empty()).ok_or_else(|| bad(LOAD, "`path` required"))?.to_string();
    let force = bool_param(LOAD, p, "force", Some(false))?;
    let playing = |d: DeckId| e.deck_status.decks.get(d.index()).is_some_and(|s| s.playing);
    let deck = match p.get("deck") {
        None | Some(Value::Null) => {
            DeckId::ALL.into_iter().find(|d| !playing(*d)).ok_or_else(|| bad(LOAD, "both decks are playing: name the `deck` to load"))?
        }
        Some(_) => deck_param(LOAD, p)?,
    };
    if playing(deck) && !force {
        return Err(bad(LOAD, format!("Deck {} is playing: pause it first (or load with force)", deck.id().to_uppercase())));
    }
    let bpm = num(LOAD, p, "bpm", 20.0..=400.0)?;
    let first = num(LOAD, p, "first_beat", 0.0..=600.0)?;
    let given = match (bpm, first) {
        (Some(b), f) => Some((b, f.unwrap_or(0.0))),
        (None, Some(_)) => return Err(bad(LOAD, "`first_beat` needs a `bpm`")),
        (None, None) => None,
    };
    let plan = LoadPlan { given, session_cues: super::export::cues_for_file(e.session(), std::path::Path::new(&path)) };
    // The last load issued for a deck wins: earlier ones still running are told to stop.
    let generation = e.deck_generation.get(deck.index()).copied().unwrap_or(0).wrapping_add(1);
    if let Some(g) = e.deck_generation.get_mut(deck.index()) {
        *g = generation;
    }
    for j in e.deck_jobs.iter().filter(|j| j.deck == deck) {
        j.cancel.store(true, Ordering::Relaxed);
    }
    let rate = e.session().sample_rate.hz();
    let analyse = e.analyse().cloned();
    let roots = crate::file_access::current();
    let cancel = Arc::new(AtomicBool::new(false));
    let (file, stop) = (path.clone(), Arc::clone(&cancel));
    let work = move || {
        let run = || read_track(&file, rate, analyse.as_ref(), plan, &stop);
        match &roots {
            Some(r) => crate::file_access::confine(Some(r), run),
            None => crate::file_access::unconfined(run),
        }
    };
    let handle = std::thread::Builder::new()
        .name("deck-load".into())
        .spawn(work)
        .map_err(|err| EngineError::Io(format!("could not start loading the file: {err}")))?;
    e.deck_jobs.push(DeckLoadJob { deck, path: path.clone(), generation, force, rate, cancel, handle });
    Ok(json!({"deck": deck.id(), "path": path, "loading": true}))
}

// ---- state ---------------------------------------------------------------------------------

fn state(e: &mut Engine, _: &Value) -> Result<Value> {
    let rate = e.session().sample_rate.as_f64().max(1.0);
    let st = e.deck_status;
    let secs = |s: f64| s / rate;
    let decks: Vec<Value> = DeckId::ALL
        .iter()
        .map(|d| {
            let s = st.decks.get(d.index()).copied().unwrap_or_default();
            let track = e.deck_tracks.get(d.index()).and_then(Option::as_ref).map(|t| {
                json!({"title": t.title, "artist": t.artist, "path": t.path, "key": t.key, "bpm": t.bpm, "grid": t.grid_from,
                    "seconds": t.frames() as f64 / rate})
            });
            let beat_ms = e
                .deck_tracks
                .get(d.index())
                .and_then(Option::as_ref)
                .and_then(|t| t.grid.as_ref())
                .map(|g| g.period_at(s.position) / s.rate.max(1e-6) / rate * 1000.0);
            let hot: Vec<Value> = s.hot.iter().map(|h| h.map_or(Value::Null, |x| json!(secs(x)))).collect();
            json!({
                "deck": d.id(), "track": track, "loading": e.deck_jobs.iter().any(|j| j.deck == *d),
                "playing": s.playing, "previewing": s.previewing,
                "position": secs(s.position), "position_samples": s.position,
                "bpm": s.bpm, "rate": s.rate, "pitch": s.pitch * 100.0, "range": s.range * 100.0,
                "master_tempo": s.key_lock, "synced": s.synced, "master": s.master,
                "phase_error": s.phase_error, "phase_error_ms": s.phase_error.zip(beat_ms).map(|(e, b)| e * b),
                "cue": secs(s.cue), "hot_cues": hot,
                "loop": {"in": s.loop_in.map(secs), "out": s.loop_out.map(secs), "on": s.loop_on},
                "level": s.level,
                "channel": {"trim_db": s.trim_db, "eq_db": s.eq_db, "filter": s.filter, "fader": s.fader, "curve": s.fader_curve.id(), "side": s.side.id()},
            })
        })
        .collect();
    Ok(json!({"audio": e.live_audio, "decks": decks, "xfader": st.xfader, "xfader_curve": st.xfader_curve.id(), "master_db": st.master_db,
        "master_level": st.master_level, "quantise": st.quantise, "arrangement_playing": e.transport.playing}))
}
