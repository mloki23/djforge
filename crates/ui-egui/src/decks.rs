//! The host side of the live decks (stage 9): deck commands go to the player, loaded tracks are
//! handed over, and what the decks play comes back to the engine for `decks.state`.
//!
//! The Decks tab ([`crate::decks_view`]) shows and plays them; every deck command is also in
//! Search (the command palette), through [`palette`].

use crate::SoundApp;
use soundcraft_engine::DeckRequest;
use soundcraft_mix::{DeckCmd, HotCueAction};

/// A Search entry for the decks: (label, command, parameters; null: the menu's own).
pub type PaletteEntry = (String, &'static str, serde_json::Value);

/// Search entries for the decks: every deck command, for each deck where it is per deck, with
/// the parameters of its everyday uses. The Decks tab (Window › Decks) has every deck control;
/// these let the decks be played from Search too. No nudge (it bends for a moment only) and no
/// hot cue jump (stopped, it plays until a release Search can't send).
pub fn palette() -> Vec<PaletteEntry> {
    use serde_json::json;
    let mut v: Vec<PaletteEntry> = vec![
        ("Load Track to Deck...".into(), "deck.load", serde_json::Value::Null),
        ("Add Library Folder...".into(), "library.add_folder", serde_json::Value::Null),
        ("Remove Library Folder...".into(), "library.remove_folder", serde_json::Value::Null),
        ("Crossfader: Centre".into(), "deck.xfader", json!({"value": 0.0})),
        ("Crossfader: Full A".into(), "deck.xfader", json!({"value": -1.0})),
        ("Crossfader: Full B".into(), "deck.xfader", json!({"value": 1.0})),
        ("Crossfader Curve: Smooth".into(), "deck.xfader", json!({"curve": "smooth"})),
        ("Crossfader Curve: Cut".into(), "deck.xfader", json!({"curve": "sharp"})),
        ("Deck Master Volume: 0 dB".into(), "deck.master_volume", json!({"db": 0.0})),
        ("Headphone Mix: Cue".into(), "deck.headphone_mix", json!({"value": 0.0})),
        ("Headphone Mix: Half".into(), "deck.headphone_mix", json!({"value": 0.5})),
        ("Headphone Mix: Master".into(), "deck.headphone_mix", json!({"value": 1.0})),
        (
            format!("Headphone Level: {} dB", soundcraft_mix::HEADPHONE_DEFAULT_DB),
            "deck.headphone_level",
            json!({"db": soundcraft_mix::HEADPHONE_DEFAULT_DB}),
        ),
        ("Master Cue (toggle)".into(), "deck.master_cue", json!({})),
        ("Deck Quantise: On".into(), "deck.quantise", json!({"on": true})),
        ("Deck Quantise: Off".into(), "deck.quantise", json!({"on": false})),
    ];
    for d in soundcraft_mix::DeckId::ALL {
        let n = d.id().to_uppercase();
        let mut add = |label: String, id: &'static str, mut p: serde_json::Value| {
            if let Some(o) = p.as_object_mut() {
                o.insert("deck".into(), json!(d.id()));
            }
            v.push((format!("Deck {n}: {label}"), id, p));
        };
        add("Play".into(), "deck.play", json!({}));
        add("Pause".into(), "deck.pause", json!({}));
        add("Cue".into(), "deck.cue", json!({"tap": true}));
        add("Back to Start".into(), "deck.seek", json!({"at": 0.0}));
        add("Search Back 1 Beat".into(), "deck.jog", json!({"delta": -1.0, "mode": "search"}));
        add("Search Forward 1 Beat".into(), "deck.jog", json!({"delta": 1.0, "mode": "search"}));
        add("Sync".into(), "deck.sync", json!({"on": true}));
        add("Sync Off".into(), "deck.sync", json!({"on": false}));
        add("Master Tempo On".into(), "deck.master_tempo", json!({"on": true}));
        add("Master Tempo Off".into(), "deck.master_tempo", json!({"on": false}));
        add("Pitch 0 %".into(), "deck.pitch", json!({"value": 0.0}));
        for r in [8, 16, 50] {
            add(format!("Pitch Range ±{r} %"), "deck.pitch", json!({"range": r}));
        }
        for k in 1..=8u8 {
            let letter = char::from(b'A'.saturating_add(k - 1));
            add(format!("Set Hot Cue {letter}"), "deck.hotcue", json!({"n": k, "action": "set"}));
            add(format!("Clear Hot Cue {letter}"), "deck.hotcue", json!({"n": k, "action": "clear"}));
        }
        for (label, beats) in [
            ("1/4 Beat", json!("1/4")),
            ("1/2 Beat", json!("1/2")),
            ("1 Beat", json!(1)),
            ("2 Beats", json!(2)),
            ("4 Beats", json!(4)),
            ("8 Beats", json!(8)),
            ("16 Beats", json!(16)),
            ("32 Beats", json!(32)),
        ] {
            add(format!("Loop {label}"), "deck.loop", json!({"beats": beats}));
        }
        for (label, action) in
            [("Loop In", "in"), ("Loop Out", "out"), ("Loop Halve", "halve"), ("Loop Double", "double"), ("Loop Exit", "exit"), ("Reloop", "reloop")]
        {
            add(label.into(), "deck.loop", json!({"action": action}));
        }
        for b in [1, 4, 16, 32] {
            add(format!("Beat Jump Back {b}"), "deck.beatjump", json!({"beats": -b}));
            add(format!("Beat Jump Forward {b}"), "deck.beatjump", json!({"beats": b}));
        }
        for (label, band) in [("HI", "high"), ("MID", "mid"), ("LOW", "low")] {
            add(format!("EQ {label} Kill"), "deck.eq", json!({"band": band, "db": "kill"}));
            add(format!("EQ {label} 0 dB"), "deck.eq", json!({"band": band, "db": 0.0}));
        }
        add("Filter Off".into(), "deck.filter", json!({"value": 0.0}));
        add("Trim 0 dB".into(), "deck.trim", json!({"db": 0.0}));
        add("Channel Fader Up".into(), "deck.fader", json!({"value": 1.0}));
        add("Channel Fader Down".into(), "deck.fader", json!({"value": 0.0}));
        for (label, side) in [("A", "a"), ("Thru", "thru"), ("B", "b")] {
            add(format!("Crossfader Assign {label}"), "deck.assign", json!({"side": side}));
        }
        add("Headphone Cue On".into(), "deck.headphone_cue", json!({"on": true}));
        add("Headphone Cue Off".into(), "deck.headphone_cue", json!({"on": false}));
    }
    v
}

