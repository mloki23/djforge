"""Screenshots of the Decks tab (Window › Decks): two synthetic demo tracks loaded, playing
mid-track, Deck B synced to Deck A, a loop on Deck A and hot cues on both, in djforge Dark and
SoundCraft Classic, rendered offscreen with wgpu.

    python -I tools/screens/demo_set.py DEMO_DIR
    python -I tools/screens/decks.py --demo DEMO_DIR --out OUT_DIR [--out OUT_DIR2] [--theme dark]

Each shot runs the `ui_shot` example with `--decks` (a silent audio engine: no device is opened)
on a blank session. The tracks are loaded with their tempo given (`bpm`, `first_beat`), so no
library or analysis is read. Writes OUT_DIR/<theme>-decks.png (3200x2000 pixels).
"""

import argparse
import json
import shutil
import subprocess
from pathlib import Path

CARGO = str(Path.home() / ".cargo" / "bin" / "cargo")
THEMES = ["dark", "classic"]
# (file, bpm) of the two demo tracks demo_set.py writes.
TRACKS = [("Northern Lights.wav", 132.0), ("Solar Drift.wav", 134.0)]


def cmd(command, **params):
    return {"method": "command", "params": {"command": command, "params": params}}


def script(demo):
    (a, abpm), (b, bbpm) = TRACKS
    lines = [
        cmd("window.decks"),
        cmd("deck.load", path=(demo / a).as_posix(), deck="a", bpm=abpm, first_beat=0.0),
        cmd("deck.load", path=(demo / b).as_posix(), deck="b", bpm=bbpm, first_beat=0.0),
        {"wait_decks": 120},
        {"wait_bands": 300},
    ]
    # Hot cues: A–D on Deck A at bars 17, 33, 49, 97; A–C on Deck B.
    for deck, bpm, bars in [("a", abpm, [16, 32, 48, 96]), ("b", bbpm, [16, 64, 96])]:
        for n, bar in enumerate(bars, start=1):
            lines += [cmd("deck.seek", deck=deck, at=bar * 4 * 60.0 / bpm), cmd("deck.hotcue", deck=deck, n=n, action="set")]
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
    for theme in a.theme or THEMES:
        stem = f"{theme}-decks"
        shot = outs[0] / f"{stem}.png"
        lines = [cmd("ui.theme", mode=theme)] + script(demo) + [{"shot": shot.as_posix()}]
        path = work / f"{stem}.jsonl"
        path.write_text("\n".join(json.dumps(line) for line in lines) + "\n", encoding="utf-8")
        args = [CARGO, "run", "-j", "1", "-q", "-p", "soundcraft-ui-egui", "--example", "ui_shot", "--",
                (work / "last.png").as_posix(), str(path), "--blank", "--decks", "--size", "1600x1000"]
        print(f"== {stem}", flush=True)
        subprocess.run(args, check=False)
        for o in outs[1:]:
            if shot.exists():
                shutil.copyfile(shot, o / shot.name)


if __name__ == "__main__":
    main()
