"""A made-up demo DJ set for the README screenshots: four synthesised trance tracks and a mixforge plan.

    python -I tools/screens/demo_set.py OUT_DIR

Writes OUT_DIR/<title>.wav (48 kHz, 16-bit stereo, 128 bars each), OUT_DIR/plan.json (a
mixforge-plan/2 that `dj.import_set` reads: bpm and first downbeat given, so nothing needs
analysing) and OUT_DIR/view.json (where transition 1 sits, for `stage8.py --plan`).

Each track is a 4/4 kick, an offbeat bass, claps and hats, a pad and a 16th-note arp, arranged in
sections (intro, build, breakdown, drop, outro) so the frequency-coloured waveforms show their
shape. The titles and the music are invented here; nothing is sampled.
"""

import json
import sys
import wave
from pathlib import Path

import numpy as np

SR = 48_000
BARS = 128
OUT_BAR = 96  # each track mixes out from this bar (counted from its first downbeat)
RNG = np.random.default_rng(8)

# title, bpm, Camelot, key, root (Hz, the bass octave), transition into the next track
TRACKS = [
    ("Northern Lights", 132.0, "8A", "A minor", 55.00, {"style": "blend", "bars": 16, "ramp": True}),
    ("Solar Drift", 134.0, "9A", "E minor", 41.20, {"style": "quick", "bars": 8, "ramp": False}),
    ("Afterglow", 136.0, "10A", "B minor", 61.74, {"style": "blend", "bars": 16, "ramp": True}),
    ("Tidal", 138.0, "9A", "E minor", 41.20, None),
]

# Minor-key progressions, as semitones above the root: i, VI, III, VII and friends.
PROGRESSIONS = [[0, 8, 3, 10], [0, 5, 8, 7], [0, 10, 8, 7], [0, 3, 8, 10]]
MINOR = [0, 3, 7]
MAJOR = [0, 4, 7]


def layers(bar):
    """The layers playing in `bar` (0-based)."""
    if bar < 16:
        return {"kick", "hats", "arp"}
    if bar < 32:
        return {"kick", "hats", "bass", "clap"}
    if bar < 48:
        return {"kick", "hats", "bass", "clap", "arp"}
    if bar < 64:
        return {"pad", "arp"} if bar < 60 else {"pad", "arp", "hats"}
    if bar < 96:
        return {"kick", "hats", "bass", "clap", "pad", "arp"}
    if bar < 112:
        return {"kick", "hats", "bass", "clap", "arp", "pad"}
    return {"kick", "hats", "arp"}


def env(n, attack, decay):
    t = np.arange(n) / SR
    return np.minimum(t / max(attack, 1e-4), 1.0) * np.exp(-t * decay)


def additive(freq, n, harmonics, odd=False, detune=0.0):
    t = np.arange(n) / SR
    out = np.zeros(n)
    for h in range(1, harmonics + 1):
        if odd and h % 2 == 0:
            continue
        f = freq * h * (1.0 + detune)
        if f >= SR / 2:
            break
        out += np.sin(2 * np.pi * f * t + h) / h
    return out


def kick():
    n = int(0.32 * SR)
    t = np.arange(n) / SR
    freq = 48 + 110 * np.exp(-t * 28)
    body = np.sin(2 * np.pi * np.cumsum(freq) / SR) * np.exp(-t * 8)
    click = RNG.standard_normal(n) * np.exp(-t * 400) * 0.15
    return body + click


def noise_hit(n, decay, bright):
    x = RNG.standard_normal(n)
    for _ in range(bright):
        x = np.diff(x, prepend=0.0)  # each difference tilts the noise towards the highs
    return x / np.max(np.abs(x)) * env(n, 0.0005, decay)


def chord_root(progression, bar):
    return progression[bar % 4]


def place(buf, at, sound, gain):
    start = int(round(at * SR))
    end = min(start + len(sound), len(buf))
    if 0 <= start < end:
        buf[start:end] += sound[: end - start] * gain


