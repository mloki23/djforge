# Stage 7b changes worth knowing

Stage 7b is the hardware stage: MIDI input devices, live play, MIDI recording, and MIDI learn
for the DDJ, the pad grid and the keyboard. What follows changes behaviour you may rely on.

## Live MIDI and the transport

- **Rendering while stopped.** With the transport stopped, the target instrument renders only
  while there is something to hear: live events, keys held, and releases ringing (about 5 s
  plus the plugin's own tail). Going idle throws away the audio rendered ahead. Only the first
  stopped block silences the arrangement's scheduled notes.
- **Seek and play keep held live keys.** A locate, or play from stopped, keeps the keys you hold
  sounding; only the arrangement's and slots' notes are released. Stop and the end of play
  release the live keys, lift sustain and reset the plugins.
- **Sustain is per source.** Two devices with sustain pedals don't lift each other's: the
  instrument's sustain lifts when the last pedal does. Unplugging one device lifts only its own.
- **A real panic.** `event.all_notes_off` and `midi.panic` turn off every live note, lift
  sustain and reset the controllers. Before, `event.all_notes_off` did nothing.

## Devices and mappings are app preferences

- MIDI input devices and MIDI mappings are app preferences, per machine, not part of the
  session. `setup.midi_input_devices` is deprecated: it opens MIDI Devices, or enables and
  disables by name like `midi.input_enable`. Sessions drop old `midi.input.*` flags on load.
- **A device on a new USB port keeps its settings.** On Windows a device's id is its USB path,
  which changes in another socket or hub; on Linux it changes on every replug. When an enabled
  device isn't found but exactly one listed port has its name, it is taken to be the same
  device: its enable and its mappings move to the new id, and the status line says "DDJ found
  on a new USB port; settings moved". With several ports of that name nothing is guessed: MIDI
  Devices lists them with "Use settings from …" (`midi.move_settings`).
- That match only looks at names: while an enabled DDJ is unplugged, its settings can move to an
  identical DDJ that was never enabled and is still plugged in. Plug the right one back in, or
  move them back with `midi.move_settings`.
- A failed subscription to the MIDI router is logged once and tried again once a second, not
  every frame. A backend that panics while connecting a port is an error, not a crash.

## MIDI learn

- **Mapped notes and controllers don't play the target.** A pad mapped to a slot launches it and
  doesn't also sound on the instrument. Unmapped keys of the same device still play. This holds
  after an audio device change too: the new audio engine gets the filter again.
- Mapping a switch controller (CC 64–69: sustain and its kin) sends it 0 on the target, so a
  pedal held while it is mapped can't stick.
- MIDI moves of volume, pan and stem faders write Touch/Latch automation, pan lanes included.
- **Removing mappings can be undone.** Remove All asks first. Delete, Remove All and
  `midi.unmap` (the `all: true` form included) keep what they removed in the preferences
  (`midi_mappings_backup`, one level); Undo Remove in the MIDI Mappings window, or
  `midi.restore_mappings`, puts it back.
- The on-screen keyboard has a Play mode that plays the live target. New menu entries:
  Setup › MIDI › MIDI Mappings…, Live MIDI Panic, Event › Recover MIDI Take.

## Recording MIDI

- **Record stop never refuses.** A pass over another clip goes on a new playlist copied from the
  active one, so the rest of the track plays on and the clips under it stay intact on the
  playlist before. With MIDI Merge on, a pass over one MIDI clip merges into it.
- **Long takes are never refused.** Redundant controller events are thinned as they arrive (a
  controller repeating its value is dropped; a second value on one tick replaces the first). A
  take longer than one command (the engine takes 1,000,000 events per `midi.record_take`) is
  placed in several commands inside one undo step, split where no note is held. Over an existing
  clip, each part that overlaps one goes on a new playlist of its own, so such a take can leave
  intermediate playlists (each a copy of the one before, so nothing is lost). A take is
  bounded in memory: past 2,000,000 events controller events are dropped, past 3,000,000 notes
  too, and the status line says how many.
- **The MIDI Input Filter works.** `setup.midi_input_filter`'s kinds (notes, poly and mono
  aftertouch, controllers, program changes, pitch bend) are left out of takes and slot
  recordings. Live play is not filtered: what you play is heard. SysEx and real-time messages
  never reach a take anyway.
- **Recovery list.** The last 8 takes are kept, in memory and in the autosave folder
  (`last_midi_takes.json`, written off the UI thread when large), so they survive a crash.
  Event › Recover MIDI Take ▸ lists them newest first with their age, track and note count;
  `midi.recover_take` places the newest (or `{index}`; `{list: true}` lists them) again on a
  new playlist, one undo step. A take from another session goes on the MIDI track of its name,
  else on a new MIDI track. Takes never placed are not pushed out by placed ones. The file is
  written atomically (a crash mid-write leaves the old copy whole); one that can't be read is
  moved aside as `last_midi_takes.json.corrupt-<time>`, never written over. The old single-take
  `last_midi_take.json` from before is no longer read and can be deleted.
- **Slot recordings.** Editing a playing MIDI slot keeps the pitches still covered sounding;
  notes added behind the playhead send no note-off. A slot recording is refused on an inactive
  column track, cancelled (and kept for recovery) by an audio device change, and while it runs
  the column's notes don't also go into an arrangement take.

## Engine

- **Undo groups.** `Engine::begin_undo_group` / `commit_undo_group` make several commands one
  undo step (nested groups join the outer one). A manual undo point (`push_undo`, as audio
  recording uses) made while a group is open joins the group.
- **Test-only APIs** (`FakeSource`, `MidiSource::as_fake_mut`, `MidiMessage::to_bytes`) are behind
  `soundcraft-midi-io`'s `testing` feature, enabled only through dev-dependencies.
