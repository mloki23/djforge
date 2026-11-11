#!/usr/bin/env python3
"""Draw djforge's app icon and write every file in assets/app-icon/.

The icon is four yellow (#f8d200) waveform bars on a dark (#111317) rounded square. The geometry
lives here once: it is written out as `icon.svg` (the canonical vector) and drawn into every PNG
with Pillow, so no SVG renderer is needed. Each bitmap is drawn at its own size with the bars
snapped to whole pixels, so the 16 px icon stays crisp, and supersampled for smooth corners.

Needs Python 3 and Pillow (`pip install pillow`). The outputs are committed, so builds and
packaging never run this.

    python tools/icon/make_icon.py
"""

import io
import struct
import sys
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "assets" / "app-icon"
APP_ID = "ai.storyteller.soundcraft"  # Linux app id: the hicolor file names follow it.

BG = "#111317"
FG = "#f8d200"

# The artwork, on a 512 x 512 grid.
GRID = 512
CORNER = 112  # rounded-square radius
BAR_W = 64
GAP = 40
BAR_HEIGHTS = (176, 328, 248, 136)  # a stylised waveform, left to right
SUPERSAMPLE = 8
# macOS icons sit on Apple's grid: an 824/1024 body with a transparent margin.
MAC_BODY = 824 / 1024


def layout(size: float, snap: bool):
    """Bars as (x0, y0, x1, y1) rectangles in a `size` px tile, snapped to whole pixels if asked."""
    s = size / GRID
    rnd = (lambda v: float(round(v))) if snap else (lambda v: v)
    w = max(1.0, rnd(BAR_W * s))
    g = max(1.0, rnd(GAP * s))
    total = len(BAR_HEIGHTS) * w + (len(BAR_HEIGHTS) - 1) * g
    x = rnd((size - total) / 2)
    bars = []
    for h in BAR_HEIGHTS:
        hh = max(w, rnd(h * s))
        if snap and (int(hh) % 2) != (int(size) % 2):
            hh += 1  # same parity as the tile, so the bar centres exactly
        y = (size - hh) / 2
        bars.append((x, y, x + w, y + hh))
        x += w + g
    return bars, w


def svg() -> str:
    bars, w = layout(GRID, snap=False)
    r = w / 2
    rects = "".join(
        f'<rect x="{x0:g}" y="{y0:g}" width="{x1 - x0:g}" height="{y1 - y0:g}" rx="{r:g}" fill="{FG}"/>' for x0, y0, x1, y1 in bars
    )
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {GRID} {GRID}"><title>djforge</title>'
        f'<rect width="{GRID}" height="{GRID}" rx="{CORNER}" fill="{BG}"/>{rects}</svg>\n'
    )


def tile(size: int) -> Image.Image:
    """The full-bleed icon at `size` px."""
    big = size * SUPERSAMPLE
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle((0, 0, big - 1, big - 1), radius=round(CORNER * big / GRID), fill=BG)
    bars, w = layout(size, snap=True)
    # Round caps only where there are pixels to round; tiny bars stay sharp rectangles.
    radius = (w / 2) * SUPERSAMPLE if w >= 4 else 0
    for x0, y0, x1, y1 in bars:
        box = tuple(round(v * SUPERSAMPLE) for v in (x0, y0, x1, y1))
        d.rounded_rectangle((box[0], box[1], box[2] - 1, box[3] - 1), radius=radius, fill=FG)
    return img.resize((size, size), Image.LANCZOS)


def mac(size: int) -> Image.Image:
    """The icon on Apple's grid: the body centred with a transparent margin."""
    body = round(size * MAC_BODY)
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    img.paste(tile(body), ((size - body) // 2, (size - body) // 2))
    return img


def png(img: Image.Image) -> bytes:
    b = io.BytesIO()
    img.save(b, "PNG", optimize=True)
    return b.getvalue()


def ico(images) -> bytes:
    """A Windows .ico holding PNG entries (read by Vista and later); 256 px is stored as 0."""
    pngs = [png(i) for i in images]
    out = struct.pack("<HHH", 0, 1, len(pngs))
    offset = 6 + 16 * len(pngs)
    for img, data in zip(images, pngs):
        dim = 0 if img.width == 256 else img.width
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    return out + b"".join(pngs)


def main() -> int:
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "icon.svg").write_text(svg(), encoding="utf-8")
    (OUT / "djforge-1024.png").write_bytes(png(tile(1024)))
    # Runtime window/Dock icon on macOS (embedded by apps/soundcraft); elsewhere hicolor 256.
    (OUT / "djforge-macos-512.png").write_bytes(png(mac(512)))
    for s in (16, 24, 32, 48, 64, 128, 256, 512):
        d = OUT / "hicolor" / f"{s}x{s}" / "apps"
        d.mkdir(parents=True, exist_ok=True)
        (d / f"{APP_ID}.png").write_bytes(png(tile(s)))
    scalable = OUT / "hicolor" / "scalable" / "apps"
    scalable.mkdir(parents=True, exist_ok=True)
    (scalable / f"{APP_ID}.svg").write_text(svg(), encoding="utf-8")
    (OUT / "djforge.ico").write_bytes(ico([tile(s) for s in (16, 20, 24, 32, 40, 48, 64, 128, 256)]))
    # Pillow writes the .icns from the 1024 image plus the exact renders of the smaller sizes.
    icns = [mac(s) for s in (16, 32, 64, 128, 256, 512)]
    mac(1024).save(OUT / "djforge.icns", format="ICNS", append_images=icns)
    print(f"icons written to {OUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
