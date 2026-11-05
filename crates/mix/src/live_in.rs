//! Live MIDI input: a controller, pad grid or the on-screen keyboard played through one track's
//! instrument (the *target*), on top of whatever its arrangement clips and Session view slot play.
//!
//! The host hands the engine timed events ([`LiveEvent`], with a sample offset into the next
//! block) for the target ([`crate::MixEngine::set_live_target`]). The target strip sends them to
//! its instrument after the arrangement's and the slot's notes for the block, so on a shared
//! sample a release from either of those comes before a live note-on.
//!
//! **Held notes.** The instrument API is per pitch, with no channel, so whatever the sources,
//! a pitch either sounds on the instrument or not. Each strip keeps a fixed table of the live
//! keys held, by (source, channel, pitch) ([`LiveKeys`]): a sparse `[bool; 128]` per (port,
//! channel) that never allocates and releases exactly one port's keys when its device goes. A
//! pitch is released on the instrument only when its *last* holder lets go: a live note-off is
//! not sent while the arrangement or the slot still hold that pitch, and their note-offs are not
//! sent while a live key holds it ([`Shield`]). So when two sources hold one pitch, each note-on
//! (re)triggers it and it sounds until both have released it; nothing cuts another source's note
//! and nothing is left stuck.
//!
//! Live keys are released on a target change (on the old target), on [`crate::MixEngine::live_release`]
//! (a device unplugged), on [`crate::MixEngine::live_panic`] and on a transport stop; an
//! instrument replaced forgets them (its notes went with it). A seek or a play from stopped
//! keeps them sounding: only the arrangement's and slot's notes are released there.

use soundcraft_dsp::{Plugin, PluginInfo};

/// Most events [`crate::MixEngine::live_input`] holds for one block.
pub const LIVE_INBOX: usize = 2048;

/// Most live keys held on one strip; a note-on beyond it is not played.
const MAX_HELD: usize = 256;

/// How long an instrument keeps rendering after its last live event while the transport is
/// stopped (seconds), on top of the plugin's own tail: its releases ring out.
const RING_S: f64 = 5.0;

