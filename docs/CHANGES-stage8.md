# Stage 8 changes worth knowing

Stage 8 makes djforge its own app: the rebrand from SoundCraft, and djforge's own look. What
follows changes behaviour you may rely on.

## The rebrand

- **Binaries.** The app is `djforge` (`djforge.exe`) and the command line `djforge-cli`
  (`djforge-cli.exe`). The Cargo package names (`soundcraft`, `soundcraft-cli` and the
  `soundcraft-*` crates) stay, as do the `SOUNDCRAFT_*` environment variables.
- **Sessions save as `.djforge`** (bundles as `.djforgez`). SoundCraft's `.scraft` and `.scraftz`
  still open everywhere: dialogs, drag and drop, the workspace browser, recent files, the CLI and
  recovery. Saving a session opened from a `.scraft` writes the `.djforge` beside it and never
  touches the `.scraft`; Open Recent then offers the `.djforge`.
- **The settings folder moves** to `%APPDATA%\djforge` (macOS: `~/Library/Application
  Support/djforge`; elsewhere `$XDG_CONFIG_HOME/djforge` or `~/.config/djforge`). The first run
  copies SoundCraft's settings files, `Presets/` and autosaves with unsaved work across once,
  with their modification times, and leaves the SoundCraft folder untouched; logs and autosave
  history are not copied. The copy never replaces a file in the djforge folder. It is done when
  `migrated-from-soundcraft.txt` is there. If a top-level settings file couldn't be read
  (another program held it), the marker isn't written and the next starts copy just those files
  again (listed in `migration-pending.json`), up to five starts; then djforge gives up on them,
  says so in the status line and marks the migration done. Files that couldn't be read inside
  `Presets/` or `Autosave/` are logged but not retried. A copy interrupted partway (djforge
  killed, the machine off) is removed and made again on the next start; every copy is on disk
  before the marker is written. A second djforge starting during the migration leaves it to the
  first (`.migrating.lock`). On a volume without hard links (a network share, FAT) the copy
  falls back to writing each new file directly.
- **App id** `io.github.mloki23.djforge` everywhere: the window's app id, Flatpak, `.desktop`,
  AppStream metadata, MIME and icon names, and the macOS bundle id. djforge has its own
  installer identity (a new MSI UpgradeCode and ProgIds), so it installs beside SoundCraft and
  never upgrades or removes it. The Windows installer makes djforge the default app for
  `.djforge` only; `.scraft` is offered under "Open with".

## djforge Dark, and SoundCraft Classic

- **Theme mode `dark` now means djforge Dark**, the new default, built from mixforge's studio
  palette: near-black blue-greys (`#111317`, `#191c22`, `#20242c`), `#e6e8ec` text, and yellow
  (`#f8d200`) for the selection, playhead, counters, active buttons and focus outlines. Recording
  and record arm are red (`#e0342c`), playing and input green (`#32be44`); meters stay green,
  yellow and red. Saved preferences with `"theme": "dark"` get the new look.
- **SoundCraft's grey dark look is kept as SoundCraft Classic**, mode `classic`
  (`ui.theme {"mode": "classic"}`), unchanged colour for colour.
- The theme menu (Setup › Appearance, or Window › UI Customization: Theme) lists djforge Dark, Light, System and
  SoundCraft Classic. System follows the OS: Light, or djforge Dark when the OS is dark.
- Light changes slightly: the edit-mode buttons are a darker green so their text is readable
  (4.5:1), default buttons (Export, Recover, a dialog's OK) have white labels, and the
  clip-gain fader popup is light, so its green readout is readable.

## Waveforms coloured by frequency

- **Audio clip waveforms are coloured by frequency in djforge Dark**, as in Rekordbox's 3-band
  view: lows (below 200 Hz) red, mids (200 Hz to 2.5 kHz) green, highs (above 2.5 kHz) blue,
  mixed per pixel column, so kicks and bass read red, vocals green and hats blue. The clip body
  turns a dark tint of the clip's colour and the colour stays in the name bar (or a 3-point strip
  along the clip's top when names are hidden). Light and SoundCraft Classic keep one-colour
  waveforms. Setup › Appearance › "Colour waveforms by frequency" (`ui.waveform_bands {"value":
  true|false|null}`, null following the theme) overrides the theme, saved with the UI prefs.
  Split clips, muted clips, the outgoing half of an overlapped crossfade and recording takes
  stay one colour. Loading audio takes a little longer: the bands are filtered once, with the
  waveform overview (about 0.2 s per channel for 10 minutes), and keep about 43 MB per hour of
  stereo in memory.
