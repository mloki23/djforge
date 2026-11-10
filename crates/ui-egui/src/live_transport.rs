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
    /// The transport is stopped: the first request that goes through starts it.
    start: bool,
}

impl SoundApp {
    /// The largest audio callback in session samples: as the player measured it, else (before
    /// its first callback) the device's buffer at the session rate.
    pub(crate) fn callback_samples(&self) -> Samples {
        let Some(p) = &self.player else { return 0 };
        let measured = p.max_callback_samples();
        if measured > 0 {
            return measured;
        }
        let frames = f64::from(p.device_buffer.unwrap_or(DEFAULT_CALLBACK).max(1));
        let rate = f64::from(p.device_rate.max(1));
        (frames * self.engine.session().sample_rate.as_f64() / rate).ceil() as Samples
    }

    /// Carries out a Session view launch or stop.
    ///
    /// - **While playing** it lands on the first boundary at least the launch margin ahead,
    ///   folded into the transport's loop when it would fall past the loop's end.
    /// - **With the transport stopped** (as the player reports it), a launch or scene starts
    ///   open-ended playback from the edit insertion (looping the selection when loop playback is
    ///   on, with no pre-roll) and lands on the first boundary at or after that point; the launch
    ///   is sent before Play, which keeps it, so the first block can't miss it. A stop does
    ///   nothing: nothing plays. A launch refused (no boundary in the loop, a meter not in 4/4)
    ///   leaves the transport stopped.
    ///
    /// A slot recording (`live.record_slot`) is armed for that boundary the same way.
    ///
    /// Every request of one frame (`frame`) counts from the same point, so they land together.
    pub(crate) fn live_request(&mut self, req: LiveRequest, frame: &mut Option<LiveFrame>) {
        let Some(playing) = self.player.as_ref().map(|p| p.is_playing()) else {
            self.ui.status = "Session view launches need the audio engine".into();
            return;
        };
        let f = match *frame {
            Some(f) => f,
            None if playing => {
                let from = self.player.as_ref().map_or(0, |p| p.position());
                LiveFrame { from, margin: live_launch_margin(self.engine.session(), self.callback_samples()), looped: self.play_loop, start: false }
            }
            None if matches!(req, LiveRequest::Stop { .. } | LiveRequest::StopAll) => return,
            None => {
                let s = self.engine.session();
                let sel = s.edit.selection;
                let looped = (s.edit.loop_playback && !sel.is_empty()).then_some(sel);
                LiveFrame { from: sel.start.max(0), margin: 0, looped, start: true }
            }
        };
        let s = self.engine.session();
        let at = live_next_boundary(s, f.from, s.live.quantise, f.margin).and_then(|at| match f.looped {
            Some(l) => live_fold_into_loop(s, at, f.from, l, s.live.quantise, f.margin),
            None => Ok(at),
        });
        let at = match at {
            Ok(at) => at,
            Err(why) => {
                self.ui.status = why;
                return;
            }
        };
        if let LiveRequest::RecordSlot { column, slot, bars, replace } = req {
            // Recording starts on the boundary; the launch follows once the slot is written.
            if !crate::midi_record::arm_slot(self, column, slot, bars, replace, at, f.looped) {
                return;
            }
        } else if let Some(p) = &self.player {
            live_cmds(s, &req, at).into_iter().for_each(|c| p.live(c));
        }
        if f.start {
            self.start_play(f.from, None, f.looped);
        }
        *frame = Some(LiveFrame { start: false, ..f });
    }

    /// Hands the engine what the audio thread last reported the Live columns playing, and
    /// whether launches can be carried out at all. A launch the audio thread reports late (it
    /// arrived after its sample) is told on the status line.
    pub(crate) fn sync_live_status(&mut self) {
        self.engine.live_audio = self.player.is_some();
        let Some(p) = &self.player else {
            self.engine.live_status = Default::default();
            return;
        };
        let state = p.live_state();
        for (status, c) in self.engine.live_status.iter_mut().zip(state.iter()) {
            *status = soundcraft_engine::LiveStatus { playing: c.playing, queued: c.queued, origin: c.origin };
        }
        let late: u64 = state.iter().map(|c| u64::from(c.late)).sum();
        if late > self.live_late_seen {
            self.ui.status = "Launch landed late — try a larger launch quantise or a smaller audio buffer".into();
        }
        self.live_late_seen = late;
    }
}