impl SoundApp {
    /// Carries a deck command to the audio engine. A deck that starts while the arrangement plays
    /// or records stops it the app's way first: a recording is finalised, half-speed reset and a
    /// transport loop or slot recording cancelled.
    pub(crate) fn deck_request(&mut self, req: DeckRequest) {
        let starts = matches!(req.0, DeckCmd::Play(_) | DeckCmd::Cue { down: true, .. } | DeckCmd::HotCue { action: HotCueAction::Jump, .. });
        if starts && (self.is_playing() || self.engine.transport.recording) {
            self.stop_play();
        }
        if let Some(p) = &self.player {
            p.deck(req.0);
        }
    }

    /// Once a frame: finishes `deck.load`s, hands their decks to the player, and reads back
    /// what the decks play.
    pub(crate) fn sync_decks(&mut self) {
        self.engine.poll_library();
        for r in self.engine.poll_deck_loads() {
            match r {
                Ok(v) => {
                    self.ui.status = crate::decks_view::load_status(&v);
                    if let Some(d) = v.get("deck").and_then(serde_json::Value::as_str).and_then(soundcraft_mix::DeckId::parse) {
                        if let Some(slot) = self.decks_view.info.get_mut(d.index()) {
                            *slot = Some(v);
                        }
                        if let Some(e) = self.decks_view.load_error.get_mut(d.index()) {
                            *e = None;
                        }
                    }
                }
                Err(err) => self.ui.status = format!("Deck load failed: {err}"),
            }
        }
        for (id, deck) in std::mem::take(&mut self.engine.deck_loads) {
            if let Some(p) = &self.player {
                p.deck_load(id, deck);
            }
        }
        if let Some(p) = &self.player {
            self.engine.set_deck_status(p.decks_state());
        }
        // Set only when it changes (no allocation per frame).
        let route = self.player.as_ref().map(|p| p.headphones);
        if self.decks_view.phones_route != route {
            self.decks_view.phones_route = route;
            self.engine.headphone_output = self.player.as_ref().and_then(soundcraft_playback::Player::headphone_output);
        }
    }
}
