#!/usr/bin/env bash
# Regenerate every app icon in assets/app-icon/ (the SVG, PNGs, .ico, .icns and hicolor tree).
#
# The icon is drawn by tools/icon/make_icon.py with Pillow (pip install pillow); no SVG renderer
# or iconutil is needed. The outputs are committed, so builds and packaging never need it.
#
#   packaging/icons.sh
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec "${PYTHON:-python3}" "$ROOT/tools/icon/make_icon.py"
