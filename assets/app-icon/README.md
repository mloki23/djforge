# djforge app icon

**Four waveform bars**: a stylised waveform in djforge yellow on a dark rounded square, simple
enough to read at 16 px.

## Palette

Exactly two colours, from djforge's studio palette:

| Colour | Hex | Used for |
|---|---|---|
| djforge yellow (accent) | `#f8d200` | The four waveform bars |
| Background | `#111317` | The rounded square |

## Geometry

A 512 × 512 grid: a full-bleed rounded square (`rx=112`) and four bars 64 wide with 40 between
them, centred, with rounded ends and heights 176, 328, 248 and 136. Every bitmap is drawn at its
own size with the bars snapped to whole pixels (at 16 px: 2 px bars, 1 px gaps), so small icons
stay crisp. Windows and Linux use the full tile. The macOS icons (`djforge.icns`,
`djforge-macos-512.png`) sit on Apple's grid: an 824/1024 body with a transparent margin.

## Provenance and licence

Original, drawn from scratch by the djforge contributors as simple geometric shapes; nothing was
traced or copied. MIT OR Apache-2.0, like the code (see `ATTRIBUTION.md`). It replaces
SoundCraft's placeholder nightingale icon.

## Files

| File | What it is |
|---|---|
| `icon.svg` | The canonical vector |
| `djforge-1024.png` | 1024 px render |
| `djforge-macos-512.png` | Runtime window/Dock icon on macOS (Apple grid margin) |
| `djforge.icns` | macOS bundle icon (16–1024 px) |
| `djforge.ico` | Windows icon (16–256 px), embedded in `djforge.exe` and used by the MSI |
| `hicolor/<size>x<size>/apps/io.github.mloki23.djforge.png` | Linux/FreeBSD icon theme, 16–512 px, plus `scalable/` (named after the Linux app id, which is unchanged) |

Regenerate everything with `python tools/icon/make_icon.py` (or `packaging/icons.sh`). It needs
Python 3 and Pillow, and nothing else: the shapes are defined once in the script, which writes the
SVG and draws every bitmap itself.