/// One live MIDI message for the target's instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveEvent {
    /// Where it came from: the MIDI port's id, or an id the host reserves for its own sources
    /// (the on-screen keyboard). Keys are released per source.
    pub source: u32,
    /// Sample offset in the next block (clamped into the block).
    pub offset: usize,
    /// A MIDI 1.0 channel message: status byte with its channel, then up to two data bytes (0 when
    /// unused). A note-on with velocity 0 is a note-off.
    pub msg: [u8; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Key {
    source: u32,
    ch: u8,
    pitch: u8,
}

/// The live keys held on one strip's instrument (see the module docs). A fixed table.
#[derive(Debug, Clone)]
pub(crate) struct LiveKeys {
    keys: [Key; MAX_HELD],
    len: usize,
    /// Live holders per pitch.
    count: [u16; 128],
    /// The source whose sustain pedal is down, if any.
    sustain: Option<u32>,
    /// Samples the instrument keeps rendering while the transport is stopped.
    ring: i64,
}

impl Default for LiveKeys {
    fn default() -> Self {
        LiveKeys { keys: [Key::default(); MAX_HELD], len: 0, count: [0; 128], sustain: None, ring: 0 }
    }
}

impl LiveKeys {
    /// Whether a live key holds `pitch`.
    pub(crate) fn holds(&self, pitch: u8) -> bool {
        self.count.get(usize::from(pitch)).is_some_and(|c| *c > 0)
    }

    /// Live keys held now.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Something to hear while the transport is stopped: keys held or releases ringing.
    pub(crate) fn awake(&self) -> bool {
        self.len > 0 || self.ring > 0
    }

    /// Keep rendering for `samples` more.
    pub(crate) fn wake(&mut self, samples: i64) {
        self.ring = self.ring.max(samples);
    }

    /// A block of `frames` went by.
    pub(crate) fn tick(&mut self, frames: usize) {
        self.ring = self.ring.saturating_sub(i64::try_from(frames).unwrap_or(i64::MAX)).max(0);
    }

    /// The instrument was silenced or replaced: nothing live sounds on it any more.
    pub(crate) fn forget(&mut self) {
        self.len = 0;
        self.count = [0; 128];
        self.sustain = None;
    }

    fn find(&self, k: Key) -> Option<usize> {
        self.keys.get(..self.len).and_then(|ks| ks.iter().position(|x| *x == k))
    }

    /// Removes the key at `i`; true when it was the pitch's last live holder.
    fn remove(&mut self, i: usize) -> Option<u8> {
        let last = self.len.checked_sub(1)?;
        let k = *self.keys.get(i)?;
        self.keys.swap(i, last);
        self.len = last;
        let c = self.count.get_mut(usize::from(k.pitch))?;
        *c = c.saturating_sub(1);
        (*c == 0).then_some(k.pitch)
    }

    /// Sends one live event to `inst`. `others(pitch)` says whether the arrangement or a slot
    /// holds the pitch (its note-off then waits for them).
    pub(crate) fn play(&mut self, ev: LiveEvent, inst: &mut dyn Plugin, others: impl Fn(u8) -> bool) {
        let [status, d1, d2] = ev.msg;
        let (kind, ch) = (status & 0xF0, status & 0x0F);
        let (d1, d2) = (d1 & 0x7F, d2 & 0x7F);
        let at = ev.offset;
        match kind {
            0x90 if d2 > 0 => {
                let k = Key { source: ev.source, ch, pitch: d1 };
                if self.find(k).is_none() {
                    let (Some(slot), Some(c)) = (self.keys.get_mut(self.len), self.count.get_mut(usize::from(d1))) else {
                        // Too many keys held: this one is not played (it could never be released).
                        return;
                    };
                    *slot = k;
                    *c = c.saturating_add(1);
                    self.len += 1;
                }
                inst.note_on(at, d1, d2);
            }
            0x80 | 0x90 => {
                let k = Key { source: ev.source, ch, pitch: d1 };
                if let Some(i) = self.find(k)
                    && let Some(p) = self.remove(i)
                    && !others(p)
                {
                    inst.note_off(at, p);
                }
            }
            0xB0 => {
                if d1 == 64 {
                    self.sustain = if d2 >= 64 { Some(ev.source) } else { None };
                }
                inst.midi(at, [status, d1, d2]);
            }
            0xA0 | 0xC0 | 0xD0 | 0xE0 => inst.midi(at, [status, d1, d2]),
            _ => {}
        }
    }

    /// Releases the keys of `source` (every source with `None`) at `offset`, and lifts its
    /// sustain pedal. Pitches the arrangement or a slot hold (`others`) keep sounding.
    pub(crate) fn release(&mut self, source: Option<u32>, inst: &mut dyn Plugin, offset: usize, others: impl Fn(u8) -> bool) {
        let mut i = 0;
        while i < self.len {
            let k = self.keys.get(i).copied().unwrap_or_default();
            if source.is_none_or(|s| s == k.source) {
                if let Some(p) = self.remove(i)
                    && !others(p)
                {
                    inst.note_off(offset, p);
                }
            } else {
                i += 1;
            }
        }
        if self.sustain.is_some_and(|s| source.is_none_or(|x| x == s)) {
            self.sustain = None;
            inst.midi(offset, [0xB0, 64, 0]);
        }
    }

    /// How long to keep rendering after a live event, in samples at `sample_rate`.
    pub(crate) fn ring_len(sample_rate: f32, inst: &dyn Plugin) -> i64 {
        let tail = i64::try_from(inst.tail_samples()).unwrap_or(i64::MAX);
        ((f64::from(sample_rate) * RING_S) as i64).saturating_add(tail)
    }
}

/// An instrument seen by the arrangement and Session view slot schedulers: their note-offs for
/// a pitch a live key holds are not sent (the live key's release sends it later).
pub(crate) struct Shield<'a> {
    pub(crate) inner: &'a mut dyn Plugin,
    pub(crate) live: &'a LiveKeys,
}

