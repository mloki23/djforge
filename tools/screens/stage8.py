"""Review and README screenshots: djforge Dark and SoundCraft Classic, rendered offscreen with wgpu.

    python -I tools/screens/demo_set.py DEMO_DIR
    python -I tools/screens/stage8.py --plan DEMO_DIR/plan.json --out OUT_DIR [STEM...]
    python -I tools/screens/stage8.py --session SESSION.djforge --view VIEW.json --out OUT_DIR [STEM...]

`--plan` imports a mixforge plan (`dj.import_set`) into a blank session, saves it next to the plan
as `Demo Set.djforge` and shoots that; the transition view comes from `view.json` beside the plan
(demo_set.py writes both). `--session` opens a session in memory only (never saved): give it a
scratch copy so nothing can touch the original, and a view.json of
`{"set_start": samples, "set_end": samples, "hit": samples}` around a transition.

Each shot runs the `ui_shot` example (crates/ui-egui/examples/ui_shot.rs) in its own process at
1600x1000 points (3200x2000 pixels), waiting for the frequency bands so waveforms are coloured.
STEMs (e.g. `dark-mix`) pick shots; `--theme dark` picks a theme. The README uses dark-edit-transition,
dark-mix and dark-session-view, downscaled (and the mix cropped to its left half).
"""

import argparse
import json
import subprocess
from pathlib import Path

CARGO = str(Path.home() / ".cargo" / "bin" / "cargo")
TIMELINE_X = 316  # the Edit timeline's left edge in points at 1600x1000
TIMELINE_PX = 1030  # the Edit timeline's width in points at 1600x1000 (ui.inspect edit_layout.timeline)
ZOOM_PX = TIMELINE_PX  # the transition shot zooms its selection to the timeline's width
MARKERS_Y = 195  # the Markers ruler


def cmd(command, **params):
    return {"method": "command", "params": {"command": command, "params": params}}


def live_columns():
    """Two audio columns (one slot from Deck A) and two instrument columns holding MIDI slots."""
    return [
        cmd("live.setup", columns=2),
        cmd("edit.select", tracks=["Deck A"], start=3_000_000, end=3_700_000),
        cmd("live.slot_from_selection", column=0),
        cmd("edit.select", start=0, end=0),
        cmd("live.add_column", kind="instrument"),
        cmd("prod.pattern", kind="drums", style="trance_full", bars=4, slot={"column": 2, "slot": 0}),
        cmd("prod.pattern", kind="drums", style="fill", bars=1, slot={"column": 2, "slot": 1}),
        cmd("live.add_column", kind="instrument", preset="rolling_bass"),
        cmd("prod.pattern", kind="bass", style="rolling", key="8A", bars=4, slot={"column": 3, "slot": 0}),
        cmd("prod.pattern", kind="bass", style="offbeat", key="8A", bars=4, slot={"column": 3, "slot": 1}),
    ]


def shots(view):
    start, end, hit = view["set_start"], view["set_end"], view["hit"]
    marker = {"x": round(TIMELINE_X + (hit - start) * ZOOM_PX / (end - start)), "y": MARKERS_Y}
    return {
        "edit-transition": ([
            cmd("edit.select", tracks=["Deck A", "Deck B"], start=start, end=end),
            cmd("view.zoom_to_selection", width_px=ZOOM_PX),
            cmd("edit.select", start=start, end=start),
            {"method": "ui.move", "params": marker},
            {"steps": 30},
        ], []),
        "edit-whole-set": ([cmd("view.zoom_fit", width_px=TIMELINE_PX)], []),
        "mix": ([cmd("window.mix")], []),
        "session-view": (live_columns() + [cmd("window.session")], []),
        "setup-midi-devices": ([cmd("window.midi_devices")], ["--fake-midi", "DDJ-FLX4,Keystation 49"]),
        "about": ([cmd("window.about")], []),
        "setup-appearance": ([cmd("window.ui_customization")], []),
    }


THEMES = ["dark", "classic"]


def ui_shot(work, stem, lines, extra=()):
    script = work / f"{stem}.jsonl"
    script.write_text("\n".join(json.dumps(line) for line in lines) + "\n", encoding="utf-8")
    args = [CARGO, "run", "-j", "1", "-q", "-p", "soundcraft-ui-egui", "--example", "ui_shot", "--",
            (work / "last.png").as_posix(), str(script), "--blank", "--size", "1600x1000", *extra]
    print(f"== {stem}", flush=True)
    subprocess.run(args, check=False)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--plan", type=Path, help="a mixforge plan.json to import (view.json beside it)")
    src.add_argument("--session", type=Path, help="a session to open in memory")
    ap.add_argument("--view", type=Path, help="view.json for --session (default: beside the session)")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--theme", choices=THEMES, action="append")
    ap.add_argument("only", nargs="*", help="shot stems, e.g. dark-mix")
    a = ap.parse_args()
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    work = out / "_scripts"
    work.mkdir(exist_ok=True)
    if a.plan:
        plan = a.plan.resolve()
        session = plan.parent / "Demo Set.djforge"
        view_path = plan.parent / "view.json"
        if not session.exists():
            ui_shot(work, "import", [cmd("dj.import_set", plan=plan.as_posix()), cmd("session.save", path=session.as_posix())])
    else:
        session = a.session.resolve()
        view_path = (a.view or session.parent / "view.json").resolve()
    view = json.loads(view_path.read_text(encoding="utf-8"))
    only = set(a.only)
    for theme in a.theme or THEMES:
        for name, (steps, extra) in shots(view).items():
            stem = f"{theme}-{name}"
            if only and stem not in only:
                continue
            lines = [cmd("session.open", path=session.as_posix()), cmd("ui.theme", mode=theme), {"wait_bands": 900}]
            lines += steps + [{"steps": 10}, {"shot": (out / f"{stem}.png").as_posix()}]
            ui_shot(work, stem, lines, extra)


if __name__ == "__main__":
    main()
