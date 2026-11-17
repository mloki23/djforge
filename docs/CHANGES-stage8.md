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
  mixed per pixel column with the loudest band leading, so kicks and bass read red, vocals green,
  hats blue, and a full mix keeps a colour. The clip body turns a dark tint of the clip's colour
  and the colour stays in the name bar (or a 3-point strip along the clip's top when names are
  hidden); a selected clip has a pale name bar and an outline in the accent colour. Light and
  SoundCraft Classic keep one-colour waveforms. Setup › Appearance › "Colour waveforms by
  frequency" (`ui.waveform_bands {"value": true|false|null}`, null following the theme)
  overrides the theme, saved with the UI prefs; turned on in Light or Classic, the clips it
  colours get the same dark bodies, as the band colours need a dark ground. Split clips, muted
  clips, the outgoing half of an overlapped crossfade and recording takes stay one colour.
- **The bands are built in the background** after audio loads, one source at a time and those of
  the clips in view first, so opening a session or importing takes no longer; a waveform shows
  one colour until its bands are ready (about 0.2 s per channel for 10 minutes of audio), so a
  screenshot (`ui_shot`, `ui.screenshot`) taken straight after a load may show one-colour
  waveforms. With the colours off they aren't built, and turning them off stops the builds under
  way. They keep about 43 MB per hour of stereo in memory.
- **In the browser (the wasm build) waveforms stay one colour**: there are no threads to build
  the bands on, and building them on the UI thread would stall it.

## Deck colours and transition markers

- **The DJ decks' clips carry their deck's colour in djforge Dark**: a 3-point band along the
  clip's top, Deck A cyan (`#26c6da`), Deck B magenta (`#c64fd6`), over the top of the name bar
  so the clip's own colour stays below it. Tinted are audio clips on a deck track (one a
  transition mixes, or holding beat-locked clips; failing both, tracks named "Deck A"/"Deck B")
  that are beat-locked or mixed. A track named "Deck A" or "Deck B" is that deck; other deck
  tracks take A, B, A… by their place among the decks. Off in Light and SoundCraft Classic;
  Setup › Appearance › "Deck colours" (`ui.deck_colours {"value": true|false|null}`) overrides
  the theme, saved with the UI prefs.
- **Each transition is marked in the Edit window**: flags at the foot of the rulers and faint
  lines through its two decks' lanes, green at the mix-in (`entry_bar`), yellow at the bass hit
  (`hit_bar`) and red at the mix-out, the bar the outgoing bass is gone by (`gone_bar`: the hit
  plus `out_bars`). Hovering a flag names the mix ("Out → In · blend · 16-bar build · hit bar
  33"); clicking it selects the transition (its incoming clip, which the DJ commands act on) and
  locates there. Where a mix-out and the next mix-in share a bar, a click takes the mix-in.
- **`edit.select` with `clips` and `start`** now keeps the given start (and end) instead of
  selecting the clips' range; clips alone still select their range.
