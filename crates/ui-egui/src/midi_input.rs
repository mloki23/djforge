//! Live MIDI input on the control side: the input devices, which of them are enabled (app
//! preferences, per machine), and which track live MIDI plays.
//!
//! The app owns a [`MidiHost`] over a [`MidiSource`] (`MidiInputs` on the desktop, a
//! `FakeSource` in tests). [`frame`] runs every UI frame and:
//! - hands each player a queue of the source's events (a new player, after an audio device
//!   change, gets a new one);
//! - once a second (not every frame: listing devices creates a backend client), refreshes the
//!   device list, forwarding `Closed` ports to the player so their notes are released, and
//!   opening enabled devices that appear;
//! - sends the player the live target ([`resolve_target`]), which only goes out when it changes.
//!
//! The `midi.*` commands live here as UI commands (like `ui.audio_device`): they need the
//! device list, the preferences and the player, which the app owns and the engine does not.

use crate::SoundApp;
use serde_json::{Value, json};
use soundcraft_midi_io::{MidiSource, PortChange, PortInfo};
use soundcraft_model::{Session, Track, TrackId, TrackKind};
use soundcraft_playback::Player;

/// Events the audio thread's queue holds between two callbacks.
pub const QUEUE_CAPACITY: usize = 4096;

/// Seconds between device-list refreshes.
pub const REFRESH_S: f64 = 1.0;

/// An enabled MIDI input, as stored in the preferences: the port's identity (backend id and raw
/// name, see `soundcraft_midi_io::PortInfo`) and the name it was listed under when enabled.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MidiInputPref {
    pub name: String,
    pub raw_name: String,
    pub backend_id: String,
}

impl MidiInputPref {
    fn is(&self, raw_name: &str, backend_id: &str) -> bool {
        self.raw_name == raw_name && self.backend_id == backend_id
    }
}

/// Whether the port (raw name, backend id) is enabled in `prefs`. On ALSA a replugged device
/// gets a new backend id, so it is enabled again by hand.
pub fn enabled(prefs: &[MidiInputPref], raw_name: &str, backend_id: &str) -> bool {
    prefs.iter().any(|p| p.is(raw_name, backend_id))
}

/// The system's MIDI inputs (`None` on the web, or when the MIDI system can't start).
pub fn system_host() -> Option<MidiHost> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        match soundcraft_midi_io::MidiInputs::new(soundcraft_midi_io::MidiClock::new()) {
            Ok(m) => Some(MidiHost::new(Box::new(m))),
            Err(e) => {
                log::warn!("no MIDI input: {e}");
                None
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        None
    }
}

/// The MIDI input devices and their upkeep (see the module docs).
pub struct MidiHost {
    source: Box<dyn MidiSource>,
    /// When the device list was last refreshed (UI time, seconds); `None` before the first frame.
    last_refresh: Option<f64>,
}

impl MidiHost {
    pub fn new(source: Box<dyn MidiSource>) -> MidiHost {
        MidiHost { source, last_refresh: None }
    }

    /// The source, for reading.
    pub fn source(&self) -> &dyn MidiSource {
        self.source.as_ref()
    }

    /// The source, for tests and the Setup window.
    pub fn source_mut(&mut self) -> &mut dyn MidiSource {
        self.source.as_mut()
    }

    /// One UI frame at `now` seconds (see the module docs).
    pub fn tick(&mut self, now: f64, prefs: &[MidiInputPref], player: Option<&Player>) {
        if let Some(p) = player.filter(|p| !p.has_midi_input()) {
            match self.source.subscribe(QUEUE_CAPACITY) {
                Ok(q) => p.set_midi_input(Some((q, self.source.clock()))),
                Err(e) => log::warn!("live MIDI input: {e}"),
            }
        }
        let Some(last) = self.last_refresh else {
            // The first frame: open the enabled devices present.
            self.last_refresh = Some(now);
            if !prefs.is_empty() {
                for port in self.source.list() {
                    if enabled(prefs, &port.raw_name, &port.backend_id) {
                        self.open(&port.name);
                    }
                }
            }
            return;
        };
        // Nothing enabled means nothing open (ports open only through the preferences): no
        // device list to keep up with.
        if prefs.is_empty() || (now - last < REFRESH_S && now >= last) {
            return;
        }
        self.last_refresh = Some(now);
        for change in self.source.refresh() {
            match change {
                PortChange::Closed { port, name } => {
                    log::info!("MIDI input {name} went away");
                    if let Some(p) = player {
                        p.midi_port_closed(port);
                    }
                }
                PortChange::Appeared { name, raw_name, backend_id } if enabled(prefs, &raw_name, &backend_id) => self.open(&name),
                _ => {}
            }
        }
    }

