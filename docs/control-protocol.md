# Control protocol

SoundCraft can be driven completely by other programs: scripts, test harnesses and AI agents.
Every menu item, button, mouse gesture and keystroke maps to a **command** (or to synthetic
input), and every command is reachable from:

| Surface | How |
|---|---|
| Desktop app control channel | `soundcraft --control <port>` (`0` = pick a free port; printed as `SOUNDCRAFT_CONTROL_PORT=…`) |
| CLI against a running app | `soundcraft-cli app --port <port> <method or command id> [JSON]` |
| Headless CLI | `soundcraft-cli run --demo --cmd 'mix.volume={"track":"Kick","db":-6}' --bounce out.wav` |
| MCP (headless) | `soundcraft-cli mcp [--demo]` |
| MCP (bridged to the app) | `soundcraft-cli mcp --connect <port>` |

## Isolated automation profiles

Set `SOUNDCRAFT_CONFIG_DIR` to a fresh directory to isolate UI preferences, `Autosave`, `Presets`, and `logs`.
The nonempty value is used as the complete settings directory. Unset retains the normal platform paths;
an explicit empty value disables the settings directory instead of falling back to a personal profile. Save synthetic sessions to explicit scratch paths. Do not use `SOUNDCRAFT_NO_PREFS`
when testing preference persistence across restarts.

Run UI acceptance with `--no-audio`. This disables the output player; it does not disable recording
input opened by arm/monitor commands. An automated UI smoke run must never arm a track, enable input
monitoring, or start recording. Control input injects egui events; it does not establish operating-system
input delivery or screen-reader acceptance. Native screenshots use the presented viewport frame.

## Wire format

Newline-delimited JSON over TCP on `127.0.0.1`. One request per line, one reply per line:

```json
{"id": 1, "method": "engine.execute", "params": {"command": "track.new", "params": {"count": 2, "format": "Stereo"}}}
{"id": 1, "ok": true, "result": {"tracks": [12, 13]}}
```

Errors: `{"id": 1, "ok": false, "error": "bad parameters for `mix.volume`: no track named `Nope`"}`.

## Methods

