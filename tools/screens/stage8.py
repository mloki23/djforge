"""Stage 8 review screenshots: djforge Dark and SoundCraft Classic, rendered offscreen with wgpu.

    python -I tools/screens/stage8.py SESSION.scraft OUT_DIR

SESSION is opened in memory only (never saved); give it a scratch copy so nothing can touch the
original. Each shot runs the `ui_shot` example (crates/ui-egui/examples/ui_shot.rs) in its own
process at 1600x1000 points (3200x2000 pixels), waiting for the frequency bands so waveforms are
coloured. The positions below suit loki journey: transition 1 sits over bars 274-282.
"""

import json
import subprocess
import sys
from pathlib import Path

CARGO = str(Path.home() / ".cargo" / "bin" / "cargo")
SET_START, SET_END = 23_500_000, 25_200_000  # samples around loki journey's transition 1
MARKER = {"x": 1097, "y": 195}  # its end marker on the Markers ruler at that zoom
TIMELINE_PX = 1030  # the Edit timeline's width in points at 1600x1000 (ui.inspect edit_layout.timeline)


def cmd(command, **params):
    return {"method": "command", "params": {"command": command, "params": params}}


def live_columns():
    """Two audio columns (one slot from Deck A) and two instrument columns holding MIDI slots."""
    return [
        cmd("live.setup", columns=2),
        cmd("edit.select", tracks=[1], start=3_000_000, end=3_700_000),
        cmd("live.slot_from_selection", column=0),
        cmd("edit.select", start=0, end=0),
        cmd("live.add_column", kind="instrument"),
        cmd("prod.pattern", kind="drums", style="trance_full", bars=4, slot={"column": 2, "slot": 0}),
        cmd("prod.pattern", kind="drums", style="fill", bars=1, slot={"column": 2, "slot": 1}),
        cmd("live.add_column", kind="instrument", preset="rolling_bass"),
        cmd("prod.pattern", kind="bass", style="rolling", key="8A", bars=4, slot={"column": 3, "slot": 0}),
        cmd("prod.pattern", kind="bass", style="offbeat", key="8A", bars=4, slot={"column": 3, "slot": 1}),
    ]


SHOTS = {
    "edit-transition": ([
        cmd("edit.select", tracks=[1, 2], start=SET_START, end=SET_END),
        cmd("view.zoom_to_selection", width_px=1300),
        cmd("edit.select", start=SET_START, end=SET_START),
        {"method": "ui.move", "params": MARKER},
        {"steps": 30},
    ], []),
    "edit-whole-set": ([cmd("view.zoom_fit", width_px=TIMELINE_PX)], []),
    "mix": ([cmd("window.mix")], []),
    "session-view": (live_columns() + [cmd("window.session")], []),
    "setup-midi-devices": ([cmd("window.midi_devices")], ["--fake-midi", "DDJ-FLX4,Keystation 49"]),
    "about": ([cmd("window.about")], []),
    "setup-appearance": ([cmd("window.ui_customization")], []),
}

THEMES = {"dark": "dark", "classic": "classic"}


def main():
    session, out = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    out.mkdir(parents=True, exist_ok=True)
    work = out / "_scripts"
    work.mkdir(exist_ok=True)
    only = set(sys.argv[3:])
    for theme in THEMES:
        for name, (steps, extra) in SHOTS.items():
            stem = f"{theme}-{name}"
            if only and stem not in only:
                continue
            lines = [cmd("session.open", path=session.as_posix()), cmd("ui.theme", mode=theme), {"wait_bands": 900}]
            lines += steps + [{"steps": 10}, {"shot": (out / f"{stem}.png").as_posix()}]
            script = work / f"{stem}.jsonl"
            script.write_text("\n".join(json.dumps(line) for line in lines) + "\n", encoding="utf-8")
            args = [CARGO, "run", "-j", "1", "-q", "-p", "soundcraft-ui-egui", "--example", "ui_shot", "--",
                    (work / "last.png").as_posix(), str(script), "--blank", "--size", "1600x1000", *extra]
            print(f"== {stem}", flush=True)
            subprocess.run(args, check=False)


if __name__ == "__main__":
    main()
