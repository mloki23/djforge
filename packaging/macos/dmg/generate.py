#!/usr/bin/env python3
"""Regenerate background.tiff and dmg-layout.DS_Store for the macOS DMG window (see README.md here).

Needs:
    pip install 'pillow>=12' ds_store==1.3.3 mac_alias==2.2.3

Runs on any OS, no Mac needed. The background is drawn here with Pillow (djforge's dark field,
a light band under the two icons so Finder's black labels stay readable, and the djforge icon
from tools/icon/make_icon.py), so no SVG renderer is needed. Both outputs come from this script
alone, so they carry nothing from the machine that made them: no local paths, user or disk
names, dates or volume UUIDs.

    python3 packaging/macos/dmg/generate.py
"""

import datetime
import os
import sys

from ds_store import DSStore
from mac_alias import Alias, TargetInfo, VolumeInfo
from PIL import Image, ImageCms, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "..", "tools", "icon"))
import make_icon  # noqa: E402  (djforge's icon geometry and colours)

# Keep in sync with package.sh (volume name) and README.md (layout).
VOLUME = "djforge"
WIDTH, HEIGHT = 660, 400  # window content, pt; the background's 1x size
TITLE_BAR = 32
ICON_SIZE = 128
PALETTE = 16  # colours in background.tiff
ICONS = {"djforge.app": (326, 205), "Applications": (574, 205)}
# A fixed date for the alias and the colour profile. Finder never matches it against the image
# (each build is a new volume): it finds the background by volume name and path.
FIXED_DATE = datetime.datetime(2026, 10, 8, tzinfo=datetime.timezone.utc)
BAND = "#e6e8ec"  # light band behind the icons and their labels


def render(scale):
    """The background at `scale` (1 or 2): the dark field, the band and the djforge icon."""
    w, h = WIDTH * scale, HEIGHT * scale
    im = Image.new("RGB", (w, h), make_icon.BG)
    d = ImageDraw.Draw(im)
    # The band: from above the icons to below their labels, rounded, inset from the edges.
    top, bottom = (205 - ICON_SIZE // 2 - 22) * scale, (205 + ICON_SIZE // 2 + 46) * scale
    d.rounded_rectangle((240 * scale, top, 648 * scale, bottom), radius=24 * scale, fill=BAND)
    # An arrow from the app to Applications, dark on the band.
    y = 205 * scale
    d.line((410 * scale, y, 488 * scale, y), fill=make_icon.BG, width=6 * scale)
    d.polygon([(500 * scale, y), (484 * scale, y - 12 * scale), (484 * scale, y + 12 * scale)], fill=make_icon.BG)
    # The djforge icon on the left, on the dark field.
    icon = make_icon.tile(112 * scale)
    im.paste(icon, (48 * scale, (h - icon.height) // 2), icon)
    return im


def quantize(im):
    # No dithering: dither noise compresses badly and the artwork has flat fills.
    return im.quantize(PALETTE, method=Image.Quantize.FASTOCTREE, dither=Image.Dither.NONE)


def srgb_profile():
    # LittleCMS's built-in sRGB ("No copyright, use freely"), with its creation date fixed.
    icc = bytearray(ImageCms.ImageCmsProfile(ImageCms.createProfile("sRGB")).tobytes())
    d = FIXED_DATE
    icc[24:36] = b"".join(v.to_bytes(2, "big") for v in (d.year, d.month, d.day, 0, 0, 0))
    if icc[84:100] != bytes(16):
        sys.exit("error: the sRGB profile has an ID; recompute it after fixing the date")
    return bytes(icc)


def write_tiff(out):
    """1x (72 dpi) and 2x (144 dpi) in one TIFF, as `tiffutil -cathidpicheck` writes them."""
    one, two = (quantize(render(s)) for s in (1, 2))
    two.encoderinfo = {"dpi": (144, 144)}  # the rest as the first page
    # Opaque 16-colour palette, Deflate: the artwork is a few flat colours plus antialiasing, and a palette
    # keeps the file under ~100 KB (repository assets must stay small). As PhotoCraft's.
    one.save(
        out,
        "TIFF",
        save_all=True,
        append_images=[two],
        compression="tiff_adobe_deflate",  # brand-ok: Pillow's name for TIFF compression 8 (Deflate)
        dpi=(72, 72),
        icc_profile=srgb_profile(),
    )
    with Image.open(out) as im:
        im.seek(1)
        if im.info.get("dpi") != (144, 144):
            sys.exit("error: the 2x page isn't 144 dpi (needs Pillow 12 or later)")
        strips = []
        for page in (0, 1):
            im.seek(page)
            strips += zip(im.tag_v2[273], im.tag_v2[279])  # StripOffsets, StripByteCounts
    # TIFF keeps directories and tag data on even offsets. When a strip ends on an odd one, libtiff
    # leaves the padding byte after it uninitialised, so the file changed from run to run with
    # identical pixels. Zero it: nothing refers to that byte.
    starts = {o for o, _ in strips}
    with open(out, "r+b") as f:
        for o, n in strips:
            if (o + n) % 2 and o + n not in starts:
                f.seek(o + n)
                f.write(bytes(1))


def background_alias():
    # What Finder stores for /.background/background.tiff on the volume, minus anything about the
    # machine (dmgbuild writes the same kind of alias from a mounted image). Disk type 5 (ejectable),
    # the volume flags and the folder/file ids (25, 26) are what Finder wrote on a test image; the ids
    # are only hints, as the volume's creation date.
    volume = VolumeInfo(VOLUME, FIXED_DATE, b"H+", 5, 0x0D02, b"\0\0")
    volume.posix_path = f"/Volumes/{VOLUME}"
    target = TargetInfo(0, "background.tiff", 25, 26, FIXED_DATE, b"\0\0\0\0", b"\0\0\0\0")
    target.folder_name = ".background"
    target.cnid_path = [25]
    target.carbon_path = f"{VOLUME}:.background:\0background.tiff"
    target.posix_path = "/.background/background.tiff"
    return Alias(volume=volume, target=target).to_bytes()


def write_ds_store(out):
    if os.path.exists(out):
        os.remove(out)
    with DSStore.open(out, "w+") as d:
        d["."]["bwsp"] = {
            "ShowStatusBar": False,
            "ShowToolbar": False,
            "ShowTabView": False,
            "ContainerShowSidebar": False,
            "ShowSidebar": False,
            "WindowBounds": f"{{{{200, 528}}, {{{WIDTH}, {HEIGHT + TITLE_BAR}}}}}",
        }
        d["."]["icvp"] = {
            "viewOptionsVersion": 1,
            "backgroundType": 2,
            "backgroundImageAlias": background_alias(),
            "backgroundColorRed": 1.0,
            "backgroundColorGreen": 1.0,
            "backgroundColorBlue": 1.0,
            "gridOffsetX": 0.0,
            "gridOffsetY": 0.0,
            "gridSpacing": 100.0,
            "arrangeBy": "none",
            "showIconPreview": True,
            "showItemInfo": False,
            "labelOnBottom": True,
            "textSize": 16.0,
            "iconSize": float(ICON_SIZE),
        }
        d["."]["vSrn"] = ("long", 1)
        for name, pos in ICONS.items():
            d[name]["Iloc"] = pos


def main():
    write_tiff(os.path.join(HERE, "background.tiff"))
    write_ds_store(os.path.join(HERE, "dmg-layout.DS_Store"))


if __name__ == "__main__":
    main()
