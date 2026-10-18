"""Independent check of a transition's alignment from two solo renders (session time from 0).

python tools/dj/suggest_check.py SOLO_A.wav SOLO_B.wav --bpm-a 137.987 --bpm-b 145 --bar 250 --ramp-bars 32
Band-matched, beat-folded onset profiles over the overlap (the prototype dj.suggest_nudge is
built from). Prints how late B sits against A, overall and per band; 0 means aligned.
"""
import argparse

import numpy as np
import soundfile as sf
from scipy.signal import butter, sosfilt

p = argparse.ArgumentParser()
p.add_argument("a")
p.add_argument("b")
p.add_argument("--bpm-a", type=float, required=True)
p.add_argument("--bpm-b", type=float, required=True)
p.add_argument("--bar", type=int, required=True)
p.add_argument("--ramp-bars", type=int, required=True)
args = p.parse_args()

sr, BINS, HOP = 48000, 128, 48
r0, r1 = (args.bar - 1) * 4, (args.bar - 1 + args.ramp_bars) * 4
t = [0.0]
for j in range(r1 + 400):
    bpm = args.bpm_a if j < r0 else (args.bpm_a + (args.bpm_b - args.bpm_a) * ((j - r0) + 0.5) / (r1 - r0) if j < r1 else args.bpm_b)
    t.append(t[-1] + 60 / bpm)
t = np.array(t)
BANDS = {"low": butter(4, 150, "lowpass", fs=sr, output="sos"),
         "mid": butter(2, [300, 3000], "bandpass", fs=sr, output="sos"),
         "high": butter(4, 5000, "highpass", fs=sr, output="sos")}


def profiles(path, b0, b1):
    s0, s1 = int((t[b0] - 0.5) * sr), int((t[b1] + 0.5) * sr)
    y, _ = sf.read(path, start=s0, stop=s1, always_2d=True)
    y = y.mean(1)
    out = {}
    for name, sos in BANDS.items():
        x = sosfilt(sos, y)
        e = np.sqrt(np.add.reduceat(x * x, np.arange(0, len(x), HOP)) / HOP + 1e-12)
        e = np.convolve(e, np.ones(10) / 10, mode="same")  # ~10 ms smoothing, as djforge does
        flux = np.maximum(0.0, np.diff(np.log(e), prepend=np.log(e[0])))
        times = s0 / sr + np.arange(len(flux)) * HOP / sr
        prof = np.zeros(BINS)
        for j in range(b0, b1):
            m = (times >= t[j]) & (times < t[j + 1])
            ph = ((times[m] - t[j]) / (t[j + 1] - t[j]) * BINS).astype(int) % BINS
            np.add.at(prof, ph, flux[m])
        prof -= prof.mean()
        out[name] = prof / (np.linalg.norm(prof) + 1e-12)
    return out


def late_ms(b0, b1):
    pa, pb = profiles(args.a, b0, b1), profiles(args.b, b0, b1)
    beat_ms = (t[b1] - t[b0]) / (b1 - b0) * 1000
    lags = np.arange(-BINS // 2, BINS // 2 + 1)
    total = np.zeros(len(lags))
    per = {}
    for name in BANDS:
        c = np.array([np.dot(pa[name], np.roll(pb[name], -L)) for L in lags])
        total += c
        per[name] = lags[int(np.argmax(c))] * beat_ms / BINS
    return lags[int(np.argmax(total))] * beat_ms / BINS, per


# The overlap: from B's entry to the end of the ramp (the crossfade spans at most this).
for label, b0, b1 in [("whole overlap", r0, r1), ("first half", r0, (r0 + r1) // 2), ("second half", (r0 + r1) // 2, r1)]:
    ms, per = late_ms(b0, b1)
    bands = "  ".join(f"{k} {v:+6.1f}" for k, v in per.items())
    print(f"{label:13s} B late by {ms:+6.1f} ms   ({bands})")
