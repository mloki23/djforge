# Pioneer DJ DDJ-FLX4: MIDI messages and djforge's built-in layout

Source: Pioneer DJ, *DDJ-FLX4 List of MIDI messages*, Ver 1.0 (5 pages, © 2022 AlphaTheta),
read from the official PDF. The MIDI numbers are facts about the hardware; the tables below are
djforge's own extraction of them, and the "djforge" columns are djforge's own design. No other
mapping (Mixxx, Rekordbox, Serato, community files) was read or used.

The built-in layout lives in `crates/ui-egui/src/layouts/ddj_flx4.rs`. A test
(`the_layout_table_matches_the_hardware_doc`) parses the two input tables below and checks that
the Rust table has exactly the same messages, so keep them in step.

## Conventions

- **Ch** is the MIDI channel as Pioneer numbers it, 1–16 (status `0x90` / `0xB0` is channel 1).
- **Data 1** is the note or controller number, decimal then hex.
- Buttons send Note On with velocity `0x7F` when pressed and `0x00` when released (a Note On with
  velocity 0 is a note-off).
- **14-bit** controls (knobs, faders, the tempo slider) send the MSB controller, then the LSB
  controller (MSB + 32). Value = MSB × 128 + LSB, 0..16383. djforge acts on the LSB, using the MSB
  it last received.
- **Relative** controls send the change since the last message. The jogs are centred on `0x40`
  (clockwise counts up from `0x41`, anticlockwise down from `0x3F`); the browse knob is two's
  complement (clockwise up from `0x01`, anticlockwise down from `0x7F`).
- **SHIFT** is handled in the hardware: holding it changes the message a control sends. The
  Shift column says which layer a row's message belongs to: `no`, sent without SHIFT; `yes`, sent
  with SHIFT held; `either`, the same message with or without SHIFT. A knob or fader whose row says
  `no` has no SHIFT message in the list.
- **SHIFT + a channel fader or the crossfader** sends only the fader-start notes (below), not
  its controller, so the fader's movement while SHIFT is held is not followed. When SHIFT is let
  go, the fader's next message applies as it is: a small jump is possible.
- **MIDI Learn** takes precedence over the layout. A mapping learned on either half of a 14-bit
  control covers both halves: the other half no longer reaches the layout.
- "Deck 1" is djforge's Deck A, "Deck 2" Deck B.
- In the djforge column, `(unmapped)` means djforge does nothing with the message yet. The layout
  still claims it, so it never plays the live instrument; MIDI Learn can map it.

## Inputs