    fn open(&mut self, name: &str) {
        if let Err(e) = self.source.open(name) {
            log::warn!("{e}");
        }
    }

    /// The ports present, with whether each is open and enabled. Lists the devices (a backend
    /// client): for queries, not every frame.
    pub fn inputs(&self, prefs: &[MidiInputPref]) -> Vec<Value> {
        self.source
            .list()
            .into_iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "raw_name": p.raw_name,
                    "backend_id": p.backend_id,
                    "open": self.source.is_open(&p.name),
                    "enabled": enabled(prefs, &p.raw_name, &p.backend_id),
                })
            })
            .collect()
    }

    /// Enables or disables the port listed as `name` in `prefs`, and opens or closes it.
    pub fn set_enabled(&mut self, name: &str, on: bool, prefs: &mut Vec<MidiInputPref>) -> Result<Value, String> {
        let port: PortInfo = self.source.list().into_iter().find(|p| p.name == name).ok_or_else(|| format!("no MIDI input named \"{name}\""))?;
        prefs.retain(|p| !p.is(&port.raw_name, &port.backend_id));
        if on {
            prefs.push(MidiInputPref { name: port.name.clone(), raw_name: port.raw_name.clone(), backend_id: port.backend_id.clone() });
            // Enabled even if it won't open now: it opens when it next appears.
            self.source.open(&port.name).map_err(|e| e.to_string())?;
        } else {
            self.source.close(&port.name);
        }
        Ok(json!({"name": port.name, "enabled": on, "open": self.source.is_open(&port.name)}))
    }
}

/// A track that plays live MIDI: an active instrument or MIDI track (its instrument plays the
/// notes; an inactive track doesn't render).
fn plays_midi(t: &Track) -> bool {
    matches!(t.kind, TrackKind::Instrument | TrackKind::Midi) && !t.inactive
}

/// The params of the `midi.*` UI commands, as `engine.commands` advertises them.
pub fn params_doc(id: &str) -> Option<&'static str> {
    Some(match id {
        "midi.inputs" | "midi.panic" => "{}",
        "midi.input_enable" => "{name, enabled: bool}",
        "midi.target" => "{track?: id | name | null}",
        _ => return None,
    })
}

/// The target live MIDI plays now, for display: its name, or `None`.
pub fn target_name(app: &SoundApp) -> Option<String> {
    let s = app.engine.session();
    resolve_target(s, app.midi_target).and_then(|t| s.track(t)).map(|t| t.name.clone())
}

/// The track live MIDI input plays: the explicit override (`midi.target`) while it names an
/// instrument track; else the first record-armed instrument track in track order; else the
/// first selected instrument track whose input monitoring is on. `None`: live MIDI plays nothing.
pub fn resolve_target(s: &Session, override_: Option<TrackId>) -> Option<TrackId> {
    if let Some(t) = override_.and_then(|id| s.track(id)).filter(|t| plays_midi(t)) {
        return Some(t.id);
    }
    if let Some(t) = s.tracks.iter().find(|t| plays_midi(t) && t.mixer.record_arm) {
        return Some(t.id);
    }
    s.edit.selected_tracks.iter().filter_map(|id| s.track(*id)).find(|t| plays_midi(t) && t.mixer.input_monitor).map(|t| t.id)
}