def track_audio(index, bpm, root):
    beat = 60.0 / bpm
    bar_secs = 4 * beat
    n = int(np.ceil(BARS * bar_secs * SR)) + SR  # a second of tail
    low, mid, high, wide = np.zeros(n), np.zeros(n), np.zeros(n), np.zeros(n)
    progression = PROGRESSIONS[index % len(PROGRESSIONS)]
    k = kick()
    hat = noise_hit(int(0.05 * SR), 90, 3)
    open_hat = noise_hit(int(0.18 * SR), 22, 3)
    clap = noise_hit(int(0.2 * SR), 25, 1)
    bass_len = int(beat / 2 * 0.9 * SR)
    arp_len = int(beat / 4 * 0.95 * SR)
    pad_len = int(bar_secs * SR)
    bass_notes, arp_notes, pads = {}, {}, {}
    for bar in range(BARS):
        on = layers(bar)
        t0 = bar * bar_secs
        step = chord_root(progression, bar)
        quality = MAJOR if step in (3, 8, 10) else MINOR
        for b in range(4):
            if "kick" in on:
                place(low, t0 + b * beat, k, 0.7)
            if "clap" in on and b in (1, 3):
                place(mid, t0 + b * beat, clap, 0.45)
            if "bass" in on:
                if step not in bass_notes:
                    f = root * 2 ** (step / 12)
                    bass_notes[step] = additive(f, bass_len, 8) * env(bass_len, 0.004, 9)
                place(low, t0 + (b + 0.5) * beat, bass_notes[step], 0.3)
            if "hats" in on:
                place(high, t0 + (b + 0.5) * beat, open_hat, 0.55)
                for s in (0, 1, 3):
                    place(high, t0 + (b + s / 4) * beat, hat, 0.3 if s else 0.2)
            if "arp" in on:
                for s in range(4):
                    i = b * 4 + s
                    semis = step + quality[i % 3] + 12 * (i % 6 >= 3)
                    if semis not in arp_notes:
                        f = root * 16 * 2 ** (semis / 12)
                        arp_notes[semis] = additive(f, arp_len, 7, odd=True) * env(arp_len, 0.002, 14)
                    place(mid, t0 + (b + s / 4) * beat, arp_notes[semis], 0.3)
        if "pad" in on:
            if step not in pads:
                pad = np.zeros(pad_len)
                for q in quality:
                    f = root * 4 * 2 ** ((step + q) / 12)
                    for d in (-0.004, 0.0, 0.004):
                        pad += additive(f, pad_len, 6, detune=d)
                shape = np.minimum(np.linspace(0, 1, pad_len) * 6, 1.0) * np.minimum(np.linspace(1, 0, pad_len) * 8, 1.0)
                pads[step] = pad * shape
            place(wide, t0, pads[step], 0.12)
    left = low + mid + 0.9 * high + wide
    right = low + mid + 1.1 * high + np.roll(wide, 240)
    stereo = np.stack([left, right], axis=1)
    stereo *= 10 ** (-1 / 20) / np.max(np.abs(stereo))
    return stereo, BARS * bar_secs


def write_wav(path, stereo):
    pcm = np.clip(np.round(stereo * 32767), -32768, 32767).astype("<i2")
    with wave.open(str(path), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(pcm.tobytes())


def transition_view(bpm_out, bpm_in, entry_bar, build_bars, ramp):
    """Samples from 2 bars before transition 1's entry to 3 bars past its hit, and its hit sample.

    Bars before the entry are at `bpm_out`; a ramp then moves one step per beat over the
    recipe's build and out bars, each beat at the tempo halfway through it (as `dj.import_set`).
    """
    out_bars = build_bars // 2
    steps = (build_bars + out_bars) * 4
    beats = [60.0 / bpm_out] * ((entry_bar - 1) * 4)
    for k in range(steps):
        bpm = bpm_out + (bpm_in - bpm_out) * (k + 0.5) / steps if ramp else bpm_out
        beats.append(60.0 / bpm)
    starts = np.concatenate([[0.0], np.cumsum(beats)]) * SR

    def bar(n):
        return int(round(starts[(n - 1) * 4]))

    hit = entry_bar + build_bars
    return bar(entry_bar - 2), bar(hit + 3), bar(hit)


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    out = Path(sys.argv[1]).resolve()
    out.mkdir(parents=True, exist_ok=True)
    plan_tracks = []
    for i, (title, bpm, camelot, key, root, transition) in enumerate(TRACKS):
        stereo, music_secs = track_audio(i, bpm, root)
        path = out / f"{title}.wav"
        write_wav(path, stereo)
        print(f"wrote {path} ({len(stereo) / SR:.0f} s at {bpm:g} bpm)", flush=True)
        plan_tracks.append({
            "path": str(path), "title": title, "artist": "djforge demo", "bpm": bpm, "play_bpm": bpm,
            "first_downbeat": 0.0, "audio_end": round(music_secs, 3),
            "out_start": round(OUT_BAR * 4 * 60.0 / bpm, 3) if transition else None,
            "gain_db": 0.0, "nudge_ms": 0.0, "camelot": camelot, "key": key, "transition": transition, "fx": [],
        })
    plan = {"format": "mixforge-plan/2", "master_bpm": TRACKS[0][1], "tempo_mode": "ramp", "target_lufs": -10.0,
            "warnings": [], "tracks": plan_tracks}
    (out / "plan.json").write_text(json.dumps(plan, indent=2), encoding="utf-8")
    first = TRACKS[0][5]
    start, end, hit = transition_view(TRACKS[0][1], TRACKS[1][1], OUT_BAR + 1, first["bars"], first["ramp"])
    (out / "view.json").write_text(json.dumps({"set_start": start, "set_end": end, "hit": hit}, indent=2), encoding="utf-8")
    print(f"wrote {out / 'plan.json'} and view.json")


if __name__ == "__main__":
    main()
