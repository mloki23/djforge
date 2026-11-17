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
- **`SOUNDCRAFT_NO_PREFS`** (agents' test runs) keeps djforge out of the settings folder
  entirely: besides reading and writing no preferences and keeping no log file, it skips the
  copy of SoundCraft's settings, turns autosave off (no crash recovery; earlier autosaves aren't
  offered; an unsaved session records into the temporary folder) and leaves no
  Presets folder, so user presets can't be saved or loaded (a plugin's factory default still can).
- **MCP resources** are `djforge://session` and `djforge://commands`; SoundCraft's
  `soundcraft://` URIs still read the same resources (see `docs/mcp.md`).
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
  (`ui.theme {"mode": "classic"}`): its colour tokens are unchanged, colour for colour (a test
  fingerprints them, and the new tempo and clip-name tokens keep SoundCraft's values). Changes
  made for every theme still reach it, so Classic is not pixel for pixel SoundCraft: idle
  controls are outlined (`widget_edge`), floating windows have the new titles and positions,
  cut-short names end in "…", tempo labels sit on the tempo ruler's colour, and fader and meter
  scale numbers are larger (see "Readability" below).
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
  hidden), unless the clip is on a DJ deck with deck colours on, whose name bar takes the deck's
  colour (see below); a selected clip has a pale name bar and an outline in the accent colour. Light and
  SoundCraft Classic keep one-colour waveforms. Setup › Appearance › "Colour waveforms by
  frequency" (`ui.waveform_bands {"value": true|false|null}`, null following the theme)
  overrides the theme, saved with the UI prefs; turned on in Light or Classic, the clips it
  colours get the same dark bodies, as the band colours need a dark ground. Split clips, muted
  clips, the outgoing half of an overlapped crossfade and recording takes stay one colour.
- **Zoomed out, the colours are smoothed**: each pixel column's band levels are mixed, by
  energy, with up to three neighbours either side, so columns that happen to catch a kick or a
  hat no longer alternate red and blue in stripes. Zoomed in close, nothing is mixed and each
  beat keeps its colour.
- **The bands are built in the background** after audio loads, one source at a time and those of
  the clips in view first, so opening a session or importing takes no longer; a waveform shows
  one colour until its bands are ready (about 0.2 s per channel for 10 minutes of audio), so a
  screenshot (`ui_shot`, `ui.screenshot`) taken straight after a load may show one-colour
  waveforms. With the colours off they aren't built, and turning them off stops the builds under
  way. They keep about 43 MB per hour of stereo in memory.
- **In the browser (the wasm build) waveforms stay one colour**: there are no threads to build
  the bands on, and building them on the UI thread would stall it.

## Deck colours and transition markers

- **The DJ decks' clips carry their deck's colour in djforge Dark**: the clip's name bar is
  filled with it, Deck A cyan (`#26c6da`), Deck B magenta (`#c64fd6`), the name in dark text;
  with names hidden it is a 5-point band along the clip's top instead (and on a muted clip, or
  one dimmed under an automation view, a faded band over the name bar). This is a deliberate
  trade-off: in djforge Dark a deck clip's own colour then shows only as the tint of its body,
  as the deck colour reads at a glance which deck a clip plays on. Tinted are audio clips on a deck track that are
  beat-locked or mixed. The deck tracks are the tracks the transitions mix; with no transitions,
  the tracks holding beat-locked clips; failing both, tracks named "Deck A"/"Deck B" (ignoring
  case and outer spaces). So in an imported set a beat-locked vocal no transition mixes is no
  deck and isn't tinted. A track named "Deck A" or "Deck B" is that deck; the other decks take
  the letter no named deck has, or alternate A, B… among themselves when neither or both are
  named. `prod.new_part` places parts after the same deck tracks: with transitions, a
  beat-locked track they don't mix no longer counts. Off in Light and SoundCraft Classic;
  Setup › Appearance › "Deck colours" (`ui.deck_colours {"value": true|false|null}`) overrides
  the theme, saved with the UI prefs.
- **Each transition is marked in the Edit window**: flags at the foot of the rulers and faint
  lines through its two decks' lanes, green at the mix-in (`entry_bar`), yellow at the bass hit
  (`hit_bar`) and red at the mix-out, the bar the outgoing bass is gone by (`gone_bar`: the hit
  plus `out_bars`). In Light they are a darker green, amber and red, to read on its pale rulers
  and lanes. Hovering a flag names the mix ("Out → In · blend · 16-bar build · hit bar 33");
  clicking it selects the transition (its incoming clip, which the DJ commands act on) and
  locates there. Where a mix-out and the next mix-in share a bar, the side of the flag clicked
  decides (the mix-in pennant points right, the mix-out's left), and the tooltip names both.
- **`edit.select` with `clips` and `start`** now keeps the given start (and end) instead of
  selecting the clips' range; clips alone still select their range.

## Readability

These apply in every theme, SoundCraft Classic included.

- **Tempo labels** sit just after their red triangle on the tempo ruler, on the ruler's own
  colour so a later triangle never cuts into one, and thin out when changes crowd together: a
  label that would run into the previous one is left out. In Light the labels are dark, to read
  on its pale green ruler (4.5:1 or more).
- **Idle controls are outlined** (the `widget_edge` colour, 3:1 or more on panels and windows),
  so an unticked checkbox, a radio button or an empty text field no longer vanishes into the
  panel.
- **Floating windows** have bold 13-point titles, first open centred just below the toolbar,
  and are kept below the menu bar, so they never cover the menus or the counters.
- **Cut-short names end in "…"**: clip names in the timeline, the Clips list and Session view
  slots.
- **MIDI Devices** spaces its grid's columns apart.
- **Fader and meter scales** have larger numbers (9.5 point rather than 8) and ticks in the dim
  text colour.

## Audio devices

- **Audio devices no longer crash djforge when opened from a thread after an ended one**
  (Windows). cpal's one WASAPI device enumerator is now made on a thread kept for it, asking only
  for the default output device. The first device call waits for it at most 5 seconds (a
  hanging driver is logged, not waited on), and a panic while making it is logged; the thread is
  kept either way.

## Crash recovery

- **Unsaved changes made right after a save are no longer deleted at the next launch.** An
  autosave was thrown away unless it was strictly newer than its session file, so a change
  autosaved within the file system's time resolution of a save (the same clock tick on NTFS, 2 s
  on FAT) was lost after a crash. An autosave is now dropped only once its session file holds
  exactly its session: the same session apart from where audio is stored, each source's audio
  the same file or one of the same bytes. Anything else, such as another program, a sync tool or
  a second instance rewriting the session file, keeps the autosave and offers it. Folders from
  SoundCraft or earlier builds, which don't record the session file's state (`origin-state`),
  are kept unless their session file is strictly newer. Audio is compared byte for byte only up
  to 200 MB a file: a larger copy (a long take) counts as different and keeps the autosave, so
  checking can't stall the start.