| Id | Control | Deck | Shift | Type | Ch | Data 1 | LSB | Values | djforge |
|---|---|---|---|---|---|---|---|---|---|
| play | PLAY/PAUSE | 1 | no | Note | 1 | 11 (0x0B) | — | press | play / pause |
| play_shift | PLAY/PAUSE | 1 | yes | Note | 1 | 14 (0x0E) | — | press | (unmapped) |
| play | PLAY/PAUSE | 2 | no | Note | 2 | 11 (0x0B) | — | press | play / pause |
| play_shift | PLAY/PAUSE | 2 | yes | Note | 2 | 14 (0x0E) | — | press | (unmapped) |
| cue | CUE | 1 | no | Note | 1 | 12 (0x0C) | — | press / release | CDJ cue: `deck.cue` down on press, up on release |
| cue_shift | CUE | 1 | yes | Note | 1 | 72 (0x48) | — | press | back to the start of the track |
| cue | CUE | 2 | no | Note | 2 | 12 (0x0C) | — | press / release | CDJ cue: `deck.cue` down on press, up on release |
| cue_shift | CUE | 2 | yes | Note | 2 | 72 (0x48) | — | press | back to the start of the track |
| shift | SHIFT | 1 | no | Note | 1 | 63 (0x3F) | — | press / release | held state only (the hardware sends the shifted messages) |
| shift | SHIFT | 2 | no | Note | 2 | 63 (0x3F) | — | press / release | held state only |
| jog_top | JOG DIAL (platter), turned, vinyl mode on | 1 | no | CC | 1 | 34 (0x22) | — | relative, 0x40 centre | paused: fine search; playing: pitch-bend nudge |
| jog_top_vinyl_off | JOG DIAL (platter), turned, vinyl mode off | 1 | no | CC | 1 | 35 (0x23) | — | relative, 0x40 centre | as the platter with vinyl mode on |
| jog_top_shift | JOG DIAL (platter), turned | 1 | yes | CC | 1 | 41 (0x29) | — | relative, 0x40 centre | fast search |
| jog_touch | JOG DIAL (platter), touch | 1 | no | Note | 1 | 54 (0x36) | — | touch / release | touch state (the platter turns send their own messages) |
| jog_touch_shift | JOG DIAL (platter), touch | 1 | yes | Note | 1 | 103 (0x67) | — | touch / release | touch state |
| jog_side | JOG DIAL (wheel side), turned | 1 | either | CC | 1 | 33 (0x21) | — | relative, 0x40 centre | nudge (paused: search) |
| jog_top | JOG DIAL (platter), turned, vinyl mode on | 2 | no | CC | 2 | 34 (0x22) | — | relative, 0x40 centre | paused: fine search; playing: pitch-bend nudge |
| jog_top_vinyl_off | JOG DIAL (platter), turned, vinyl mode off | 2 | no | CC | 2 | 35 (0x23) | — | relative, 0x40 centre | as the platter with vinyl mode on |
| jog_top_shift | JOG DIAL (platter), turned | 2 | yes | CC | 2 | 41 (0x29) | — | relative, 0x40 centre | fast search |
| jog_touch | JOG DIAL (platter), touch | 2 | no | Note | 2 | 54 (0x36) | — | touch / release | touch state |
| jog_touch_shift | JOG DIAL (platter), touch | 2 | yes | Note | 2 | 103 (0x67) | — | touch / release | touch state |
| jog_side | JOG DIAL (wheel side), turned | 2 | either | CC | 2 | 33 (0x21) | — | relative, 0x40 centre | nudge (paused: search) |
| loop_in | IN | 1 | no | Note | 1 | 16 (0x10) | — | press | loop in |
| loop_in_shift | IN (IN ADJUST) | 1 | yes | Note | 1 | 76 (0x4C) | — | press | (unmapped) |
| loop_in | IN | 2 | no | Note | 2 | 16 (0x10) | — | press | loop in |
| loop_in_shift | IN (IN ADJUST) | 2 | yes | Note | 2 | 76 (0x4C) | — | press | (unmapped) |
| loop_out | OUT | 1 | no | Note | 1 | 17 (0x11) | — | press | loop out |
| loop_out_shift | OUT (OUT ADJUST) | 1 | yes | Note | 1 | 78 (0x4E) | — | press | (unmapped) |
| loop_out | OUT | 2 | no | Note | 2 | 17 (0x11) | — | press | loop out |
| loop_out_shift | OUT (OUT ADJUST) | 2 | yes | Note | 2 | 78 (0x4E) | — | press | (unmapped) |
| loop_4beat | 4 BEAT / EXIT | 1 | no | Note | 1 | 77 (0x4D) | — | press | not looping: a 4-beat loop; looping: exit |
| loop_4beat_shift | 4 BEAT / EXIT (ACTIVE) | 1 | yes | Note | 1 | 80 (0x50) | — | press | reloop (the last loop again) |
| loop_4beat | 4 BEAT / EXIT | 2 | no | Note | 2 | 77 (0x4D) | — | press | not looping: a 4-beat loop; looping: exit |
| loop_4beat_shift | 4 BEAT / EXIT (ACTIVE) | 2 | yes | Note | 2 | 80 (0x50) | — | press | reloop (the last loop again) |
| loop_call_back | CUE/LOOP CALL ◁ | 1 | no | Note | 1 | 81 (0x51) | — | press | looping: halve; else beat jump back 4 |
| loop_call_back_shift | CUE/LOOP CALL ◁ (1/2X) | 1 | yes | Note | 1 | 62 (0x3E) | — | press | halve the loop |
| loop_call_back | CUE/LOOP CALL ◁ | 2 | no | Note | 2 | 81 (0x51) | — | press | looping: halve; else beat jump back 4 |
| loop_call_back_shift | CUE/LOOP CALL ◁ (1/2X) | 2 | yes | Note | 2 | 62 (0x3E) | — | press | halve the loop |
| loop_call_fwd | CUE/LOOP CALL ▷ | 1 | no | Note | 1 | 83 (0x53) | — | press | looping: double; else beat jump on 4 |
| loop_call_fwd_shift | CUE/LOOP CALL ▷ (2X) | 1 | yes | Note | 1 | 61 (0x3D) | — | press | double the loop |
| loop_call_fwd | CUE/LOOP CALL ▷ | 2 | no | Note | 2 | 83 (0x53) | — | press | looping: double; else beat jump on 4 |
| loop_call_fwd_shift | CUE/LOOP CALL ▷ (2X) | 2 | yes | Note | 2 | 61 (0x3D) | — | press | double the loop |
| sync | BEAT SYNC | 1 | no | Note | 1 | 88 (0x58) | — | sent on release | sync on / off |
| sync_long | BEAT SYNC, long press | 1 | no | Note | 1 | 92 (0x5C) | — | sent on release | sync off |
| sync_shift | BEAT SYNC (TEMPO RANGE) | 1 | yes | Note | 1 | 96 (0x60) | — | sent on release | tempo range 8 → 16 → 50 % |
| sync | BEAT SYNC | 2 | no | Note | 2 | 88 (0x58) | — | sent on release | sync on / off |
| sync_long | BEAT SYNC, long press | 2 | no | Note | 2 | 92 (0x5C) | — | sent on release | sync off |
| sync_shift | BEAT SYNC (TEMPO RANGE) | 2 | yes | Note | 2 | 96 (0x60) | — | sent on release | tempo range 8 → 16 → 50 % |
| tempo | TEMPO slider | 1 | either | CC | 1 | 0 (0x00) | 32 (0x20) | 14-bit; 0 at the "−" end, 16383 at the "+" end | `deck.pitch` within the deck's range, with soft takeover |
| tempo | TEMPO slider | 2 | either | CC | 2 | 0 (0x00) | 32 (0x20) | 14-bit; 0 at the "−" end, 16383 at the "+" end | `deck.pitch` within the deck's range, with soft takeover |
| mode_hot_cue | HOT CUE mode button | 1 | no | Note | 1 | 27 (0x1B) | — | press | pad mode: hot cue |
| mode_keyboard | HOT CUE mode button (KEYBOARD) | 1 | yes | Note | 1 | 105 (0x69) | — | press | pad mode: keyboard (pads unmapped) |
| mode_pad_fx1 | PAD FX 1 mode button | 1 | no | Note | 1 | 30 (0x1E) | — | press | pad mode: pad FX 1 (pads unmapped) |
| mode_pad_fx2 | PAD FX 1 mode button (PAD FX 2) | 1 | yes | Note | 1 | 107 (0x6B) | — | press | pad mode: pad FX 2 (pads unmapped) |
| mode_beat_jump | BEAT JUMP mode button | 1 | no | Note | 1 | 32 (0x20) | — | press | pad mode: beat jump |
| mode_beat_loop | BEAT JUMP mode button (BEAT LOOP) | 1 | yes | Note | 1 | 109 (0x6D) | — | press | pad mode: beat loop |
| mode_sampler | SAMPLER mode button | 1 | no | Note | 1 | 34 (0x22) | — | press | pad mode: sampler (pads unmapped) |
| mode_key_shift | SAMPLER mode button (KEY SHIFT) | 1 | yes | Note | 1 | 111 (0x6F) | — | press | pad mode: key shift (pads unmapped) |
| mode_hot_cue | HOT CUE mode button | 2 | no | Note | 2 | 27 (0x1B) | — | press | pad mode: hot cue |
| mode_keyboard | HOT CUE mode button (KEYBOARD) | 2 | yes | Note | 2 | 105 (0x69) | — | press | pad mode: keyboard (pads unmapped) |
| mode_pad_fx1 | PAD FX 1 mode button | 2 | no | Note | 2 | 30 (0x1E) | — | press | pad mode: pad FX 1 (pads unmapped) |
| mode_pad_fx2 | PAD FX 1 mode button (PAD FX 2) | 2 | yes | Note | 2 | 107 (0x6B) | — | press | pad mode: pad FX 2 (pads unmapped) |
| mode_beat_jump | BEAT JUMP mode button | 2 | no | Note | 2 | 32 (0x20) | — | press | pad mode: beat jump |
| mode_beat_loop | BEAT JUMP mode button (BEAT LOOP) | 2 | yes | Note | 2 | 109 (0x6D) | — | press | pad mode: beat loop |
| mode_sampler | SAMPLER mode button | 2 | no | Note | 2 | 34 (0x22) | — | press | pad mode: sampler (pads unmapped) |
| mode_key_shift | SAMPLER mode button (KEY SHIFT) | 2 | yes | Note | 2 | 111 (0x6F) | — | press | pad mode: key shift (pads unmapped) |
| fx_ch_select_10 | FX CH SELECT switch | — | no | Note | 5 | 16 (0x10) | — | position: see below | (unmapped: beat FX) |
| fx_ch_select_11 | FX CH SELECT switch | — | no | Note | 5 | 17 (0x11) | — | position: see below | (unmapped: beat FX) |
| fx_ch_select_10 | FX CH SELECT switch | — | no | Note | 6 | 16 (0x10) | — | position: see below | (unmapped: beat FX) |
| fx_ch_select_11 | FX CH SELECT switch | — | no | Note | 6 | 17 (0x11) | — | position: see below | (unmapped: beat FX) |
| fx_select | FX SELECT | — | no | Note | 5 | 99 (0x63) | — | press | (unmapped: beat FX) |
| fx_select_shift | FX SELECT | — | yes | Note | 5 | 100 (0x64) | — | press | (unmapped: beat FX) |
| fx_beat_back | BEAT ◁ | — | no | Note | 5 | 74 (0x4A) | — | press | (unmapped: beat FX) |
| fx_beat_back_shift | BEAT ◁ | — | yes | Note | 5 | 102 (0x66) | — | press | (unmapped: beat FX) |
| fx_beat_fwd | BEAT ▷ | — | no | Note | 5 | 75 (0x4B) | — | press | (unmapped: beat FX) |
| fx_beat_fwd_shift | BEAT ▷ | — | yes | Note | 5 | 107 (0x6B) | — | press | (unmapped: beat FX) |
| fx_level | LEVEL/DEPTH | — | no | CC | 5 | 2 (0x02) | 34 (0x22) | 14-bit | (unmapped: beat FX) |
| fx_on | FX ON/OFF (CH1, or CH1&CH2) | — | no | Note | 5 | 71 (0x47) | — | press | (unmapped: beat FX) |
| fx_on | FX ON/OFF (CH2) | — | no | Note | 6 | 71 (0x47) | — | press | (unmapped: beat FX) |
| fx_on_shift | FX ON/OFF (CH1, or CH1&CH2) | — | yes | Note | 5 | 67 (0x43) | — | press | (unmapped: beat FX) |
| fx_on_shift | FX ON/OFF (CH2) | — | yes | Note | 6 | 67 (0x43) | — | press | (unmapped: beat FX) |
| master_level | MASTER LEVEL | — | no | CC | 7 | 8 (0x08) | 40 (0x28) | 14-bit | `deck.master_volume` (−60..0 dB, as the on-screen knob) |
| master_cue | MASTER CUE | — | no | Note | 7 | 99 (0x63) | — | press | `deck.master_cue` on / off (the master joins the headphones' cue bus) |
| master_cue_shift | MASTER CUE | — | yes | Note | 7 | 120 (0x78) | — | press | (unmapped) |
| trim | TRIM | 1 | no | CC | 1 | 4 (0x04) | 36 (0x24) | 14-bit | `deck.trim` (as the on-screen knob, 0 dB in the centre) |
| trim | TRIM | 2 | no | CC | 2 | 4 (0x04) | 36 (0x24) | 14-bit | `deck.trim` (as the on-screen knob, 0 dB in the centre) |
| eq_hi | EQ HI | 1 | no | CC | 1 | 7 (0x07) | 39 (0x27) | 14-bit | `deck.eq` high (as the on-screen knob) |
| eq_hi | EQ HI | 2 | no | CC | 2 | 7 (0x07) | 39 (0x27) | 14-bit | `deck.eq` high (as the on-screen knob) |
| eq_mid | EQ MID | 1 | no | CC | 1 | 11 (0x0B) | 43 (0x2B) | 14-bit | `deck.eq` mid (as the on-screen knob) |
| eq_mid | EQ MID | 2 | no | CC | 2 | 11 (0x0B) | 43 (0x2B) | 14-bit | `deck.eq` mid (as the on-screen knob) |
| eq_low | EQ LOW | 1 | no | CC | 1 | 15 (0x0F) | 47 (0x2F) | 14-bit | `deck.eq` low (as the on-screen knob) |
| eq_low | EQ LOW | 2 | no | CC | 2 | 15 (0x0F) | 47 (0x2F) | 14-bit | `deck.eq` low (as the on-screen knob) |
| cfx | CFX | 1 | no | CC | 7 | 23 (0x17) | 55 (0x37) | 14-bit | `deck.filter` (−1..1, off in the centre) |
| cfx | CFX | 2 | no | CC | 7 | 24 (0x18) | 56 (0x38) | 14-bit | `deck.filter` (−1..1, off in the centre) |
| ch_cue | CH CUE (headphone cue) | 1 | no | Note | 1 | 84 (0x54) | — | press | `deck.headphone_cue` on / off |
| ch_cue_shift | CH CUE | 1 | yes | Note | 1 | 104 (0x68) | — | press | (unmapped) |
| ch_cue | CH CUE (headphone cue) | 2 | no | Note | 2 | 84 (0x54) | — | press | `deck.headphone_cue` on / off |
| ch_cue_shift | CH CUE | 2 | yes | Note | 2 | 104 (0x68) | — | press | (unmapped) |
| fader | CH FADER | 1 | no | CC | 1 | 19 (0x13) | 51 (0x33) | 14-bit; 0 at the bottom | `deck.fader` 0..1 |
| fader | CH FADER | 2 | no | CC | 2 | 19 (0x13) | 51 (0x33) | 14-bit; 0 at the bottom | `deck.fader` 0..1 |
| fader_start_play | CH FADER (fader start), up from the bottom | 1 | yes | Note | 1 | 102 (0x66) | — | press | (unmapped: fader start) |
| fader_start_cue | CH FADER (fader start), back to the bottom | 1 | yes | Note | 1 | 82 (0x52) | — | press | (unmapped: fader start) |
| fader_start_play | CH FADER (fader start), up from the bottom | 2 | yes | Note | 2 | 102 (0x66) | — | press | (unmapped: fader start) |
| fader_start_cue | CH FADER (fader start), back to the bottom | 2 | yes | Note | 2 | 82 (0x52) | — | press | (unmapped: fader start) |
| crossfader | CROSSFADER | — | no | CC | 7 | 31 (0x1F) | 63 (0x3F) | 14-bit; 0 at the left | `deck.xfader` −1 (A) .. 1 (B) |
| mic_level | MIC LEVEL | — | no | CC | 7 | 5 (0x05) | 37 (0x25) | 14-bit | (unmapped) |
| smart_cfx | SMART CFX | — | no | Note | 7 | 0 (0x00) | — | press | (unmapped) |
| smart_cfx_shift | SMART CFX | — | yes | Note | 7 | 8 (0x08) | — | press | (unmapped) |
| headphone_mix | HEADPHONE MIX | — | no | CC | 7 | 12 (0x0C) | 44 (0x2C) | 14-bit | `deck.headphone_mix` 0 (cue) .. 1 (master) |
| headphone_level | HEADPHONE LEVEL | — | no | CC | 7 | 13 (0x0D) | 45 (0x2D) | 14-bit | unmapped by default (the hardware's own level); with the device's option "FLX4 headphone knob controls djforge's level", `deck.headphone_level` −60..0 dB |
| smart_fader | SMART FADER | — | no | Note | 7 | 1 (0x01) | — | press | (unmapped) |
| smart_fader_shift | SMART FADER | — | yes | Note | 7 | 9 (0x09) | — | press | (unmapped) |
| android_mono_stereo | Android MONO/STEREO switch (rear) | — | no | Note | 7 | 109 (0x6D) | — | STEREO 0x00, MONO 0x7F | (unmapped) |
| browse | BROWSE knob, turned | — | no | CC | 7 | 64 (0x40) | — | relative, two's complement | moves the library selection one row per click |
| browse_shift | BROWSE knob, turned | — | yes | CC | 7 | 100 (0x64) | — | relative, two's complement | moves the library selection 8 rows per click |
| browse_press | BROWSE knob, pressed | — | no | Note | 7 | 65 (0x41) | — | press | loads the selected row on the first empty deck |
| browse_press_shift | BROWSE knob, pressed | — | yes | Note | 7 | 66 (0x42) | — | press | (unmapped) |
| load | LOAD (deck 1) | 1 | no | Note | 7 | 70 (0x46) | — | press | loads the selected row on Deck A (refused while it plays: Load Lock) |
| load_shift | LOAD (deck 1) | 1 | yes | Note | 7 | 104 (0x68) | — | press | (unmapped) |
| load | LOAD (deck 2) | 2 | no | Note | 7 | 71 (0x47) | — | press | loads the selected row on Deck B (refused while it plays: Load Lock) |
| load_shift | LOAD (deck 2) | 2 | yes | Note | 7 | 122 (0x7A) | — | press | (unmapped) |

FX CH SELECT is a three-position switch reported as four notes, Note On `0x7F` or `0x00`:

| Position | Ch 5 note 16 | Ch 5 note 17 | Ch 6 note 16 | Ch 6 note 17 |
|---|---|---|---|---|
| CH1 | 0x7F | 0x00 | 0x00 | 0x00 |
| CH2 | 0x00 | 0x00 | 0x00 | 0x7F |
| CH1&CH2 | 0x7F | 0x00 | 0x00 | 0x7F |

LEVEL/DEPTH is listed with channel 6 in the "MIDI Channel" column but status byte `0xB4`
(channel 5) and controllers 2 / 34. djforge reads the status byte: channel 5.

FX ON/OFF: footnote \*4 of the list says the button blinks when it receives a Note On and lights
when it receives a Note Off. The list gives it no MIDI-OUT row and djforge never sends it
anything, not even when the layout stops (a Note Off, or a Note On with velocity 0, would light
it).

Headphones: djforge plays its headphone output on the controller's outputs 3–4 (master on 1–2)
when the device opens with 4 outputs (Setup › Playback Engine › Headphones on outputs). HEADPHONE
MIX drives djforge's mix. HEADPHONE LEVEL is left to the hardware by default, on the assumption
that it sets the headphone amplifier's level itself (mapping it too would turn the level down
twice); djforge's own headphone level starts at −12 dB. MIDI Devices has a per-device option,
"FLX4 headphone knob controls djforge's level" (off by default), that maps it to
`deck.headphone_level`. The MASTER CUE light isn't in the list's outputs, so djforge doesn't
drive it.

The crossfader's fader-start messages (SHIFT + crossfader) are listed with channel 7 in the
"MIDI Channel" column but status bytes `0x90` / `0x91` (channels 1 and 2) and notes 102 / 82: the
same messages as the channel faders' fader start. djforge reads the status byte, so they are the
`fader_start_*` rows above.

## Performance pads

Each deck has 8 pads (pad 1–4 on the top row, 5–8 below). The pad mode is kept in the hardware
(the mode buttons switch it); a pad sends a note on the deck's pad channel, `base + pad − 1`, so
the note says the mode. SHIFT + pad sends the same note on the next channel. Press `0x7F`,
release `0x00`. A held pad's release may come on the other channel (SHIFT pressed or let go while
it was held); djforge lets go of it either way.