| Method | Params | What it does |
|---|---|---|
| `engine.execute` (alias `command`) | `{command, params}` | Run a command programmatically. **Never opens a dialog**; empty params use defaults or the current selection. |
| `engine.commands` | `{filter?}` | Every command: id, label, menu path, shortcut, params doc, enabled + reason. Includes UI-layer commands (`window.*`). |
| `engine.parity` | `{}` | Parity report against the incumbent's menu catalog. |
| `session.inspect` | `{detail?: "summary"|"full"}` | The document: tracks, clips (with times), inserts (+params when full), sends, routing, automation, markers, groups, busses, sources (`loaded` true when decoded audio is in the pool), selection, zoom (`samples_per_px`, `scroll`), transport, undo label. |
| `ui.inspect` | `{}` | UI state (`window`, panels, dialogs), window size, playing/position, audio device, and `edit_layout` (timeline rect and every track row's rect, for clicking). |
| `ui.set` | any `UiState` fields | e.g. `{"window": "Mix", "narrow_mix": true}`. |
| `ui.menu.list` | `{}` | Every catalog menu path and the command that implements it (or null). |
| `ui.menu.invoke` | `{path}` or `{id}` | Like clicking the menu item: may open its dialog (e.g. `"Track > New..."`). |
| `ui.click` | `{x, y, button?, count?, cmd?, shift?, alt?, ctrl?}` | Synthetic click (window points). |
| `ui.drag` | `{x, y, to_x, to_y, steps?}` | Synthetic drag with the primary button. |
| `ui.scroll` | `{x, y, dx?, dy?, unit?: "point"\|"line", steps?, cmd?, shift?, alt?, ctrl?}` | A two-finger trackpad swipe over `(x, y)`: `dx`/`dy` points spread over `steps` frames (default 8). Positive values move the content right/down (natural scrolling). `unit: "line"` sends mouse-wheel notches instead. With `cmd` it zooms; with `shift` it scrolls sideways. |
| `ui.zoom` | `{x, y, factor, steps?}` | A trackpad pinch around `(x, y)`: `factor` > 1 spreads the fingers (zooms in). |
| `ui.move` | `{x, y}` | Move the pointer. |
| `ui.key` | `{key, cmd?, shift?, alt?, ctrl?}` | Key press (`Space`, `Enter`, `A`, `F7`, `=`…). |
| `ui.text` | `{text}` | Type text into the focused field. |
| `ui.screenshot` | `{path?}` | PNG of the window (base64 in `png_base64` when no path). |
| `app.quit` | `{}` | Quit. |

### Live MIDI input (UI commands, through `engine.execute`)

| Command | Params | What it does |
|---|---|---|
| `midi.inputs` | `{}` | The MIDI input ports: `name`, `raw_name`, `backend_id`, `open`, `enabled`; plus `available` (a MIDI system), `latency_ms` (the live latency L: the longest recent audio callback plus 2 ms) and `dropped` (live events dropped). |
| `midi.input_enable` | `{name, enabled: bool}` | Enables or disables a port for live play, in the app preferences (per machine), and opens or closes it. Enabled ports open when they appear. |
| `midi.target` | `{track?: id \| name \| null}` | The track live MIDI plays: this override, else the first record-armed instrument track, else the selected instrument track with input monitoring on. `null` or nothing clears the override. Returns `override` and `target`. |
| `midi.panic` | `{}` | Silences every instrument: live notes, arrangement and slot notes off, controllers reset. `event.all_notes_off` does the same. |

### MIDI learn (UI commands, through `engine.execute`)

A mapping binds a device and a message (a note or a controller on a channel) to a control, with a mode. Mappings are app preferences (per machine), not part of the session; Setup › MIDI › MIDI Mappings lists them. While mapped, a note or controller doesn't play the live MIDI target (note-offs still pass). Tracks are kept as `{id, name}`: the id while that track has the name, else the track with the name, so a mapping made in one session finds its track in another. A mapping whose target is missing does nothing and says so on the status line.

| Command | Params | What it does |
|---|---|---|
| `midi.mappings` | `{}` | Every mapping: `index`, `device` (`backend_id`, `raw_name`, `name`), `matcher`, `message` (`"CC 7"`, `"Note 36 ch 10"`), `target`, `label`, `mode`, `sensitivity`, `pickup`, `resolved` (the target exists now) and `activity_s` (seconds since its last message, or `null`); plus `learning` (the target being learned, or `null`). |
| `midi.map` | `{device, matcher, target, mode?, sensitivity?, pickup?, replace?}` | Adds a mapping, replacing the target's own. `device`: a listed input name or `{backend_id, raw_name, name}`. `matcher`: `{type: "note", ch, note}` or `{type: "cc", ch, cc}` (channels 0–15). `target` (`kind`, then its fields, 0-based): `slot_launch {column, slot}`, `scene {row}`, `stop_column {column}`, `stop_all`, `track_volume {track}`, `track_pan {track}`, `stem_fader {track, stem}`, `instrument_param {track, param}`, `insert_param {track, slot, param}`, `command {id, params}`; `track` is an id, a name or `{id, name}`. `mode`: `momentary`, `toggle` (notes are one of these), `absolute`, `rel_twos` (1–63 up, 65–127 down), `rel_offset` (64 is centre), `rel_sign_mag` (bit 6 is down); default momentary on triggers, toggle for a note on a fader, absolute for a controller on one. `sensitivity`: travel per relative tick, 0.001–0.25 (clamped), default 0.01; channels above 15 or notes and controllers above 127 are refused. `pickup` (default true): an absolute controller is ignored until it reaches the current value. `replace: true` also removes another target's mapping of the same message (else both are kept). Returns `{index, mapping}`. |
| `midi.unmap` | `{index}` \| `{target}` \| `{device, matcher}` \| `{all: true}` | Removes mappings; returns `{removed}`. |
| `midi.learn` | `{target}` \| `{cancel: true}` | Learn MIDI: the next note-on or controller from an enabled input binds `target` (as Learn MIDI in a control's right-click menu does). Learning a target again replaces its mapping; a message another target uses asks Replace or Keep Both. |
| `midi.learn_command` | `{}` | Opens Search to pick the command a control runs (Learn MIDI for Command...). |
| `window.midi_mappings` | `{value?: bool}` | Shows or hides Setup › MIDI › MIDI Mappings. |

Triggers (slot launch, scene, stops, commands) act on a press (a note-on, or a controller rising through 64) with the Session view's own quantised commands; Toggle on a slot launches it, or stops its column when it is the one playing. Faders, pans, stem faders and parameters move along the on-screen control's travel (the fader taper for volume and stems). Moves of MIDI controls are one undo step until they pause for half a second, however many controls move together; in Touch or Latch automation they write the volume, pan and stem lanes as a mouse drag does. A controller learned with Learn MIDI on a fader or knob is watched for 0.3 s (or 4 messages) to tell an encoder from a knob, and the "Mapped to" overlay offers the mode it chose.

## Positions and references

- Tracks: id (number) or name (`"Kick"`).
- Clips: id (from `session.inspect`).
- Positions: samples (`96000`), seconds (`{"seconds": 2.5}`), or a time string in any timebase,
  optionally prefixed: `"bars_beats:5|1|000"`, `"min_secs:0:12.500"`, `"timecode:00:00:10:00"`,
  `"samples:48000"`.
- Ranges: `start` + `end` (or `start` + `length`); omitted = the current edit selection.

## Examples

```sh
P=$(soundcraft --demo --control 0 | sed -n 's/SOUNDCRAFT_CONTROL_PORT=//p')   # or read stderr
soundcraft-cli app --port $P mix.solo '{"track":"Bass"}'
soundcraft-cli app --port $P transport.play
soundcraft-cli app --port $P ui.screenshot '{"path":"/tmp/shot.png"}'
soundcraft-cli app --port $P ui.menu.invoke '{"path":"Track > New..."}'
```

Offscreen (no window): `cargo run -p soundcraft-ui-egui --example ui_shot -- out.png script.jsonl`,
where each script line is a request (`{"method": …, "params": …}`), `{"shot": "path.png"}` or
`{"steps": n}`. Use `--system-theme light|dark|none` to supply the initial OS appearance, and
script events such as `{"system_theme": "dark"}` to change it on subsequent frames.

Appearance defaults to Dark. `ui.theme {"mode":"system"}` follows the appearance supplied by
the native or web integration at launch and on later frames, while the saved choice stays System.
When the integration cannot report an appearance, System uses Dark. Manual Light and Dark stay
fixed when the OS appearance changes. An offscreen render without `--system-theme` supplies no
OS appearance and therefore resolves System to Dark.

## Panel docking

Tracks and Groups, Clips, MIDI Editor, Memory Locations, and Undo History use movable panels. Commands target the active Edit or Mix view; floating panels stay inside the current viewport. Edit and Mix retain separate arrangements; Clips and MIDI Editor belong to Edit. Transport, plug-in, and specialist utility windows retain their existing hosts. The document/session view is protected. Layouts, active tabs, panel visibility, and floating rectangles are saved with the app's existing UI preferences. These operations do not edit document contents.

Panel IDs: `tracks`, `clips`, `midi`, `memory`, `history`. The protected center is `canvas`.

`ui.dock` accepts `operation` and `panel`. Operations are `open`, `close`, `activate`, `float` (with finite positive `rect: [x, y, width, height]`), or `move` (with `target` and `zone: center|left|right|top|bottom`). Center groups with the target; side zones split beside it. `ui.dock.reset` restores the default arrangement. Invalid moves return an error without changing the layout.

Invoke these UI commands through `engine.execute`, for example `{"command":"ui.dock","params":{"operation":"float","panel":"tracks","rect":[100,100,320,440]}}`.

Advanced `ui.dock` operations use the same validated transaction as pointer gestures:
`moveFloating` updates `rect`, `resizeSplit` accepts a boolean child `path` and
`size` (`{"Ratio":0.5}`, `{"FixedFirst":280}`, or `{"FixedSecond":280}`),
`setStackOpen` takes `open`, and `resizeStack` takes a point `height` or null.
A center `move` can include `before` (panel ID or null to append) for tab order.
The equivalent `{"action": ...}` form accepts a serialized shared docking action;
this is also the form emitted by the renderer. Protected-canvas and app-specific panel
rules apply equally to both forms.
