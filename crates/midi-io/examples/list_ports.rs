//! Lists the MIDI inputs this machine has (does not open them):
//! `cargo run -p soundcraft-midi-io --example list_ports`.

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    use soundcraft_midi_io::{MidiClock, MidiInputs, MidiSource};
    match MidiInputs::new(MidiClock::new()) {
        Ok(inputs) => {
            let ports = inputs.list();
            println!("{} MIDI input(s)", ports.len());
            for p in ports {
                println!("  [{}] {}", p.index, p.name);
            }
        }
        Err(e) => eprintln!("{e}"),
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
