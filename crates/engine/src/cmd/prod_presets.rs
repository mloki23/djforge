//! The built-in trance presets `prod.new_part` puts on a part: a built-in instrument and the
//! parameters it starts from (every other parameter keeps its default).
//!
//! FX patterns play one note (`patterns::FX_PITCH`) and sweep cutoff and volume over it, so
//! every FX preset is a Subtractive Synth (white noise through its low-pass) that sounds on that
//! note.

use soundcraft_midi::patterns::PatternKind;

/// One preset: what it is for, the built-in instrument it uses and that instrument's parameters
/// (ids and values as `mix.instrument` takes them; choices are option indexes).
#[derive(Debug)]
pub(crate) struct Preset {
    pub id: &'static str,
    pub kind: PatternKind,
    pub instrument: &'static str,
    pub params: &'static [(&'static str, f32)],
}

const DRUMS: &str = "drum_synth";
const SYNTH: &str = "subtractive_synth";

// Subtractive Synth oscillator choices.
const SAW: f32 = 0.0;
const SQUARE: f32 = 1.0;
const SINE: f32 = 2.0;
const WHITE_NOISE: f32 = 4.0;

pub(crate) const PRESETS: &[Preset] = &[
    // A short punchy kick that leaves room for a rolling bass, a snappy snare, crisp hats.
    Preset {
        id: "trance_kit",
        kind: PatternKind::Drums,
        instrument: DRUMS,
        params: &[
            ("level", -4.0),
            ("kick_tune", 0.0),
            ("kick_decay", 280.0),
            ("snare_snappy", 85.0),
            ("snare_decay", 150.0),
            ("hat_decay", 40.0),
            ("open_hat_decay", 320.0),
            ("cymbal_decay", 2500.0),
        ],
    },
    // Saws plucked shut by the filter envelope: each 16th is a short "dug".
    Preset {
        id: "rolling_bass",
        kind: PatternKind::Bass,
        instrument: SYNTH,
        params: &[
            ("osc1_wave", SAW),
            ("osc2_wave", SAW),
            ("osc2_semitones", 0.0),
            ("osc2_detune", 6.0),
            ("osc_mix", 35.0),
            ("cutoff", 300.0),
            ("resonance", 30.0),
            ("filter_env_amount", 55.0),
            ("filter_attack", 0.5),
            ("filter_decay", 160.0),
            ("filter_sustain", 0.0),
            ("filter_release", 80.0),
            ("amp_attack", 1.0),
            ("amp_decay", 220.0),
            ("amp_sustain", 0.0),
            ("amp_release", 60.0),
            ("glide", 0.0),
            ("velocity", 40.0),
            ("level", 4.0),
        ],
    },
    // A rounder off-beat bass: saw over a square an octave down, a little longer.
    Preset {
        id: "offbeat_bass",
        kind: PatternKind::Bass,
        instrument: SYNTH,
        params: &[
            ("osc1_wave", SAW),
            ("osc2_wave", SQUARE),
            ("osc2_semitones", -12.0),
            ("osc2_detune", 0.0),
            ("osc_mix", 40.0),
            ("cutoff", 300.0),
            ("resonance", 20.0),
            ("filter_env_amount", 40.0),
            ("filter_attack", 0.5),
            ("filter_decay", 180.0),
            ("filter_sustain", 10.0),
            ("filter_release", 80.0),
            ("amp_attack", 1.0),
            ("amp_decay", 180.0),
            ("amp_sustain", 30.0),
            ("amp_release", 60.0),
            ("velocity", 40.0),
            ("level", 2.0),
        ],
    },
    // A pure sine sub, held for as long as the note.
    Preset {
        id: "sub_bass",
        kind: PatternKind::Bass,
        instrument: SYNTH,
        params: &[
            ("osc1_wave", SINE),
            ("osc2_wave", SINE),
            ("osc2_semitones", 0.0),
            ("osc2_detune", 0.0),
            ("osc_mix", 0.0),
            ("cutoff", 400.0),
            ("resonance", 0.0),
            ("filter_env_amount", 0.0),
            ("amp_attack", 3.0),
            ("amp_decay", 300.0),
            ("amp_sustain", 100.0),
            ("amp_release", 80.0),
            ("velocity", 30.0),
            ("level", 0.0),
        ],
    },
    // Two detuned saws (Osc 2 Detune), short and bright: off-beat chord stabs.
    Preset {
        id: "stab",
        kind: PatternKind::Synth,
        instrument: SYNTH,
        params: &[
            ("osc1_wave", SAW),
            ("osc2_wave", SAW),
            ("osc2_semitones", 0.0),
            ("osc2_detune", 15.0),
            ("osc_mix", 50.0),
            ("cutoff", 1500.0),
            ("resonance", 25.0),
            ("filter_env_amount", 60.0),
            ("filter_attack", 0.5),
            ("filter_decay", 260.0),
            ("filter_sustain", 0.0),
            ("filter_release", 150.0),
            ("amp_attack", 1.0),
            ("amp_decay", 350.0),
            ("amp_sustain", 0.0),
            ("amp_release", 150.0),
            ("velocity", 50.0),
            ("level", 2.0),
        ],
    },
    // A saw and a square an octave up, closing fast with a little resonance: arps.
    Preset {
        id: "pluck",
        kind: PatternKind::Synth,
        instrument: SYNTH,
        params: &[
            ("osc1_wave", SAW),
            ("osc2_wave", SQUARE),
            ("osc2_semitones", 12.0),
            ("osc2_detune", 4.0),
            ("osc_mix", 35.0),
            ("cutoff", 600.0),
            ("resonance", 35.0),
            ("filter_env_amount", 70.0),
            ("filter_attack", 0.5),
            ("filter_decay", 160.0),
            ("filter_sustain", 0.0),
            ("filter_release", 200.0),
            ("amp_attack", 0.5),
            ("amp_decay", 400.0),
            ("amp_sustain", 0.0),
            ("amp_release", 250.0),
            ("velocity", 50.0),
            ("level", 0.0),
        ],
    },
    // Wide detuned saws that sustain, with a touch of glide between notes.
    Preset {
        id: "lead",
        kind: PatternKind::Synth,
        instrument: SYNTH,
        params: &[
            ("osc1_wave", SAW),
            ("osc2_wave", SAW),
            ("osc2_semitones", 0.0),
            ("osc2_detune", 12.0),
            ("osc_mix", 50.0),
            ("cutoff", 3500.0),
            ("resonance", 15.0),
            ("filter_env_amount", 25.0),
            ("filter_attack", 5.0),
            ("filter_decay", 600.0),
            ("filter_sustain", 50.0),
            ("filter_release", 400.0),
            ("amp_attack", 8.0),
            ("amp_decay", 300.0),
            ("amp_sustain", 85.0),
            ("amp_release", 400.0),
            ("glide", 40.0),
            ("velocity", 40.0),
            ("level", 0.0),
        ],
    },
    // White noise through a resonant low-pass that opens over five seconds as it fades in; the
    // pattern's cutoff and volume sweep ride on top, so the cutoff starts low.
    Preset {
        id: "noise_riser",
        kind: PatternKind::Fx,
        instrument: SYNTH,
        params: &[
            ("osc1_wave", WHITE_NOISE),
            ("osc2_wave", WHITE_NOISE),
            ("osc_mix", 0.0),
            ("cutoff", 300.0),
            ("resonance", 45.0),
            ("filter_env_amount", 60.0),
            ("filter_attack", 5000.0),
            ("filter_decay", 1000.0),
            ("filter_sustain", 100.0),
            ("filter_release", 800.0),
            ("amp_attack", 4000.0),
            ("amp_decay", 1.0),
            ("amp_sustain", 100.0),
            ("amp_release", 800.0),
            ("velocity", 0.0),
            ("level", 3.0),
        ],
    },
    // The riser in reverse: white noise with a detuned saw for body, open at once and closing
    // over four seconds; the pattern's volume sweep fades it, so it holds while the note does.
    Preset {
        id: "downlifter",
        kind: PatternKind::Fx,
        instrument: SYNTH,
        params: &[
            ("osc1_wave", WHITE_NOISE),
            ("osc2_wave", SAW),
            ("osc2_semitones", 0.0),
            ("osc2_detune", 30.0),
            ("osc_mix", 30.0),
            ("cutoff", 5000.0),
            ("resonance", 30.0),
            ("filter_env_amount", -60.0),
            ("filter_attack", 4000.0),
            ("filter_decay", 1000.0),
            ("filter_sustain", 100.0),
            ("filter_release", 1000.0),
            ("amp_attack", 1.0),
            ("amp_decay", 1.0),
            ("amp_sustain", 100.0),
            ("amp_release", 1000.0),
            ("velocity", 0.0),
            ("level", 2.0),
        ],
    },
    // A burst of white noise the filter envelope sweeps shut, over a sine two octaves down,
    // ringing out for seconds after the pattern's beat-long note.
    Preset {
        id: "impact",
        kind: PatternKind::Fx,
        instrument: SYNTH,
        params: &[
            ("osc1_wave", WHITE_NOISE),
            ("osc2_wave", SINE),
            ("osc2_semitones", -24.0),
            ("osc2_detune", 0.0),
            ("osc_mix", 70.0),
            ("cutoff", 150.0),
            ("resonance", 10.0),
            ("filter_env_amount", 100.0),
            ("filter_attack", 0.5),
            ("filter_decay", 200.0),
            ("filter_sustain", 0.0),
            ("filter_release", 200.0),
            ("amp_attack", 0.5),
            ("amp_decay", 3000.0),
            ("amp_sustain", 0.0),
            ("amp_release", 3000.0),
            ("velocity", 30.0),
            ("level", 3.0),
        ],
    },
];

/// The preset a new part of `kind` starts with.
pub(crate) fn default_preset(kind: PatternKind) -> &'static str {
    match kind {
        PatternKind::Drums => "trance_kit",
        PatternKind::Bass => "rolling_bass",
        PatternKind::Synth => "stab",
        PatternKind::Fx => "noise_riser",
    }
}

/// The preset called `id`.
pub(crate) fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_names_real_parameters_in_range() {
        for p in PRESETS {
            let info = soundcraft_dsp::plugin_info(p.instrument).unwrap_or_else(|| panic!("{}: no plugin {}", p.id, p.instrument));
            assert!(info.is_instrument, "{}", p.id);
            for (k, v) in p.params {
                let pi = info.param(k).unwrap_or_else(|| panic!("{}: {} has no `{k}`", p.id, p.instrument));
                assert_eq!(pi.clamp(*v), *v, "{}: `{k}` = {v} is out of range", p.id);
            }
            assert_eq!(PRESETS.iter().filter(|q| q.id == p.id).count(), 1, "{} is listed once", p.id);
        }
        for k in PatternKind::ALL {
            assert_eq!(preset(default_preset(k)).map(|p| p.kind), Some(k), "{}'s default is one of its own", k.name());
        }
    }
}