impl Plugin for Shield<'_> {
    fn info(&self) -> &'static PluginInfo {
        self.inner.info()
    }
    fn prepare(&mut self, sample_rate: f32, max_block: usize, channels: usize) {
        self.inner.prepare(sample_rate, max_block, channels);
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn set_param(&mut self, id: &str, value: f32) -> bool {
        self.inner.set_param(id, value)
    }
    fn param(&self, id: &str) -> Option<f32> {
        self.inner.param(id)
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        self.inner.process(io, frames);
    }
    fn note_on(&mut self, offset: usize, note: u8, velocity: u8) {
        self.inner.note_on(offset, note, velocity);
    }
    fn note_off(&mut self, offset: usize, note: u8) {
        if !self.live.holds(note) {
            self.inner.note_off(offset, note);
        }
    }
    fn midi(&mut self, offset: usize, msg: [u8; 3]) {
        self.inner.midi(offset, msg);
    }
    fn all_notes_off(&mut self) {
        self.inner.all_notes_off();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Log(Vec<(char, u8)>);

    impl Plugin for Log {
        fn info(&self) -> &'static PluginInfo {
            soundcraft_dsp::create("gain").unwrap().info()
        }
        fn prepare(&mut self, _: f32, _: usize, _: usize) {}
        fn reset(&mut self) {}
        fn set_param(&mut self, _: &str, _: f32) -> bool {
            false
        }
        fn param(&self, _: &str) -> Option<f32> {
            None
        }
        fn process(&mut self, _: &mut [Vec<f32>], _: usize) {}
        fn note_on(&mut self, _: usize, note: u8, _: u8) {
            self.0.push(('+', note));
        }
        fn note_off(&mut self, _: usize, note: u8) {
            self.0.push(('-', note));
        }
        fn midi(&mut self, _: usize, msg: [u8; 3]) {
            self.0.push(('m', msg[1]));
        }
    }

    fn ev(source: u32, msg: [u8; 3]) -> LiveEvent {
        LiveEvent { source, offset: 0, msg }
    }

    #[test]
    fn a_pitch_sounds_until_its_last_holder_lets_go() {
        let (mut keys, mut inst) = (LiveKeys::default(), Log::default());
        keys.play(ev(1, [0x90, 60, 100]), &mut inst, |_| false);
        keys.play(ev(2, [0x91, 60, 90]), &mut inst, |_| false);
        keys.play(ev(1, [0x80, 60, 0]), &mut inst, |_| false);
        assert_eq!(inst.0, [('+', 60), ('+', 60)], "port 2 still holds it");
        keys.play(ev(2, [0x91, 60, 0]), &mut inst, |_| false);
        assert_eq!(inst.0.last(), Some(&('-', 60)));
        assert_eq!(keys.len(), 0);
        // The arrangement holds it: the live release waits for the arrangement's.
        keys.play(ev(1, [0x90, 62, 100]), &mut inst, |_| false);
        keys.play(ev(1, [0x80, 62, 0]), &mut inst, |p| p == 62);
        assert_eq!(inst.0.last(), Some(&('+', 62)));
        // A note-off nobody played is ignored.
        keys.play(ev(3, [0x80, 64, 0]), &mut inst, |_| false);
        assert_eq!(inst.0.last(), Some(&('+', 62)));
    }

    #[test]
    fn releasing_a_source_releases_only_its_keys_and_pedal() {
        let (mut keys, mut inst) = (LiveKeys::default(), Log::default());
        keys.play(ev(1, [0x90, 60, 100]), &mut inst, |_| false);
        keys.play(ev(1, [0xB0, 64, 127]), &mut inst, |_| false);
        keys.play(ev(2, [0x90, 61, 100]), &mut inst, |_| false);
        inst.0.clear();
        keys.release(Some(1), &mut inst, 0, |_| false);
        assert_eq!(inst.0, [('-', 60), ('m', 64)]);
        assert!(keys.holds(61) && !keys.holds(60));
        keys.release(None, &mut inst, 0, |_| false);
        assert_eq!(keys.len(), 0);
    }

    #[test]
    fn a_full_table_plays_no_note_it_could_not_release() {
        let (mut keys, mut inst) = (LiveKeys::default(), Log::default());
        for i in 0..MAX_HELD + 10 {
            let (ch, pitch) = ((i / 128) as u8, (i % 128) as u8);
            keys.play(ev(9, [0x90 | ch, pitch, 100]), &mut inst, |_| false);
        }
        assert_eq!(keys.len(), MAX_HELD);
        assert_eq!(inst.0.len(), MAX_HELD);
        keys.release(None, &mut inst, 0, |_| false);
        assert_eq!(inst.0.iter().filter(|e| e.0 == '-').count(), 128, "each pitch released once");
    }

    #[test]
    fn the_shield_holds_back_note_offs_of_live_pitches() {
        let (mut keys, mut inst) = (LiveKeys::default(), Log::default());
        keys.play(ev(1, [0x90, 60, 100]), &mut inst, |_| false);
        inst.0.clear();
        let mut sh = Shield { inner: &mut inst, live: &keys };
        sh.note_off(0, 60);
        sh.note_off(0, 61);
        sh.note_on(0, 60, 1);
        assert_eq!(inst.0, [('-', 61), ('+', 60)]);
    }
}