/// Every UI frame (see the module docs). `now` is the UI clock in seconds.
pub fn frame(app: &mut SoundApp, now: f64) {
    if let Some(h) = &mut app.midi_host {
        h.tick(now, &app.ui.midi_inputs, app.player.as_ref());
    }
    if let Some(p) = &app.player {
        p.set_midi_target(resolve_target(app.engine.session(), app.midi_target));
    }
}

/// The `midi.*` UI commands; `None` for other ids.
/// - `midi.inputs {}`: `{ports: [{name, raw_name, backend_id, open, enabled}], latency_ms, dropped}`.
/// - `midi.input_enable {name, enabled}`: enables or disables a port in the preferences and opens
///   or closes it.
/// - `midi.target {track?}`: the live target override (a track id or name; `null` or nothing
///   clears it). Returns the override and the target in effect.
/// - `midi.panic {}`: every live note off and the controllers reset.
pub fn command(app: &mut SoundApp, id: &str, p: &Value) -> Option<Result<Value, String>> {
    Some(match id {
        "midi.inputs" => {
            let ports = app.midi_host.as_ref().map(|h| h.inputs(&app.ui.midi_inputs)).unwrap_or_default();
            let latency = app.player.as_ref().map(|p| p.midi_latency_ns() as f64 / 1e6);
            let dropped = app.player.as_ref().map(Player::midi_dropped);
            Ok(json!({"ports": ports, "available": app.midi_host.is_some(), "latency_ms": latency, "dropped": dropped}))
        }
        "midi.input_enable" => (|| {
            let name = p.get("name").and_then(Value::as_str).ok_or("`name` required")?;
            let on = p.get("enabled").and_then(Value::as_bool).ok_or("`enabled` (true or false) required")?;
            let host = app.midi_host.as_mut().ok_or("no MIDI input on this system")?;
            let r = host.set_enabled(name, on, &mut app.ui.midi_inputs);
            // Saved straight away, like the recent sessions.
            app.mark_prefs_changed();
            r
        })(),
        "midi.target" => (|| {
            let s = app.engine.session();
            let track = match p.get("track") {
                None | Some(Value::Null) => None,
                Some(Value::String(n)) => Some(s.track_by_name(n).ok_or_else(|| format!("no track named \"{n}\""))?),
                Some(v) => {
                    let id = v.as_u64().ok_or("`track` is a track id or name")?;
                    Some(s.track(TrackId(id)).ok_or_else(|| format!("no track {id}"))?)
                }
            };
            if let Some(t) = track.filter(|t| !plays_midi(t)) {
                return Err(format!("\"{}\" is not an active instrument track", t.name));
            }
            app.midi_target = track.map(|t| t.id);
            let target = resolve_target(app.engine.session(), app.midi_target);
            if let Some(pl) = &app.player {
                pl.set_midi_target(target);
            }
            Ok(json!({"override": app.midi_target.map(|t| t.0), "target": target.map(|t| t.0)}))
        })(),
        "midi.panic" => {
            if let Some(pl) = &app.player {
                pl.midi_panic();
            }
            Ok(json!({}))
        }
        _ => return None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use soundcraft_model::ChannelFormat;

    #[test]
    fn the_override_then_the_armed_then_the_selected_monitored_track_is_the_target() {
        let mut s = Session::default();
        let audio = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let a = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        let b = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        assert_eq!(resolve_target(&s, None), None);
        s.edit.selected_tracks = vec![audio, b];
        s.track_mut(b).unwrap().mixer.input_monitor = true;
        assert_eq!(resolve_target(&s, None), Some(b), "selected and monitored");
        s.track_mut(a).unwrap().mixer.record_arm = true;
        assert_eq!(resolve_target(&s, None), Some(a), "armed comes first");
        assert_eq!(resolve_target(&s, Some(b)), Some(b), "the override comes first");
        assert_eq!(resolve_target(&s, Some(audio)), Some(a), "an audio track is no target");
        s.track_mut(a).unwrap().inactive = true;
        assert_eq!(resolve_target(&s, None), Some(b), "an inactive track is no target");
    }
}
