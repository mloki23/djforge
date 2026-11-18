"""djforge sidecar: a track's beat grid, key and loudness from mixforge's analysis.

Run as:  python -I -c <this file's text> MIXFORGE_DIR AUDIO_PATH
Prints one JSON object. Reads mixforge's library; never writes it.
The library is mixforge's (in MIXFORGE_HOME when set, as mixforge reads it).
"""
import json
import sys
from pathlib import Path


def retagged(library, path):
    """The cached analysis of a file whose size or date changed but whose audio did not.

    Tag writers (Mixed In Key) touch the file, which invalidates mixforge's cache key. The cached
    analysis still holds when its version and the audio's length are unchanged.
    """
    with library._connect() as db:
        row = db.execute("SELECT version, data FROM tracks WHERE path = ?", (str(Path(path).resolve()),)).fetchone()
    if not row or row[0] != library.ANALYSIS_VERSION:
        return None
    info = json.loads(row[1])
    length = library._duration(path)
    return info if length and abs(length - info.get("duration", 0.0)) < 0.05 else None


def main(mixforge_dir, path):
    sys.path.insert(0, mixforge_dir)
    from mixforge import library

    info, stale = library.cached(path), False
    if info is None:
        info = retagged(library, path)
        stale = info is not None
    if info is None:
        print(json.dumps({"error": f"not analysed by mixforge: run `mixforge analyze \"{path}\"`"}))
        return 2
    # Section boundaries from all-in-one, from the cache only (compute=False): never run here,
    # never written. Optional: a track without them falls back to bar-change phrases.
    try:
        library.attach_structure([info], progress=None, compute=False)
    except Exception:
        pass
    keys = ("bpm", "first_downbeat", "duration", "audio_end", "key", "camelot", "lufs", "constant_tempo", "downbeat_confidence",
            "grid_ok_4bar", "bar_change", "sections")
    out = {k: info.get(k) for k in keys}
    out.update(path=path, stale_cache=stale)
    print(json.dumps(out))
    return 0


if __name__ == "__main__":
    args = sys.argv[1:]
    if len(args) != 2:
        print(json.dumps({"error": "usage: MIXFORGE_DIR AUDIO_PATH"}))
        sys.exit(2)
    sys.exit(main(*args))
