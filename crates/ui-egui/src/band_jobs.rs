//! Builds the sources' frequency bands (`SourceAudio::bands`) in the background, so loading a
//! session or importing audio never waits for them. One worker thread takes the sources one at a
//! time; a source dropped while its bands are building is given up within about a million
//! samples, and a repaint is asked for as each source's bands are ready. The audio thread is never
//! involved: the worker only reads the samples and fills the bands' slots.

use soundcraft_model::{SourceAudio, SourcePool};
use std::sync::{Arc, Weak};

/// The worker, started with the first source that needs bands.
#[derive(Default)]
pub struct BandJobs {
    #[cfg(not(target_arch = "wasm32"))]
    tx: Option<std::sync::mpsc::Sender<Weak<SourceAudio>>>,
    /// Sources waiting, where there is no thread to build them on (the browser).
    #[cfg(target_arch = "wasm32")]
    waiting: Vec<Weak<SourceAudio>>,
}

impl BandJobs {
    /// Queue every source in `pool` whose bands nobody is building yet.
    pub fn queue(&mut self, ctx: &egui::Context, pool: &SourcePool) {
        for audio in pool.audio() {
            if audio.claim_bands() {
                self.send(ctx, Arc::downgrade(audio));
            }
        }
        #[cfg(target_arch = "wasm32")]
        if let Some(next) = self.waiting.pop() {
            build(&next);
            ctx.request_repaint();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn send(&mut self, ctx: &egui::Context, audio: Weak<SourceAudio>) {
        if self.tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel::<Weak<SourceAudio>>();
            let ctx = ctx.clone();
            let spawned = std::thread::Builder::new().name("waveform bands".into()).spawn(move || {
                for audio in rx {
                    if build(&audio) {
                        ctx.request_repaint();
                    }
                }
            });
            match spawned {
                Ok(_) => self.tx = Some(tx),
                // Without a worker the waveforms keep one colour.
                Err(err) => log::warn!("waveform bands: no worker thread: {err}"),
            }
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(audio);
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn send(&mut self, _ctx: &egui::Context, audio: Weak<SourceAudio>) {
        self.waiting.push(audio);
    }
}

/// Build `audio`'s bands unless it is gone; given up when nothing but this build holds it any
/// more. Returns whether its bands were built.
fn build(audio: &Weak<SourceAudio>) -> bool {
    let Some(audio) = audio.upgrade() else { return false };
    audio.build_bands_while(|| Arc::strong_count(&audio) > 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_audio_io::AudioBuffer;
    use soundcraft_model::SourceId;

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
        jobs.queue(&ctx, &pool);
        jobs.queue(&ctx, &pool);
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
        jobs.queue(&ctx, &pool);
        drop(pool);
        // The worker gives it up and frees it; nothing else holds it.
        wait_for("dropped source freed", || weak.strong_count() == 0);
        // The worker carries on with the next source.
        let a = audio(4_800);
        let mut pool = SourcePool::default();
        pool.insert(SourceId(2), Arc::clone(&a));
        jobs.queue(&ctx, &pool);
        wait_for("next source built", || a.bands_ready());
        // A source gone before the worker gets to it is skipped.
        assert!(!build(&Weak::new()));
    }
}