djforge can't read the hardware's pad mode: it assumes hot cue until a mode button or a pad says
otherwise, and lights the HOT CUE mode LED when the output opens, so the two agree from the start.

| Mode | Shift | Deck | Ch | Pad 1 note | djforge |
|---|---|---|---|---|---|
| hot_cue | no | 1 | 8 | 0 (0x00) | hot cue A–H: empty → set; set → jump, held: preview from it while paused |
| hot_cue | yes | 1 | 9 | 0 (0x00) | clear the hot cue |
| pad_fx1 | no | 1 | 8 | 16 (0x10) | (unmapped: pad FX) |
| pad_fx1 | yes | 1 | 9 | 16 (0x10) | (unmapped: pad FX) |
| beat_jump | no | 1 | 8 | 32 (0x20) | beat jump: pads ◁1 1▷ ◁4 4▷ / ◁16 16▷ ◁32 32▷ (beats) |
| beat_jump | yes | 1 | 9 | 32 (0x20) | as without SHIFT |
| sampler | no | 1 | 8 | 48 (0x30) | (unmapped: sampler) |
| sampler | yes | 1 | 9 | 48 (0x30) | (unmapped: sampler) |
| keyboard | no | 1 | 8 | 64 (0x40) | (unmapped: keyboard) |
| keyboard | yes | 1 | 9 | 64 (0x40) | (unmapped: keyboard) |
| pad_fx2 | no | 1 | 8 | 80 (0x50) | (unmapped: pad FX) |
| pad_fx2 | yes | 1 | 9 | 80 (0x50) | (unmapped: pad FX) |
| beat_loop | no | 1 | 8 | 96 (0x60) | beat loop: pads 1/4 1/2 1 2 / 4 8 16 32 beats; the size playing again exits |
| beat_loop | yes | 1 | 9 | 96 (0x60) | as without SHIFT |
| key_shift | no | 1 | 8 | 112 (0x70) | (unmapped: key shift) |
| key_shift | yes | 1 | 9 | 112 (0x70) | (unmapped: key shift) |
| hot_cue | no | 2 | 10 | 0 (0x00) | hot cue A–H: empty → set; set → jump, held: preview from it while paused |
| hot_cue | yes | 2 | 11 | 0 (0x00) | clear the hot cue |
| pad_fx1 | no | 2 | 10 | 16 (0x10) | (unmapped: pad FX) |
| pad_fx1 | yes | 2 | 11 | 16 (0x10) | (unmapped: pad FX) |
| beat_jump | no | 2 | 10 | 32 (0x20) | beat jump: pads ◁1 1▷ ◁4 4▷ / ◁16 16▷ ◁32 32▷ (beats) |
| beat_jump | yes | 2 | 11 | 32 (0x20) | as without SHIFT |
| sampler | no | 2 | 10 | 48 (0x30) | (unmapped: sampler) |
| sampler | yes | 2 | 11 | 48 (0x30) | (unmapped: sampler) |
| keyboard | no | 2 | 10 | 64 (0x40) | (unmapped: keyboard) |
| keyboard | yes | 2 | 11 | 64 (0x40) | (unmapped: keyboard) |
| pad_fx2 | no | 2 | 10 | 80 (0x50) | (unmapped: pad FX) |
| pad_fx2 | yes | 2 | 11 | 80 (0x50) | (unmapped: pad FX) |
| beat_loop | no | 2 | 10 | 96 (0x60) | beat loop: pads 1/4 1/2 1 2 / 4 8 16 32 beats; the size playing again exits |
| beat_loop | yes | 2 | 11 | 96 (0x60) | as without SHIFT |
| key_shift | no | 2 | 10 | 112 (0x70) | (unmapped: key shift) |
| key_shift | yes | 2 | 11 | 112 (0x70) | (unmapped: key shift) |

