//! The host side of the live decks (stage 9): deck commands go to the player, loaded tracks are
//! handed over, and what the decks play comes back to the engine for `decks.state`.
//!
//! The Decks tab comes later; until then the deck controls are reached from Search (the command
//! palette), through [`PALETTE`].

use crate::SoundApp;
use soundcraft_engine::DeckRequest;
use soundcraft_mix::{DeckCmd, HotCueAction};

/// Search entries for the decks: (label, command, parameters).
pub const PALETTE: &[(&str, &str, &str)] = &[
    ("Load Track to Deck...", "deck.load", ""),
    ("Deck A: Play", "deck.play", r#"{"deck":"a"}"#),
    ("Deck B: Play", "deck.play", r#"{"deck":"b"}"#),
    ("Deck A: Pause", "deck.pause", r#"{"deck":"a"}"#),
    ("Deck B: Pause", "deck.pause", r#"{"deck":"b"}"#),
    ("Deck A: Cue", "deck.cue", r#"{"deck":"a","tap":true}"#),
    ("Deck B: Cue", "deck.cue", r#"{"deck":"b","tap":true}"#),
    ("Deck A: Back to Start", "deck.seek", r#"{"deck":"a","at":0}"#),
    ("Deck B: Back to Start", "deck.seek", r#"{"deck":"b","at":0}"#),
    ("Deck A: Search Forward a Beat", "deck.jog", r#"{"deck":"a","delta":1,"mode":"search"}"#),
    ("Deck B: Search Forward a Beat", "deck.jog", r#"{"deck":"b","delta":1,"mode":"search"}"#),
    ("Deck A: Pitch to 0", "deck.pitch", r#"{"deck":"a","value":0}"#),
    ("Deck B: Pitch to 0", "deck.pitch", r#"{"deck":"b","value":0}"#),
    ("Deck A: Master Tempo On", "deck.master_tempo", r#"{"deck":"a","on":true}"#),
    ("Deck B: Master Tempo On", "deck.master_tempo", r#"{"deck":"b","on":true}"#),
    ("Deck A: Master Tempo Off", "deck.master_tempo", r#"{"deck":"a","on":false}"#),
    ("Deck B: Master Tempo Off", "deck.master_tempo", r#"{"deck":"b","on":false}"#),
    ("Deck A: Sync", "deck.sync", r#"{"deck":"a","on":true}"#),
    ("Deck B: Sync", "deck.sync", r#"{"deck":"b","on":true}"#),
    ("Deck A: Sync Off", "deck.sync", r#"{"deck":"a","on":false}"#),
    ("Deck B: Sync Off", "deck.sync", r#"{"deck":"b","on":false}"#),
    ("Deck A: Hot Cue A", "deck.hotcue", r#"{"deck":"a","n":1}"#),
    ("Deck B: Hot Cue A", "deck.hotcue", r#"{"deck":"b","n":1}"#),
    ("Deck A: 4-Beat Loop", "deck.loop", r#"{"deck":"a","beats":4}"#),
    ("Deck B: 4-Beat Loop", "deck.loop", r#"{"deck":"b","beats":4}"#),
    ("Deck A: Exit Loop", "deck.loop", r#"{"deck":"a","action":"exit"}"#),
    ("Deck B: Exit Loop", "deck.loop", r#"{"deck":"b","action":"exit"}"#),
    ("Deck A: Beat Jump +16", "deck.beatjump", r#"{"deck":"a","beats":16}"#),
    ("Deck B: Beat Jump +16", "deck.beatjump", r#"{"deck":"b","beats":16}"#),
    ("Deck A: Kill Low", "deck.eq", r#"{"deck":"a","band":"low","db":"kill"}"#),
    ("Deck B: Kill Low", "deck.eq", r#"{"deck":"b","band":"low","db":"kill"}"#),
    ("Deck A: Filter Off", "deck.filter", r#"{"deck":"a","value":0}"#),
    ("Deck B: Filter Off", "deck.filter", r#"{"deck":"b","value":0}"#),
    ("Deck A: Trim 0 dB", "deck.trim", r#"{"deck":"a","db":0}"#),
    ("Deck B: Trim 0 dB", "deck.trim", r#"{"deck":"b","db":0}"#),
    ("Deck A: Fader Up", "deck.fader", r#"{"deck":"a","value":1}"#),
    ("Deck B: Fader Up", "deck.fader", r#"{"deck":"b","value":1}"#),
    ("Crossfader Centre", "deck.xfader", r#"{"value":0}"#),
    ("Deck A: Crossfader Thru", "deck.assign", r#"{"deck":"a","side":"thru"}"#),
    ("Deck B: Crossfader Thru", "deck.assign", r#"{"deck":"b","side":"thru"}"#),
    ("Decks Master Volume 0 dB", "deck.master_volume", r#"{"db":0}"#),
    ("Decks Quantise On", "deck.quantise", r#"{"on":true}"#),
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
        for r in self.engine.poll_deck_loads() {
            if let Err(err) = r {
                self.ui.status = err.to_string();
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
