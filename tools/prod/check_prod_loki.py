"""Stage 7a gate checks for tools/prod/prod_loki.cmds (read-only).

    python -I tools/prod/check_prod_loki.py "<produce folder>"

Reads, from the folder: `loki produce.scraft`, `loki produce.wav` (the mix, bars 896..968) and
`loki produce (parts only).wav` (the same window with Deck A/B muted). Needs numpy and scipy.
"""

import json
import sys
import wave
from pathlib import Path

import numpy as np
from scipy.signal import resample_poly

# Bar starts on the set's tempo ramp at 48 kHz (as in prod_loki.cmds); the bounces start on bar 896.
BARS = {896: 77361445, 910: 78555961, 925: 79827044, 926: 79911166, 927: 79995212, 930: 80246894, 962: 82918801, 968: 83419671}
ORIGIN = BARS[896]
# F minor (4A): F G Ab Bb C Db Eb.
FM_PCS = {5, 7, 8, 10, 0, 1, 3}
NAMES = ["C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B"]


def read_wav(path):
    with wave.open(str(path), "rb") as w:
        ch, width, rate, n = w.getnchannels(), w.getsampwidth(), w.getframerate(), w.getnframes()
        raw = w.readframes(n)
    if width == 3:
        b = np.frombuffer(raw, dtype=np.uint8).reshape(-1, 3)
        x = (b[:, 0].astype(np.int32) | (b[:, 1].astype(np.int32) << 8) | (b[:, 2].astype(np.int32) << 16))
        x = np.where(x >= 1 << 23, x - (1 << 24), x).astype(np.float64) / (1 << 23)
    elif width == 2:
        x = np.frombuffer(raw, dtype="<i2").astype(np.float64) / (1 << 15)
    else:
        raise SystemExit(f"{path}: unsupported sample width {width}")
    return rate, x.reshape(-1, ch)


def db(v):
    return 20 * np.log10(max(v, 1e-12))


def true_peak(x):
    # 4x oversampling (as BS.1770), in overlapping blocks to bound memory.
    peak, block, pad = 0.0, 48000 * 10, 64
    for c in range(x.shape[1]):
        for i in range(0, len(x), block):
            seg = x[max(0, i - pad) : i + block + pad, c]
            peak = max(peak, float(np.max(np.abs(resample_poly(seg, 4, 1)))))
    return peak


def segments(name, x):
    print(f"{name}: RMS / peak per bar range")
    keys = sorted(BARS)
    out = {}
    for a, b in zip(keys, keys[1:]):
        s, e = BARS[a] - ORIGIN, BARS[b] - ORIGIN
        seg = x[s:e]
        rms, pk = float(np.sqrt(np.mean(seg**2))), float(np.max(np.abs(seg)))
        out[(a, b)] = (rms, pk)
        print(f"  bars {a}..{b}: rms {db(rms):7.1f} dBFS, peak {db(pk):7.1f} dBFS")
    return out


def main():
    folder = Path(sys.argv[1])
    ok = True
    s = json.loads((folder / "loki produce.scraft").read_text(encoding="utf-8"))
    tracks = {t["name"]: t for t in s["tracks"]}

    def clips(name):
        t = tracks[name]
        pl = t["playlists"][t["active_playlist"]]
        return pl["clips"]

    # 1. Audio.
    rate, mix = read_wav(folder / "loki produce.wav")
    _, parts = read_wav(folder / "loki produce (parts only).wav")
    sp, tp = db(float(np.max(np.abs(mix)))), db(true_peak(mix))
    print(f"mix: {len(mix) / rate:.2f} s, sample peak {sp:.2f} dBFS, true peak {tp:.2f} dBTP")
    # Sample peak is the gate's proxy for the -1 dBTP ceiling; the true peak is reported as a
    # warning (the engine's own bounce meter under-reads it).
    ok &= sp <= -0.99
    if tp > -1.0:
        print(f"WARNING: mix true peak {tp:.2f} dBTP is above -1 dBTP")
    print(f"parts only: sample peak {db(float(np.max(np.abs(parts)))):.2f} dBFS, true peak {db(true_peak(parts)):.2f} dBTP")
    seg = segments("parts only", parts)
    segments("mix", mix)
    # The bounce is dithered 24-bit, so "silent" is dither: below -120 dBFS.
    silent = db(seg[(896, 910)][1]) < -120
    print(f"parts silent (dither only) before bar 910 (drum clip start): {silent} (peak {db(seg[(896, 910)][1]):.1f} dBFS)")
    ok &= silent
    for r in [(910, 925), (925, 926), (926, 927), (930, 962)]:
        audible = db(seg[r][0]) > -40
        print(f"parts audible over bars {r[0]}..{r[1]}: {audible}")
        ok &= audible
    first = int(np.argmax(np.max(np.abs(parts), axis=1) > 1e-4))
    print(f"first parts sample above -80 dBFS: {first + ORIGIN} (bar 910 = {BARS[910]}, {first + ORIGIN - BARS[910]} samples after)")

    # 2. Bass notes in key.
    for c in clips("Bass 1"):
        notes = c["content"]["Midi"]["sequence"]["notes"]
        pcs = sorted({n["pitch"] % 12 for n in notes})
        good = set(pcs) <= FM_PCS
        print(f"bass clip {c['id']} '{c['name']}': {len(notes)} notes, pitches {sorted({n['pitch'] for n in notes})}, pitch classes {[NAMES[p] for p in pcs]}, all in Fm: {good}")
        ok &= good
    for col in s["live"]["columns"]:
        for sl in col["slots"]:
            if sl and "sequence" in sl and col["track"] == tracks["Live 6"]["id"]:
                pcs = sorted({n["pitch"] % 12 for n in sl["sequence"]["notes"]})
                good = set(pcs) <= FM_PCS
                print(f"bass slot '{sl['name']}': pitch classes {[NAMES[p] for p in pcs]}, all in Fm: {good}")
                ok &= good

    # 3. Riser automation peaks on the hit bar.
    hit = BARS[926]
    for lane in tracks["FX 1"]["automation"]:
        param = lane["param"]["Plugin"]["param"] if "Plugin" in lane["param"] else str(lane["param"])
        # The sweep's own points (bar 910 to the hit); the anchors one sample either side hold the
        # part's setting outside the bars.
        pts = [p for p in lane["points"] if BARS[910] <= p["at"] <= hit]
        top = max(pts, key=lambda p: p["value"])
        around = [p for p in lane["points"] if abs(p["at"] - BARS[910]) <= 1 or abs(p["at"] - hit) <= 1]
        print(f"riser {param}: {len(pts)} sweep points, {pts[0]['value']:.3f} at bar 910 -> peak {top['value']:.3f} at {top['at']} (hit bar {hit}, off by {top['at'] - hit}); edges {around}")
        ok &= top["at"] == hit

    # 4. Session view.
    for col in s["live"]["columns"]:
        t = next(x for x in s["tracks"] if x["id"] == col["track"])
        midi = [sl["name"] for sl in col["slots"] if sl and "sequence" in sl]
        print(f"column track {t['name']} ({t['kind']}): MIDI slots {midi}")
    inst = [c for c in s["live"]["columns"] if next(x for x in s["tracks"] if x["id"] == c["track"])["kind"] == "Instrument"]
    two = len(inst) == 2 and all(any(sl and "sequence" in sl for sl in c["slots"]) for c in inst)
    print(f"two instrument columns with MIDI slots: {two}")
    ok &= two
    print("ALL CHECKS PASS" if ok else "SOME CHECKS FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
