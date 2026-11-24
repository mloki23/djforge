"""djforge sidecar for agents: mixforge's next-track suggestions, set planner and analysis.

Run as:  python -I -c <this file's text> MIXFORGE_DIR OP      (OP's arguments: JSON on stdin)
Prints JSON lines on stdout: {"progress": {...}} while it works, then one {"result": ...} or
{"error": "..."}. mixforge's own printing goes to stderr.

suggest and plan read mixforge's library; only analyze writes it (mixforge's analysis cache,
as `mixforge analyze` does). Music files are never written.
"""
import json
import sys
from pathlib import Path

OUT = sys.stdout

TRACK_FIELDS = ("path", "title", "artist", "bpm", "camelot", "key", "key_source", "energy", "duration", "constant_tempo", "lufs")


def emit(**kw):
    OUT.write(json.dumps(kw, default=str) + "\n")
    OUT.flush()


def resolved(p):
    return str(Path(p).resolve())


def brief(t):
    return {k: t.get(k) for k in TRACK_FIELDS if k in t}


def library_full():
    from mixforge.studio import library

    return library(full=True)


def op_suggest(a):
    from mixforge.suggest import suggest

    full = library_full()
    by_path = {t["path"]: t for t in full}
    current = by_path.get(resolved(a["from"]))
    if current is None:
        raise ValueError(f"not analysed by mixforge: {a['from']} (analyse it first)")
    in_set = [by_path[resolved(p)] for p in a.get("set", []) if resolved(p) in by_path]
    if all(t["path"] != current["path"] for t in in_set):
        in_set.append(current)
    return suggest(current, full, in_set, a.get("direction", "steady"), int(a.get("limit", 40)))


def collect(sources):
    from mixforge.library import read_playlist

    paths = []
    for s in sources:
        paths.extend(read_playlist(s))
    return paths


def analyse(paths, force=False, sections=True, label="analysing"):
    from mixforge.library import analyze_all

    failed = []

    def progress(done, total, result, error):
        name = Path(result.get("path", "?")).name
        if error:
            failed.append({"path": result.get("path"), "error": str(error)})
        emit(progress={"stage": label, "done": done, "total": total, "current": name, "failed": len(failed)})

    results = analyze_all(paths, force=force, progress=progress, structure_analysis=sections)
    return results, failed


def op_analyze(a):
    paths = [str(p) for p in a["paths"]]
    emit(progress={"stage": "analysing", "done": 0, "total": len(paths)})
    results, failed = analyse(paths, force=bool(a.get("force")), sections=a.get("sections", True))
    return {"analysed": len(results), "failed": failed, "tracks": [brief(r) for r in results]}


def op_plan(a):
    from mixforge.library import same_song_pairs
    from mixforge.planner import plan_mix

    paths = collect([str(s) for s in a["sources"]])
    if not paths:
        raise ValueError("no audio files in the tracks given")
    emit(progress={"stage": "analysing", "done": 0, "total": len(paths)})
    results, failed = analyse(paths, label="analysing (cached results are reused)")
    emit(progress={"stage": "planning", "tracks": len(results)})
    bpm = a.get("bpm")
    plan = plan_mix(
        results,
        order=a.get("order", "keep"),
        master_bpm=bpm,
        transition_bars=a.get("bars"),
        skip_variable_tempo=not a.get("keep_variable", False),
        tempo="master" if bpm else a.get("tempo", "ramp"),
        style=a.get("style", "auto"),
        play_minutes=a.get("minutes_per_track"),
    )
    for pair in same_song_pairs(results):
        label = "Same song" if pair["level"] == "same" else "Similar melody"
        plan["warnings"].insert(0, f"{label}? {pair['a']}  ~  {pair['b']} ({pair['why']})")
    out = Path(a["out"])
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(plan, indent=2), encoding="utf-8")
    tracks = []
    for t in plan.get("tracks", []):
        tr = t.get("transition") or {}
        # A track's transition is the one out of it into the next; times are in its own file.
        tracks.append({
            "path": t.get("path"), "title": t.get("title"), "artist": t.get("artist"), "camelot": t.get("camelot"),
            "bpm": t.get("bpm"), "play_bpm": t.get("play_bpm"), "mix_in_s": t.get("cue_in"), "mix_out_s": t.get("out_start"),
            "transition_out": {"style": tr.get("style"), "bars": tr.get("bars"), "notes": tr.get("notes")} if tr else None,
        })
    return {"plan": str(out), "master_bpm": plan.get("master_bpm"), "tempo_mode": plan.get("tempo_mode"), "tracks": tracks,
            "warnings": plan.get("warnings", []), "failed": failed, "left_out": len(results) - len(tracks)}


OPS = {"suggest": op_suggest, "analyze": op_analyze, "plan": op_plan}


def main(mixforge_dir, op):
    sys.path.insert(0, mixforge_dir)
    sys.stdout = sys.stderr  # mixforge prints as it works: that is not ours to parse
    try:
        args = json.loads(sys.stdin.read() or "{}")
        if op not in OPS:
            raise ValueError(f"unknown op {op}")
        emit(result=OPS[op](args))
        return 0
    except Exception as error:  # the error goes back to djforge, whatever it is
        emit(error=f"{type(error).__name__}: {error}")
        return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], sys.argv[2]))
