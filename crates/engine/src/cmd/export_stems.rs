//! NI STEM files of an exported set's split tracks: an MP4 of five ALAC tracks (the mix, then
//! drums, bass, other and vocals), with the `stem` metadata box at `moov/udta/stem`.
//!
//! The mix is the exported file decoded; the stems are the split clip's stems from the session
//! (brought back to the file's rate, at their stored scale and the copy's prep gain), so they add
//! up to the mix the decks play. ffmpeg encodes; djforge adds the metadata box itself.

use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use soundcraft_model::SourceAudio;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// NI's stems after the mix, as indices into the session's stem order
/// ([`soundcraft_model::STEMS`]: vocals, drums, bass, other), with their names and colours.
const NI_ORDER: [(usize, &str, &str); 4] = [(1, "Drums", "#FF0000"), (2, "Bass", "#00FF00"), (3, "Other", "#FFFF00"), (0, "Vocals", "#00FFFF")];
/// What marks a stem file an export wrote: a key of its metadata.
const MARK: &str = "djforge";
/// Stems this many frames longer or shorter than the mix are padded or trimmed; more is refused.
const SLACK_SECS: f64 = 0.05;
/// Shortest time ffmpeg is given; longer tracks get four times their length.
const MIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The four stems of a split track, in the session's stem order, and their stored scale.
#[derive(Debug, Clone)]
pub(crate) struct Stems {
    pub audio: [Arc<SourceAudio>; 4],
    /// The stems are stored at 2^-scale_log2 of their level.
    pub scale_log2: u8,
}

/// The ffmpeg to run: `DJFORGE_FFMPEG`, else `ffmpeg` on the PATH.
pub(crate) fn ffmpeg() -> String {
    std::env::var("DJFORGE_FFMPEG").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "ffmpeg".to_string())
}

/// Whether `ffmpeg` runs.
pub(crate) fn ffmpeg_found(ffmpeg: &str) -> bool {
    let mut c = std::process::Command::new(ffmpeg);
    c.args(["-hide_banner", "-version"]).stdin(std::process::Stdio::null());
    hide_window(&mut c);
    matches!(crate::cmd::dj::output_within(&mut c, std::time::Duration::from_secs(20), &|| false), Ok(crate::cmd::dj::Sidecar::Ran(out)) if out.status.success())
}

/// The `stem` box's JSON: NI's version 1 metadata (no mastering DSP: the stems play as they
/// are), the stem names and colours, and djforge's mark.
pub(crate) fn metadata() -> String {
    let stems: Vec<serde_json::Value> = NI_ORDER.iter().map(|(_, name, color)| serde_json::json!({"name": name, "color": color})).collect();
    serde_json::json!({
        "version": 1,
        "mastering_dsp": {"compressor": {"enabled": false}, "limiter": {"enabled": false}},
        "stems": stems,
        MARK: {"written_by": "djforge dj.export_set"},
    })
    .to_string()
}

/// Why `path` may not be replaced: it exists and is not a stem file an export wrote.
pub(crate) fn foreign(path: &Path) -> Option<String> {
    if !crate::io::path_exists(path) {
        return None;
    }
    let ours = crate::io::read_file(path).ok().is_some_and(|b| {
        soundcraft_audio_io::mp4box::stem_payload(&b)
            .and_then(|j| serde_json::from_slice::<serde_json::Value>(j).ok())
            .is_some_and(|v| v.get(MARK).is_some())
    });
    (!ours).then(|| format!("{}: exists and is not a stem file an export wrote; move it and export again", path.display()))
}

/// One stem file to write.
pub(crate) struct Job<'a> {
    pub ffmpeg: &'a str,
    /// The exported file: the mix, and the rate and length the stems are brought to.
    pub mix: &'a Path,
    pub stems: &'a Stems,
    /// Prep's gain on the copy (linear), applied to the stems too.
    pub gain: f64,
    /// Where the file goes, and the folder for its temporary files (made, and removed after).
    pub dest: &'a Path,
    pub tmp: &'a Path,
    pub sources: &'a crate::io::SourceFiles,
    /// Stops ffmpeg (an error then).
    pub stop: &'a dyn Fn() -> bool,
}

/// Writes the stem file of `j`, never over a file an export didn't write; the temporary folder
/// is removed whatever happens.
pub(crate) fn write(j: &Job) -> Result<PathBuf, String> {
    if let Some(why) = foreign(j.dest) {
        return Err(why);
    }
    crate::io::make_dir(j.tmp).map_err(|e| e.to_string())?;
    let r = build(j);
    let _ = crate::io::remove_tree(j.tmp);
    r.map(|()| j.dest.to_path_buf())
}

