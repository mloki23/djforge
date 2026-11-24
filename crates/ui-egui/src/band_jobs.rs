//! Builds the sources' frequency bands (`SourceAudio::bands`) in the background, so loading a
//! session or importing audio never waits for them. One worker thread takes the sources one at a
//! time, those of clips in view first, and asks for a repaint as each source's bands are ready;
//! until then its waveforms keep one colour (so a screenshot taken straight after a load may show
//! them so). A build stops within about a million samples when its source is dropped or the
//! colours are turned off; a source stopped while still in use is queued again later. The audio
//! thread is never involved: the worker only reads the samples and fills the bands' slots.
//!
//! In the browser there are no threads, and building on the UI thread would stall it, so there no
//! bands are built and waveforms keep one colour.

use soundcraft_model::{SourceAudio, SourceId, SourcePool};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

/// The worker, started with the first source that needs bands, and started again should it be
/// gone.
#[derive(Default)]
pub struct BandJobs {
    #[cfg(not(target_arch = "wasm32"))]
    tx: Option<std::sync::mpsc::Sender<Weak<SourceAudio>>>,
    /// Whether waveforms are coloured by frequency: a build stops when they no longer are.
    on: Arc<AtomicBool>,
}

impl BandJobs {
    /// Queue the sources in `pool` whose bands nobody is building, the ones in `first` (those of
    /// clips in view) before the rest, while `on` (waveforms coloured by frequency); when not,
    /// stop the builds.
    pub fn queue(&mut self, ctx: &egui::Context, pool: &SourcePool, on: bool, first: &[SourceId]) {
        self.on.store(on, Ordering::Release);
        if !on || cfg!(target_arch = "wasm32") {
            return;
        }
        let rest = pool.audio();
        for audio in first.iter().filter_map(|&id| pool.get(id)).chain(rest) {
            if audio.claim_bands() && !self.send(ctx, Arc::downgrade(audio)) {
                // No worker: queued again next time.
                audio.unclaim_bands();
                break;
            }
        }
    }

    /// Queue sources outside the session's pool (the decks' tracks) whose bands nobody is
    /// building, while waveforms are coloured by frequency (as [`BandJobs::queue`] last set).
    pub fn queue_sources<'s>(&mut self, ctx: &egui::Context, sources: impl IntoIterator<Item = &'s Arc<SourceAudio>>) {
        if !self.on.load(Ordering::Acquire) || cfg!(target_arch = "wasm32") {
            return;
        }
        for audio in sources {
            if audio.claim_bands() && !self.send(ctx, Arc::downgrade(audio)) {
                audio.unclaim_bands();
                break;
            }
        }
    }

    /// Hand `audio` to the worker, starting it if need be. False when there is none to take it.
    #[cfg(not(target_arch = "wasm32"))]
    fn send(&mut self, ctx: &egui::Context, audio: Weak<SourceAudio>) -> bool {
        if self.tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel::<Weak<SourceAudio>>();
            let (ctx, on) = (ctx.clone(), Arc::clone(&self.on));
            let spawned = std::thread::Builder::new().name("waveform bands".into()).spawn(move || {
                for audio in rx {
                    if build(&audio, &on) {
                        ctx.request_repaint();
                    }
                }
            });
            match spawned {
                Ok(_) => self.tx = Some(tx),
                Err(err) => {
                    log::warn!("waveform bands: no worker thread: {err}");
                    return false;
                }
            }
        }
        let sent = self.tx.as_ref().is_some_and(|tx| tx.send(audio).is_ok());
        if !sent {
            // The worker is gone: the next frame starts another.
            self.tx = None;
        }
        sent
    }

    #[cfg(target_arch = "wasm32")]
    fn send(&mut self, _ctx: &egui::Context, _audio: Weak<SourceAudio>) -> bool {
        false
    }

    /// The worker as gone, for testing a respawn: its channel has no receiver.
    #[cfg(all(test, not(target_arch = "wasm32")))]
    fn lose_worker(&mut self) {
        let (tx, _) = std::sync::mpsc::channel();
        self.tx = Some(tx);
    }
}

