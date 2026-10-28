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
/// The colours are NI's defaults.
const NI_ORDER: [(usize, &str, &str); 4] = [(1, "Drums", "#009E73"), (2, "Bass", "#D55E00"), (3, "Other", "#CC79A7"), (0, "Vocals", "#56B4E9")];
/// How the temporary folder of a stem file starts: `<this><pid>-<n>`.
pub(crate) const TMP_PREFIX: &str = ".djforge-stems-";
/// Most bytes of a `moov` box read to find a stem file's metadata.
const MAX_MOOV: u64 = 64 << 20;
/// What marks a stem file an export wrote: a key of its metadata.
const MARK: &str = "djforge";
/// Stems this many frames longer or shorter than the mix are padded or trimmed; more is refused.
const SLACK_SECS: f64 = 0.05;
/// The highest sample a track may hold: ALAC stores integers, so more would clip.
const FULL_SCALE: f32 = 0.999;
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

/// The `stem` box's JSON: NI's version 1 metadata with its full mastering DSP parameter set,
/// switched off (the stems play as they are; the values are neutral settings written by hand),
/// the stem names and NI's default colours, and djforge's mark.
pub(crate) fn metadata() -> String {
    let stems: Vec<serde_json::Value> = NI_ORDER.iter().map(|(_, name, color)| serde_json::json!({"name": name, "color": color})).collect();
    serde_json::json!({
        "version": 1,
        "mastering_dsp": {
            "compressor": {
                "enabled": false,
                "input_gain": 0.5,
                "output_gain": 0.5,
                "threshold": 0.0,
                "dry_wet": 50,
                "attack": 0.0001,
                "release": 0.3,
                "ratio": 3,
                "hp_cutoff": 300,
            },
            "limiter": {"enabled": false, "threshold": 0.0, "ceiling": -0.35, "release": 0.05},
        },
        "stems": stems,
        MARK: {"written_by": "djforge dj.export_set"},
    })
    .to_string()
}

/// The `moov` box of the MP4 file at `path`, read alone (top-level box headers walked, then
/// `moov` read whole when it is at most 64 MB); `None` when there is none to be had.
fn moov_of(path: &Path) -> Option<Vec<u8>> {
    let mut pos = 0u64;
    for _ in 0..10_000 {
        let head = crate::io::read_at(path, pos, 8).ok()?;
        let size = u64::from(u32::from_be_bytes([*head.first()?, *head.get(1)?, *head.get(2)?, *head.get(3)?]));
        if size < 8 {
            return None;
        }
        if head.get(4..8)? == b"moov" {
            return (size <= MAX_MOOV).then(|| crate::io::read_at(path, pos, usize::try_from(size).ok()?).ok()).flatten();
        }
        pos = pos.checked_add(size)?;
    }
    None
}

/// Removes the temporary folders of stem files in `out` that a process no longer running left.
pub(crate) fn sweep(out: &Path) {
    for entry in crate::io::list_entries(out).unwrap_or_default() {
        let pid = entry
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(TMP_PREFIX))
            .and_then(|r| r.split('-').next())
            .and_then(|p| p.parse::<u32>().ok());
        if let Some(pid) = pid
            && pid != std::process::id()
            && !crate::cmd::stems::pid_running(pid)
        {
            let _ = crate::io::remove_tree(&entry);
        }
    }
}

