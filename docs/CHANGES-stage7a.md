# Stage 7a changes worth knowing

## Master true peak: every imported DJ set is affected

The Maximizer's True Peak mode now holds the master under its ceiling measured as true peak.
Every set made by `dj.import_set` has this Maximizer on its master, so every imported DJ set
changes.

How the detector works now:
- It reads 16x oversampled through a 64-tap-per-phase filter whose passband reaches about
  23 kHz. The old 8x, 32-tap filter rolled off from about 20 kHz, and content above that
  slipped through.
- It scales its largest estimate up by a bound on the error between grid points (0.04 dB), so
  it errs high.
- It also counts the loudness meter's own 4x estimate.
- The gain a peak needs is held over every sample the estimate was made from, not just the one
  next to it.

What changes for a set:
- **Latency.** In True Peak mode the limiter adds 64 more samples (1.3 ms at 48 kHz) on top of
  its lookahead, up from 6 before stage 7a: the detector's span, twice its 32-sample delay. It
  reports this, so playback and bounces stay aligned.
- **Inter-sample peaks.** On the loki journey gate (bars 896..968), the master reads:
  - ffmpeg ebur128 per-frame maximum: -1.0005 dBTP (it was -0.954);
  - engine meter: -1.000 dBTP (was -0.935);
  - gate checker's 4x estimate: -0.97 dBTP (was -0.91);
  - a 32x long-sinc reference, at the loudest point: -1.04 dBTP.

  ffmpeg's margin is only 0.0005 dB: its filter differs a little from the meter's, which the
  limiter holds exactly at -1.000. Other material could read a hair over on ffmpeg.
- **Loudness.** Loud passages get a little more gain reduction. The gate's integrated loudness
  moved from -10.26 to -10.35 LUFS (engine meter; ffmpeg reads -10.3 → -10.4).
- **Bounces differ.** A bounce of an existing set doesn't match an older one sample for sample.
- **Prep gain steps may change.** Prep steps gain from true-peak readings, so its suggested
  gain steps for a set may come out different from before.
- **CPU.** The Maximizer costs about 122 µs per 512-sample stereo block in True Peak mode, on
  this machine in release: about 1.1% of the block's 10.7 ms. Before stage 7a it cost 102 µs.
- **Full-band noise.** Unfiltered white noise, with content right up to Nyquist, now stays
  under the ceiling too: -1.00 / -0.98 dBTP against a 16x long-sinc reference, where the old
  detector let it reach -0.52.

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
