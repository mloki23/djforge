//! Session view launches through the app's audio engine: the engine's `live.*` requests become
//! the player's Live commands on the next quantise boundary, and what the audio thread reports
//! playing goes back to the engine for `live.state`.

use crate::SoundApp;
use soundcraft_engine::LiveRequest;
use soundcraft_engine::cmd::{live_cmds, live_launch_margin, live_next_boundary};

/// Frames in one block of the audio engine (the player renders at most this many at once).
const BLOCK: usize = 512;

impl SoundApp {
    /// Carries out a Session view launch or stop. With the transport stopped it starts playback
    /// from the edit insertion (as Play does) and the request lands on the first boundary from
    /// there; while playing, on the first boundary at least the launch margin ahead.
    pub(crate) fn live_request(&mut self, req: LiveRequest) {
        if self.player.is_none() {
            self.ui.status = "Session view launches need the audio engine".into();
            return;
        }
        let starting = !self.is_playing();
        if starting {
            self.play_from_selection();
        }
        let Some(p) = &self.player else { return };
        let s = self.engine.session();
        let block = p.device_buffer.map_or(BLOCK, |b| usize::try_from(b).unwrap_or(BLOCK)).max(BLOCK);
        let (from, margin) = if starting { (self.engine.transport.position, 0) } else { (p.position(), live_launch_margin(s, block)) };
        match live_next_boundary(s, from, s.live.quantise, margin) {
            Ok(at) => live_cmds(s, &req, at).into_iter().for_each(|c| p.live(c)),
            Err(why) => self.ui.status = why,
        }
    }

    /// Hands the engine what the audio thread last reported the Live columns playing, and
    /// whether launches can be carried out at all.
    pub(crate) fn sync_live_status(&mut self) {
        self.engine.live_audio = self.player.is_some();
        match &self.player {
            Some(p) => {
                for (status, c) in self.engine.live_status.iter_mut().zip(p.live_state()) {
                    *status = soundcraft_engine::LiveStatus { playing: c.playing, queued: c.queued };
                }
            }
            None => self.engine.live_status = Default::default(),
        }
    }
}