fn build(j: &Job) -> Result<(), String> {
    let wav = |name: &str| j.tmp.join(name);
    let bytes = crate::io::read_file(j.mix).map_err(|e| e.to_string())?;
    let ext = j.mix.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase);
    let (_, mix) = soundcraft_audio_io::decode(&bytes, ext.as_deref()).map_err(|e| format!("{}: {e}", j.mix.display()))?;
    drop(bytes);
    let (rate, frames) = (mix.sample_rate, mix.frames());
    if frames == 0 || rate == 0 {
        return Err(format!("{}: no audio", j.mix.display()));
    }
    write_wav(&wav("0.wav"), stereo(mix)?)?;
    let slack = (SLACK_SECS * f64::from(rate)).ceil() as usize;
    let scale = (j.gain * 2f64.powi(i32::from(j.stems.scale_log2))) as f32;
    for (k, (index, name, _)) in NI_ORDER.iter().enumerate() {
        let audio = j.stems.audio.get(*index).ok_or("a stem is missing")?;
        let mut stem =
            crate::io::at_rate(audio.buffer.clone(), rate, crate::io::MAX_IMPORT_SAMPLES).ok_or_else(|| format!("the {name} stem is too long"))?;
        let have = stem.frames();
        if have.abs_diff(frames) > slack {
            return Err(format!("the {name} stem is {have} samples long and the track {frames}: they don't line up"));
        }
        for ch in &mut stem.channels {
            ch.resize(frames, 0.0);
            ch.iter_mut().for_each(|x| *x *= scale);
        }
        write_wav(&wav(&format!("{}.wav", k + 1)), stereo(stem)?)?;
    }
    let out = wav("out.mp4");
    run_ffmpeg(j, &out, frames as f64 / f64::from(rate))?;
    let mp4 = crate::io::read_file(&out).map_err(|e| e.to_string())?;
    let file = soundcraft_audio_io::mp4box::insert_stem_box(&mp4, metadata().as_bytes()).map_err(|e| format!("ffmpeg's MP4: {e}"))?;
    drop(mp4);
    // Checked again just before: a file may have taken its place.
    if let Some(why) = foreign(j.dest) {
        return Err(why);
    }
    crate::io::replace_file_refusing(j.sources, j.dest, &file).map_err(|e| e.to_string())
}

/// `buf` as two channels: a mono one doubled, more than two cut to the first two.
fn stereo(mut buf: AudioBuffer) -> Result<AudioBuffer, String> {
    match buf.channels.len() {
        0 => Err("no channels".into()),
        1 => {
            let c = buf.channels.first().cloned().unwrap_or_default();
            buf.channels.push(c);
            Ok(buf)
        }
        _ => {
            buf.channels.truncate(2);
            Ok(buf)
        }
    }
}

fn write_wav(path: &Path, buf: AudioBuffer) -> Result<(), String> {
    let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None };
    let bytes = soundcraft_audio_io::encode(&buf, &opts).map_err(|e| e.to_string())?;
    drop(buf);
    let p = path.to_str().ok_or_else(|| format!("{}: not a Unicode path", path.display()))?;
    crate::io::write_file(p, &bytes).map_err(|e| e.to_string())
}

/// ffmpeg muxing the five WAVs of `j` into ALAC tracks of an MP4 at `out` (the mix the default
/// track), killed after four times the audio's `secs` (at least two minutes) or when stopped.
fn run_ffmpeg(j: &Job, out: &Path, secs: f64) -> Result<(), String> {
    let mut c = std::process::Command::new(j.ffmpeg);
    c.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    for i in 0..5 {
        c.arg("-i").arg(j.tmp.join(format!("{i}.wav")));
    }
    for i in 0..5 {
        c.args(["-map", &format!("{i}:a")]);
    }
    c.args(["-c:a", "alac", "-disposition:a:0", "default"]);
    for i in 1..5 {
        c.args([&format!("-disposition:a:{i}"), "0"]);
    }
    c.args(["-f", "mp4"]).arg(out).stdin(std::process::Stdio::null());
    hide_window(&mut c);
    let four = if secs.is_finite() && secs > 0.0 { std::time::Duration::from_secs((secs * 4.0).ceil().min(1e7) as u64) } else { MIN_TIMEOUT };
    match crate::cmd::dj::output_within(&mut c, MIN_TIMEOUT.max(four), j.stop) {
        Ok(crate::cmd::dj::Sidecar::Ran(o)) if o.status.success() => Ok(()),
        Ok(crate::cmd::dj::Sidecar::Ran(o)) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let lines: Vec<&str> = err.lines().collect();
            Err(format!("ffmpeg failed ({}): {}", o.status, lines.get(lines.len().saturating_sub(3)..).unwrap_or_default().join(" | ")))
        }
        Ok(crate::cmd::dj::Sidecar::TimedOut) => Err("ffmpeg timed out".into()),
        Ok(crate::cmd::dj::Sidecar::LeftHolding) => Err("ffmpeg left a process holding its output".into()),
        Ok(crate::cmd::dj::Sidecar::Stopped) => Err("cancelled".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("ffmpeg not found".into()),
        Err(e) => Err(format!("could not run ffmpeg ({}): {e}", j.ffmpeg)),
    }
}

/// Keeps ffmpeg from flashing a console window over the app on Windows.
fn hide_window(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_metadata_names_ni_s_stems_in_order_and_carries_the_mark() {
        let v: serde_json::Value = serde_json::from_str(&metadata()).unwrap();
        assert_eq!(v["version"], 1);
        let names: Vec<&str> = v["stems"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Drums", "Bass", "Other", "Vocals"]);
        // Each is the session stem of that name.
        for (i, _, _) in NI_ORDER {
            assert!(soundcraft_model::STEMS.get(i).is_some());
        }
        let picked: Vec<&str> = NI_ORDER.iter().map(|(i, _, _)| soundcraft_model::STEMS[*i]).collect();
        assert_eq!(picked, ["drums", "bass", "other", "vocals"]);
        assert!(v[MARK].is_object());
        assert_eq!(v["mastering_dsp"]["limiter"]["enabled"], false);
    }

    #[test]
    fn a_missing_ffmpeg_is_not_found() {
        assert!(!ffmpeg_found("djforge-no-such-ffmpeg"));
    }
}
