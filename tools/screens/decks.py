"""Screenshots of the Decks tab (Window › Decks): two synthetic demo tracks loaded, playing
mid-track, Deck B synced to Deck A, a loop on Deck A and hot cues on both, in djforge Dark and
SoundCraft Classic, rendered offscreen with wgpu.

    python -I tools/screens/demo_set.py DEMO_DIR
    python -I tools/screens/decks.py --demo DEMO_DIR --out OUT_DIR [--out OUT_DIR2] [--theme dark]

Each shot runs the `ui_shot` example with `--decks` (a silent audio engine: no device is opened)
on a blank session, reading a small mixforge-style library written here for the two demo tracks
(their grid, key and sections), so the decks show mixforge's cues and sections as they would for
analysed music. Writes OUT_DIR/<theme>-decks.png (3200x2000 pixels).
"""

import argparse
import json
import shutil
import sqlite3
import subprocess
import wave
from pathlib import Path

CARGO = str(Path.home() / ".cargo" / "bin" / "cargo")
THEMES = ["dark", "classic"]
# (file, bpm) of the two demo tracks demo_set.py writes.
TRACKS = [("Northern Lights.wav", 132.0), ("Solar Drift.wav", 134.0)]
KEYS = ["8A", "9A"]
# Sections in bars from the first downbeat (all-in-one's labels).
SECTIONS = [(0, 16, "intro"), (16, 32, "build"), (32, 64, "drop"), (64, 80, "break"), (80, 112, "drop"), (112, 10_000, "outro")]


def library(demo, db):
    """A mixforge library (its `tracks` and `structure` tables) for the demo tracks."""
    if db.exists():
        db.unlink()
    c = sqlite3.connect(db)
    c.executescript(
        "CREATE TABLE tracks (path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, version INTEGER, data TEXT);"
        "CREATE TABLE structure (path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, data TEXT);"
    )
    for (name, bpm), key in zip(TRACKS, KEYS):
        path = demo / name
        st = path.stat()
        with wave.open(str(path)) as w:
            secs = w.getnframes() / w.getframerate()
        bar = 240.0 / bpm
        data = {"path": str(path), "title": path.stem, "artist": "djforge demo", "bpm": bpm, "first_downbeat": 0.0,
                "duration": secs, "audio_end": secs, "camelot": key, "bars": int(secs / bar), "constant_tempo": True}
        segs = [{"start": a * bar, "end": min(b * bar, secs), "label": l} for a, b, l in SECTIONS if a * bar < secs]
        c.execute("INSERT INTO tracks VALUES (?, ?, ?, 9, ?)", (str(path), st.st_size, int(st.st_mtime), json.dumps(data)))
        c.execute("INSERT INTO structure VALUES (?, ?, ?, ?)", (str(path), st.st_size, int(st.st_mtime), json.dumps({"segments": segs})))
    c.commit()
    c.close()


def cmd(command, **params):
    return {"method": "command", "params": {"command": command, "params": params}}


def script(demo):
    (a, abpm), (b, bbpm) = TRACKS
    lines = [
        cmd("window.decks"),
        cmd("deck.load", path=str(demo / a), deck="a"),
        cmd("deck.load", path=str(demo / b), deck="b"),
        {"wait_decks": 120},
        {"wait_bands": 300},
    ]
    # Hot cues A–D are mixforge's mix points; E is set by hand on a section start (named for it).
    for deck, bpm, bar in [("a", abpm, 64), ("b", bbpm, 32)]:
        lines += [cmd("deck.seek", deck=deck, at=bar * 4 * 60.0 / bpm), cmd("deck.hotcue", deck=deck, n=5, action="set")]
    lines += [
        cmd("deck.seek", deck="a", at=80 * 4 * 60.0 / abpm),
        cmd("deck.seek", deck="b", at=72 * 4 * 60.0 / bbpm),
        cmd("deck.play", deck="a"),
        cmd("deck.play", deck="b"),
        cmd("deck.sync", deck="b", on=True),
        cmd("deck.loop", deck="a", beats=8),
        cmd("deck.eq", deck="b", band="low", db="kill"),
        cmd("deck.xfader", value=-0.2),
        {"steps": 60},
    ]
    return lines


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--demo", type=Path, required=True, help="the folder demo_set.py wrote")
    ap.add_argument("--out", type=Path, action="append", required=True, help="where the PNGs go (repeat for copies)")
    ap.add_argument("--theme", choices=THEMES, action="append")
    a = ap.parse_args()
    demo = a.demo.resolve()
    outs = [o.resolve() for o in a.out]
    for o in outs:
        o.mkdir(parents=True, exist_ok=True)
    work = outs[0] / "_scripts"
    work.mkdir(exist_ok=True)
    db = work / "library.db"
    library(demo, db)
    for theme in a.theme or THEMES:
        stem = f"{theme}-decks"
        shot = outs[0] / f"{stem}.png"
        lines = [cmd("ui.theme", mode=theme)] + script(demo) + [{"shot": shot.as_posix()}]
        path = work / f"{stem}.jsonl"
        path.write_text("\n".join(json.dumps(line) for line in lines) + "\n", encoding="utf-8")
        args = [CARGO, "run", "-j", "1", "-q", "-p", "soundcraft-ui-egui", "--example", "ui_shot", "--",
                (work / "last.png").as_posix(), str(path), "--blank", "--decks", "--size", "1600x1000", "--library", str(db)]
        print(f"== {stem}", flush=True)
        subprocess.run(args, check=False)
        for o in outs[1:]:
            if shot.exists():
                shutil.copyfile(shot, o / shot.name)


if __name__ == "__main__":
    main()
