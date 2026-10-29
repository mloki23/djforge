//! Session view launches through the app's audio engine: the engine's `live.*` requests become
//! the player's Live commands on the next quantise boundary, and what the audio thread reports
//! playing goes back to the engine for `live.state`.

use crate::SoundApp;
use soundcraft_engine::LiveRequest;
use soundcraft_engine::cmd::{live_cmds, live_fold_into_loop, live_launch_margin, live_next_boundary};
use soundcraft_time::{Range, Samples};

/// A device callback's frames when the device doesn't say (the usual default).
const DEFAULT_CALLBACK: u32 = 1_024;

/// Where this frame's Session view requests count from, worked out by the first of them so all
/// land on one boundary.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LiveFrame {
    from: Samples,
    margin: Samples,
    looped: Option<Range>,
}

impl SoundApp {
    /// The largest audio callback in session samples (the device's buffer at the session rate).
    fn callback_samples(&self) -> Samples {
        let Some(p) = &self.player else { return 0 };
        let frames = f64::from(p.device_buffer.unwrap_or(DEFAULT_CALLBACK).max(1));
        let rate = f64::from(p.device_rate.max(1));
        (frames * self.engine.session().sample_rate.as_f64() / rate).ceil() as Samples
    }

    /// Carries out a Session view launch or stop. With the transport stopped (as the player
    /// reports it) it starts open-ended playback from the edit insertion, looping the selection
    /// when loop playback is on, with no pre-roll; the request lands on the first boundary at or
    /// after that point (Play and the launch reach the audio thread together, so no margin).
    /// While playing, it lands on the first boundary at least the launch margin ahead, folded
    /// into the transport's loop when it would fall past the loop's end. Every request of one
    /// frame (`frame`) counts from the same point, so they land together.
    pub(crate) fn live_request(&mut self, req: LiveRequest, frame: &mut Option<LiveFrame>) {
        let Some(playing) = self.player.as_ref().map(|p| p.is_playing()) else {
            self.ui.status = "Session view launches need the audio engine".into();
            return;
        };
        let f = match *frame {
            Some(f) => f,
            None => {
                let f = if playing {
                    let from = self.player.as_ref().map_or(0, |p| p.position());
                    LiveFrame { from, margin: live_launch_margin(self.engine.session(), self.callback_samples()), looped: self.play_loop }
                } else {
                    let s = self.engine.session();
                    let sel = s.edit.selection;
                    let looped = (s.edit.loop_playback && !sel.is_empty()).then_some(sel);
                    let from = sel.start.max(0);
                    self.start_play(from, None, looped);
                    LiveFrame { from, margin: 0, looped }
                };
                *frame = Some(f);
                f
            }
        };
        let s = self.engine.session();
        let at = live_next_boundary(s, f.from, s.live.quantise, f.margin).and_then(|at| match f.looped {
            Some(l) => live_fold_into_loop(s, at, f.from, l, s.live.quantise, f.margin),
            None => Ok(at),
        });
        match (at, &self.player) {
            (Ok(at), Some(p)) => live_cmds(s, &req, at).into_iter().for_each(|c| p.live(c)),
            (Err(why), _) => self.ui.status = why,
            (Ok(_), None) => {}
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