/// Build `audio`'s bands unless it is gone, giving up when nothing but this build holds it any
/// more or the colours go off. A build given up hands its claim back, so a source still in use is
/// queued again. Returns whether its bands were built.
#[cfg(not(target_arch = "wasm32"))]
fn build(audio: &Weak<SourceAudio>, on: &AtomicBool) -> bool {
    let Some(audio) = audio.upgrade() else { return false };
    let done = audio.build_bands_while(|| Arc::strong_count(&audio) > 1 && on.load(Ordering::Acquire));
    if !done {
        audio.unclaim_bands();
    }
    done
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use soundcraft_audio_io::AudioBuffer;

    fn audio(n: usize) -> Arc<SourceAudio> {
        let c: Vec<f32> = (0..n).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![c.clone(), c] }))
    }

    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !done() {
            assert!(std::time::Instant::now() < until, "{what}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn sources_get_their_bands_in_the_background_once() {
        let (a, b) = (audio(48_000), audio(4_800));
        let mut pool = SourcePool::default();
        pool.insert(SourceId(1), Arc::clone(&a));
        pool.insert(SourceId(2), Arc::clone(&b));
        let mut jobs = BandJobs::default();
        let ctx = egui::Context::default();
        jobs.queue(&ctx, &pool, true, &[SourceId(2), SourceId(7)]);
        jobs.queue(&ctx, &pool, true, &[]);
        wait_for("bands built", || a.bands_ready() && b.bands_ready());
        assert!(a.bands(0).is_some_and(|x| !x.is_empty()) && a.bands(1).is_some());
        assert!(!a.claim_bands(), "nothing left to queue");
    }

    #[test]
    fn a_source_dropped_while_its_bands_build_is_let_go() {
        // Long enough that the build is still running when the source is dropped.
        let big = audio(48_000 * 600);
        let weak = Arc::downgrade(&big);
        let mut pool = SourcePool::default();
        pool.insert(SourceId(1), big);
        let mut jobs = BandJobs::default();
        let ctx = egui::Context::default();
        jobs.queue(&ctx, &pool, true, &[]);
        drop(pool);
        // The worker gives it up and frees it; nothing else holds it.
        wait_for("dropped source freed", || weak.strong_count() == 0);
        // The worker carries on with the next source.
        let a = audio(4_800);
        let mut pool = SourcePool::default();
        pool.insert(SourceId(2), Arc::clone(&a));
        jobs.queue(&ctx, &pool, true, &[]);
        wait_for("next source built", || a.bands_ready());
        // A source gone before the worker gets to it is skipped.
        assert!(!build(&Weak::new(), &AtomicBool::new(true)));
    }

    #[test]
    fn turning_the_colours_off_stops_the_builds_and_they_resume_later() {
        let big = audio(48_000 * 600);
        let mut pool = SourcePool::default();
        pool.insert(SourceId(1), Arc::clone(&big));
        let mut jobs = BandJobs::default();
        let ctx = egui::Context::default();
        jobs.queue(&ctx, &pool, true, &[]);
        jobs.queue(&ctx, &pool, false, &[]);
        // Stopped part-way, its claim handed back.
        wait_for("build stopped", || big.bands_pending());
        assert!(!big.bands_ready());
        jobs.queue(&ctx, &pool, true, &[]);
        wait_for("built once the colours are back", || big.bands_ready());
    }

    #[test]
    fn a_lost_worker_is_started_again_and_no_source_is_stranded() {
        let a = audio(4_800);
        let mut pool = SourcePool::default();
        pool.insert(SourceId(1), Arc::clone(&a));
        let mut jobs = BandJobs::default();
        let ctx = egui::Context::default();
        jobs.lose_worker();
        jobs.queue(&ctx, &pool, true, &[]);
        assert!(jobs.tx.is_none(), "the lost worker is let go");
        assert!(a.bands_pending(), "the source is queued again, not stranded");
        jobs.queue(&ctx, &pool, true, &[]);
        wait_for("built by the new worker", || a.bands_ready());
    }
}
