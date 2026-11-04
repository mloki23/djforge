# Stage 7a changes worth knowing

## Master true peak: every imported DJ set is affected

The Maximizer's True Peak mode now detects peaks on an 8x oversampled signal (BS.1770 Annex 2,
on a finer grid than the meter's 4x) and holds them under its ceiling. Every set made by
`dj.import_set` has this Maximizer on its master, so every imported DJ set changes:

- **Latency.** In True Peak mode the limiter adds 10 more samples of latency than before: the
  detector's filter delay is now 16 samples, up from 6. It reports this, so playback and
  bounces stay aligned.
- **Inter-sample peaks.** It now catches inter-sample peaks it used to miss, so masters come
  out near -1 dBTP measured, not only under -1 dBFS sample peak. It isn't exact yet: on the
  loki journey gate (bars 896..968), ffmpeg's ebur128 reads -0.95 dBTP, the engine's meter
  -0.93 and the gate checker's 4x estimate -0.91. That's up to 0.1 dB over the ceiling, so the
  checker's hard -0.95 dBTP check still fails.
- **Bounces differ slightly.** Loud passages get a little more gain reduction, so a bounce of an
  existing set does not match an older bounce sample for sample.
- **Prep gain steps may change.** Prep steps gain from true-peak readings, so its suggested
  gain steps for a set may come out different from before.

A known limit: content up to Nyquist (unfiltered full-band noise, for example) can still read
about 0.5 dB over the ceiling, because the detector's filter passes up to about 20 kHz. Real
48 kHz masters stop around 20 kHz. The `true_peak` test
`maximizer_true_peak_mode_on_unfiltered_white_noise` records this case and is marked ignored.

## Things that now ask before they replace

These now ask before replacing anything (`replace: true`; the app shows a Replace? offer):

- `prod.pattern` refuses when its FX sweep would overwrite `instrument:cutoff` or
  `instrument:level` points inside the bars. With `replace`, it also clears those lanes inside
  the bars when the new pattern doesn't sweep them, so an old riser stops playing under a drum
  pattern or an impact.
- `live.slot_set` refuses to shorten a MIDI slot when that would drop notes, and says how many.
  What it keeps is exactly what a saved session reloads.
- `live.slot_from_selection` and `live.slot_from_file` refuse a filled slot. That covers
  dropping a file on a filled slot and Add Selection Here.

## Other changes

- A playing MIDI slot keeps playing when you edit its notes or length (audio slots already
  did). Notes it was holding are released at the edit.
- `live.setup` picks a lost instrument column's preset from what its slots hold. It uses
  `trance_kit` when every note is a General MIDI drum on channel 10, and `rolling_bass`
  otherwise. It also names any audio slots it had to empty in a message.