## Outputs (LEDs)

Every LED takes the same message the control sends, from djforge to the controller. Buttons:
`0x00` off, `0x7F` on. The list documents no other values: no blink and no pad colours, so pads
are on or off, and djforge blinks a LED by switching it.

| LED | Deck | Ch | Message | Values | djforge |
|---|---|---|---|---|---|
| PLAY/PAUSE | 1 / 2 | 1 / 2 | Note 11 (0x0B) | off / on | on while playing; blinking (2 Hz) while paused with a track |
| PLAY/PAUSE (SHIFT layer) | 1 / 2 | 1 / 2 | Note 14 (0x0E) | off / on | mirrors PLAY |
| CUE | 1 / 2 | 1 / 2 | Note 12 (0x0C) | off / on | on while paused at the cue point or previewing; blinking while paused elsewhere |
| CUE (SHIFT layer) | 1 / 2 | 1 / 2 | Note 72 (0x48) | off / on | mirrors CUE |
| IN | 1 / 2 | 1 / 2 | Note 16 (0x10) | off / on | on while a loop plays |
| IN (SHIFT layer) | 1 / 2 | 1 / 2 | Note 76 (0x4C) | off / on | off |
| OUT | 1 / 2 | 1 / 2 | Note 17 (0x11) | off / on | on while a loop plays |
| OUT (SHIFT layer) | 1 / 2 | 1 / 2 | Note 78 (0x4E) | off / on | off |
| BEAT SYNC | 1 / 2 | 1 / 2 | Note 88 (0x58) | off / on | on while synced |
| BEAT SYNC (SHIFT layer) | 1 / 2 | 1 / 2 | Note 96 (0x60) | off / on | on while the deck is the sync master |
| HOT CUE mode | 1 / 2 | 1 / 2 | Note 27 (0x1B) | off / on | on in hot cue mode |
| KEYBOARD mode | 1 / 2 | 1 / 2 | Note 105 (0x69) | off / on | on in keyboard mode |
| PAD FX 1 mode | 1 / 2 | 1 / 2 | Note 30 (0x1E) | off / on | on in pad FX 1 mode |
| PAD FX 2 mode | 1 / 2 | 1 / 2 | Note 107 (0x6B) | off / on | on in pad FX 2 mode |
| BEAT JUMP mode | 1 / 2 | 1 / 2 | Note 32 (0x20) | off / on | on in beat jump mode |
| BEAT LOOP mode | 1 / 2 | 1 / 2 | Note 109 (0x6D) | off / on | on in beat loop mode |
| SAMPLER mode | 1 / 2 | 1 / 2 | Note 34 (0x22) | off / on | on in sampler mode |
| KEY SHIFT mode | 1 / 2 | 1 / 2 | Note 111 (0x6F) | off / on | on in key shift mode |
| CH CUE | 1 / 2 | 1 / 2 | Note 84 (0x54) | off / on | on while the deck is cued to the headphones |
| CH CUE (SHIFT layer) | 1 / 2 | 1 / 2 | Note 104 (0x68) | off / on | mirrors CH CUE |
| Performance pads | 1 / 2 | 8 and 9 / 10 and 11 | the pad's note | off / on | see below |
| CH LEVEL METER | 1 / 2 | 1 / 2 | CC 2 (0x02) | lights from the bottom: green 1 0x26–0x40, green 2 0x41–0x56, orange 1 0x57–0x64, orange 2 0x65–0x76, red 0x77–0x7F | the deck's post-fader peak, at most 30 times a second |
| SMART CFX | — | 7 | Note 0 (0x00) | off / on | off (unmapped) |
| SMART FADER | — | 7 | Note 1 (0x01) | off / on | off (unmapped) |
| LOADED (track load illumination) | 1 / 2 | 16 | Note 0 / 1 (0x00 / 0x01) | 0x7F | sent when a track lands on the deck |
| VINYL MODE (setting) | 1 / 2 | 1 / 2 | Note 23 (0x17) | off / on (default on) | not sent: the hardware keeps its default |
| FX ON/OFF (not in the list's outputs; footnote \*4) | — | 5 / 6 | Note 71 / 67 (0x47 / 0x43) | Note On blinks, Note Off lights | not sent: djforge never drives it, not even when the layout stops |

Pads, in the mode the deck is in (on both the plain and the SHIFT channel):

- **Hot cue:** a pad is on while its hot cue is set.
- **Beat jump:** all on while the deck has a track.
- **Beat loop:** the pad of the loop size playing is on.
- **Pad FX 1/2, sampler, keyboard, key shift:** off (unmapped).

The level meter takes the peak in dB: below −24 dB nothing is lit; −24 dB is the first green
(0x26), −12 dB the second green (0x41), −6 dB the first orange (0x57), −3 dB the second orange
(0x65) and 0 dB red (0x77), linear in dB between; the reading falls at 30 dB a second. Only
changes are sent. When the controller's output opens (or reopens) every LED is sent; when the
layout stops (the device is disabled or unplugged, its layout turned off, or djforge quits)
every LED is turned off, and a held CUE or hot cue preview is let go (also when MIDI Learn
starts). A light counts as sent only once the output took it; if the output's queue dropped
messages, every light is sent again. An output that won't open is tried again after 1 s, then
less often, up to every 30 s.

The controller acts and its lights change in UI frames: while it is connected djforge keeps
the UI running at 30 frames a second while a deck plays (so the meters reach 30 Hz), at 60 while
a nudge is held on the platter, and at 8 for blinking otherwise. If the OS throttles a
minimised window, the lights lag until it is shown again.

Toggles (PLAY, BEAT SYNC, CH CUE) remember the state they asked for until the deck reports it,
so a quick double press sends on then off rather than on twice.

## Device identity

On Windows (WinMM) the controller lists an input and an output, both named "DDJ-FLX4"; the
backend ids differ (each is its own device interface). djforge recognises the controller by its
raw input name containing "DDJ-FLX4" (any case, so CoreMIDI's and ALSA's longer names match too)
and pairs the output by raw name: the output with the input's backend id if there is one, else
the output with the same name at the same position among same-named ports (the second
"DDJ-FLX4" input pairs with the second "DDJ-FLX4" output).

## Jog and tempo details

- The jog sends a count per message (the change since the last). The PDF gives no counts per
  revolution; djforge assumes about 720 per turn. Platter: paused, a full turn searches 2 beats;
  playing, it nudges. SHIFT + platter searches 32 beats per turn. The wheel side nudges while
  playing and searches while paused. Nudges are as strong as the wheel turns fast (a full turn a
  second bends about 7 %), as the on-screen jog.
- The tempo slider maps 0..16383 onto −range..+range of the deck's pitch range (8, 16 or 50 %),
  with ±32 counts around the centre read as 0. Pioneer's "+" end is the end toward the DJ.
  Soft takeover: after the deck's tempo moved elsewhere (a click, sync, a load), the slider does
  nothing until it reaches or crosses the deck's pitch. A deck synced to a master on the other
  deck takes the slider straight away: the engine's own takeover applies (it turns sync off and
  waits for the tempo held). A synced deck that is itself the master waits like any other.

## Hardware checklist

To check on the controller itself:

- **Outputs:** Windows lists the FLX4 with 4 output channels (Sound settings › the FLX4 ›
  speaker setup Quadraphonic if it shows Stereo); djforge's Decks tab then reads
  "Headphones: outputs 3-4". If the device refuses 4 channels djforge opens it in stereo and says
  "Headphones off: the device wouldn't open with 4 channels".
- **HEADPHONE LEVEL:** turning it changes the headphone volume with the option off. If it
  doesn't, turn the option on.
- **HEADPHONE MIX:** it blends cue and master once, not twice (the hardware doesn't also mix
  outputs 1–2 into the headphones).
- **FX ON/OFF:** its light stays as it was when the layout stops (djforge sends it nothing).
- **Latency:** the headphones are about 3 ms behind the master outputs (their limiter's
  lookahead).
