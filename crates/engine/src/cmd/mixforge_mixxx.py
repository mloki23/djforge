"""djforge sidecar: write an exported set into Mixxx's library through mixforge's exporter.

Run as:  python -I -c <this file's text> MIXFORGE_DIR SET_NAME
Reads mixforge's plan ({"tracks": [...]}, JSON in UTF-8) on standard input. Prints one JSON
object: mixforge's summary, or {error}. mixforge refuses while Mixxx runs and backs up the
database before writing.
"""
import json
import sys


def main(mixforge_dir, set_name):
    try:
        plan = json.loads(sys.stdin.buffer.read().decode("utf-8"))
        sys.path.insert(0, mixforge_dir)
        from mixforge import mixxx

        summary = mixxx.export(plan, set_name)
    except Exception as e:  # every failure is reported, as {error}
        print(json.dumps({"error": str(e) or type(e).__name__}))
        return 1
    print(json.dumps(summary))
    return 0


if __name__ == "__main__":
    args = sys.argv[1:]
    if len(args) != 2:
        print(json.dumps({"error": "usage: MIXFORGE_DIR SET_NAME"}))
        sys.exit(2)
    sys.exit(main(*args))
