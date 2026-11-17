//! The audio input opened from one short-lived thread after another, nothing else running: as
//! UI tests run one at a time do (each test on its own thread), and as a worker thread might.
//!
//! On Windows, cpal keeps one WASAPI device enumerator for the whole process, made on the first
//! thread to ask under that thread's COM initialisation, which ends with the thread. The next
//! thread to open a device then crashed the process (an access violation). This is its own test
//! binary so that no other thread keeps COM alive and hides the crash.
//!
//! Elsewhere there is no such enumerator and this is only a smoke test: the input opens (or
//! fails cleanly) from ended threads.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_playback::AudioConfig;
use soundcraft_playback::record::Recorder;

/// Tries the audio input on a thread of its own, which then ends.
fn on_a_thread_that_ends() {
    std::thread::spawn(|| {
        // No input device is fine (an error); a crash is not.
        let _ = Recorder::open_with(&AudioConfig::default());
    })
    .join()
    .expect("the audio input thread");
}

#[test]
fn the_input_opens_from_one_ended_thread_after_another() {
    for _ in 0..3 {
        on_a_thread_that_ends();
    }
}
