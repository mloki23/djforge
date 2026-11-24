//! The host side of the live decks (stage 9): deck commands go to the player, loaded tracks are
//! handed over, and what the decks play comes back to the engine for `decks.state`.
//!
//! The Decks tab ([`crate::decks_view`]) shows and plays them; a few deck commands are also in
//! Search (the command palette), through [`PALETTE`].

use crate::SoundApp;
use soundcraft_engine::DeckRequest;
use soundcraft_mix::{DeckCmd, HotCueAction};

/// Search entries for the decks: (label, command, parameters). The Decks tab (Window › Decks)
/// has every deck control; these few stay so the decks can be played from Search too.
pub const PALETTE: &[(&str, &str, &str)] = &[
    ("Load Track to Deck...", "deck.load", ""),
    ("Deck A: Play", "deck.play", r#"{"deck":"a"}"#),
    ("Deck B: Play", "deck.play", r#"{"deck":"b"}"#),
    ("Deck A: Pause", "deck.pause", r#"{"deck":"a"}"#),
    ("Deck B: Pause", "deck.pause", r#"{"deck":"b"}"#),
    ("Deck A: Cue", "deck.cue", r#"{"deck":"a","tap":true}"#),
    ("Deck B: Cue", "deck.cue", r#"{"deck":"b","tap":true}"#),
    ("Deck A: Sync", "deck.sync", r#"{"deck":"a","on":true}"#),
    ("Deck B: Sync", "deck.sync", r#"{"deck":"b","on":true}"#),
    ("Add Library Folder...", "library.add_folder", ""),
    ("Remove Library Folder...", "library.remove_folder", ""),
];

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
    }
}