/// Why `path` may not be replaced: it exists and is not a stem file an export wrote.
pub(crate) fn foreign(path: &Path) -> Option<String> {
    if !crate::io::path_exists(path) {
        return None;
    }
    // Only the moov box is read: the audio can be hundreds of megabytes.
    let ours = moov_of(path).is_some_and(|b| {
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
/// is removed whatever happens. Returns where it went, and a doubt when it plays quieter than
/// the exported file (so that no stem clips).
pub(crate) fn write(j: &Job) -> Result<(PathBuf, Option<String>), String> {
    if let Some(why) = foreign(j.dest) {
        return Err(why);
    }
    // A folder of its own: one already there is not ours to fill.
    crate::io::make_new_dir(j.tmp).map_err(|e| e.to_string())?;
    let r = build(j);
    let _ = crate::io::remove_tree(j.tmp);
    r.map(|doubt| (j.dest.to_path_buf(), doubt))
}

/// The highest |sample| of `buf`.
fn peak(buf: &AudioBuffer) -> f32 {
    buf.channels.iter().flatten().fold(0.0f32, |m, x| m.max(x.abs()))
}

/// How many halvings bring `peak` under [`FULL_SCALE`]: 0 when it is under already.
fn halvings(peak: f32) -> i32 {
    let mut k = 0;
    while k < 60 && peak * 2f32.powi(-k) >= FULL_SCALE {
        k += 1;
    }
    k
}

fn build(j: &Job) -> Result<Option<String>, String> {
    let wav = |name: &str| j.tmp.join(name);
    let bytes = crate::io::read_file(j.mix).map_err(|e| e.to_string())?;
    let ext = j.mix.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase);
    let (_, mix) = soundcraft_audio_io::decode(&bytes, ext.as_deref()).map_err(|e| format!("{}: {e}", j.mix.display()))?;
    // The decode is gapless, but the set's grid and cues are on mixforge's timeline, which keeps
    // an MP3's priming: every track starts with that much silence so they fit the stem file.
    let priming = if ext.as_deref() == Some("mp3") {
        soundcraft_audio_io::mp3gain::gapless_priming(&bytes).filter(|(_, at)| *at == mix.sample_rate).map_or(0, |(n, _)| n as usize)
    } else {
        0
    };
    drop(bytes);
    let (rate, frames) = (mix.sample_rate, mix.frames());
    if frames == 0 || rate == 0 {
        return Err(format!("{}: no audio", j.mix.display()));
    }
    let slack = (SLACK_SECS * f64::from(rate)).ceil() as usize;
    // The stems come from the session's pool, at the session rate: when that is not the file's,
    // they were resampled once from the file and are resampled back here (a small loss at the
    // top of the band; reading the cache's stems at the file's rate would avoid it).
    let scale = (j.gain * 2f64.powi(i32::from(j.stems.scale_log2))) as f32;
    // A stem can peak above full scale though the mix doesn't, and the WAVs (and ALAC) clip
    // there: then all five tracks are halved until none does (a power of two, so exact), and
    // they still add up to the mix. Guessed from the stems as stored (resampling can overshoot a
    // little), then checked on each stem as written: one over starts again a halving lower.
    let resampled = j.stems.audio.iter().any(|a| a.buffer.sample_rate != rate);
    let stored = j.stems.audio.iter().map(|a| peak(&a.buffer)).fold(0.0f32, f32::max) * scale * if resampled { 1.05 } else { 1.0 };
    let mut k = halvings(peak(&mix).max(stored));
    'pass: loop {
        let factor = 2f32.powi(-k);
        let mut m = mix.clone();
        m.channels.iter_mut().flatten().for_each(|x| *x *= factor);
        write_wav(&wav("0.wav"), stereo(delayed(m, priming))?)?;
        for (n, (index, name, _)) in NI_ORDER.iter().enumerate() {
            let audio = j.stems.audio.get(*index).ok_or("a stem is missing")?;
            let mut stem = crate::io::at_rate(audio.buffer.clone(), rate, crate::io::MAX_IMPORT_SAMPLES)
                .ok_or_else(|| format!("the {name} stem is too long"))?;
            let have = stem.frames();
            if have.abs_diff(frames) > slack {
                return Err(format!("the {name} stem is {have} samples long and the track {frames}: they don't line up"));
            }
            for ch in &mut stem.channels {
                ch.resize(frames, 0.0);
                ch.iter_mut().for_each(|x| *x *= scale * factor);
            }
            if peak(&stem) >= FULL_SCALE {
                if k >= 60 {
                    return Err(format!("the {name} stem has samples that are not finite"));
                }
                k += 1;
                continue 'pass;
            }
            write_wav(&wav(&format!("{}.wav", n + 1)), stereo(delayed(stem, priming))?)?;
        }
        break;
    }
    drop(mix);
    let out = wav("out.mp4");
    run_ffmpeg(j, &out, frames as f64 / f64::from(rate))?;
    let mp4 = crate::io::read_file(&out).map_err(|e| e.to_string())?;
    let file = soundcraft_audio_io::mp4box::insert_stem_box(&mp4, metadata().as_bytes()).map_err(|e| format!("ffmpeg's MP4: {e}"))?;
    drop(mp4);
    // Checked again just before: a file may have taken its place.
    if let Some(why) = foreign(j.dest) {
        return Err(why);
    }
    crate::io::replace_file_refusing(j.sources, j.dest, &file).map_err(|e| e.to_string())?;
    Ok((k > 0).then(|| format!("the stem file plays {:.1} dB quieter than the prepped copy so no stem clips", 20.0 * f64::from(k) * 2f64.log10())))
}

/// `buf` with `frames` of silence before it.
fn delayed(mut buf: AudioBuffer, frames: usize) -> AudioBuffer {
    if frames > 0 {
        for ch in &mut buf.channels {
            ch.splice(0..0, std::iter::repeat_n(0.0, frames));
        }
    }
    buf
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
        let colors: Vec<&str> = v["stems"].as_array().unwrap().iter().map(|s| s["color"].as_str().unwrap()).collect();
        assert_eq!(colors, ["#009E73", "#D55E00", "#CC79A7", "#56B4E9"]);
        let dsp = &v["mastering_dsp"];
        for key in ["input_gain", "output_gain", "threshold", "dry_wet", "attack", "release", "ratio", "hp_cutoff"] {
            assert!(dsp["compressor"][key].is_number(), "compressor {key}");
        }
        for key in ["threshold", "ceiling", "release"] {
            assert!(dsp["limiter"][key].is_number(), "limiter {key}");
        }
        assert_eq!(dsp["compressor"]["enabled"], false);
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
    fn peaks_are_halved_under_full_scale() {
        assert_eq!(halvings(0.5), 0);
        assert_eq!(halvings(0.999), 1);
        assert_eq!(halvings(1.6), 1);
        assert_eq!(halvings(2.0), 2);
        assert_eq!(halvings(f32::INFINITY), 60);
    }

    #[test]
    fn the_sweep_removes_only_folders_of_processes_gone() {
        let out = std::env::temp_dir().join(format!("sc-stem-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let out = crate::cmd::export::tests::Folder(out);
        // A pid no process has (Windows pids are multiples of 4), ours, and another name.
        let names = [format!("{TMP_PREFIX}4000001-0"), format!("{TMP_PREFIX}{}-0", std::process::id()), "keep".to_string()];
        for n in &names {
            std::fs::create_dir_all(out.join(n)).unwrap();
        }
        sweep(&out);
        let left: Vec<bool> = names.iter().map(|n| out.join(n).exists()).collect();
        assert_eq!(left, [false, true, true]);
        // A folder that is there already is not written into.
        let tmp = out.join(&names[1]);
        let none = crate::io::SourceFiles::default();
        let job = Job {
            ffmpeg: "x",
            mix: &tmp,
            stems: &Stems {
                audio: std::array::from_fn(|_| Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![] }))),
                scale_log2: 0,
            },
            gain: 1.0,
            dest: &out.join("x.stem.mp4"),
            tmp: &tmp,
            sources: &none,
            stop: &|| false,
        };
        assert!(write(&job).is_err());
    }

    #[test]
    fn a_missing_ffmpeg_is_not_found() {
        assert!(!ffmpeg_found("djforge-no-such-ffmpeg"));
    }
}
