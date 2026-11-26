//! Audio device selection: which audio system (cpal host), output and input device, sample rate
//! and buffer size to open. The choice is per machine (the app keeps it with its preferences, not
//! in sessions). Anything that cannot be honoured falls back to the system default: a device that
//! is unplugged, a rate or buffer size the device does not offer.

use cpal::traits::{DeviceTrait, HostTrait};

/// Rates offered when a device supports them (any rate inside a supported range is possible, but
/// these are the ones interfaces actually run at).
pub const STANDARD_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
/// Buffer sizes offered when the device reports a range that contains them.
pub const STANDARD_BUFFERS: [u32; 9] = [32, 64, 128, 256, 512, 1024, 2048, 4096, 8192];

/// Sample formats the streams convert to and from, best first (ASIO drivers usually run in
/// 32-bit integers, WASAPI and Core Audio in float).
pub(crate) const FORMATS: [cpal::SampleFormat; 3] = [cpal::SampleFormat::F32, cpal::SampleFormat::I32, cpal::SampleFormat::I16];

/// Preference of a sample format (lower is better); `None` when it is not supported.
pub(crate) fn format_rank(f: cpal::SampleFormat) -> Option<usize> {
    FORMATS.iter().position(|x| *x == f)
}

/// The user's audio device choice. Every field is optional: `None` means "the system default".
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Audio system (cpal host name: "WASAPI", "ASIO", "CoreAudio", "ALSA", "JACK", ...).
    pub system: Option<String>,
    pub output: Option<String>,
    pub input: Option<String>,
    /// Device sample rate; the mix resamples when it differs from the session's.
    pub sample_rate: Option<u32>,
    /// Device buffer in frames.
    pub buffer_size: Option<u32>,
    /// Where each output device (by name) plays the decks' headphone output, as chosen; a
    /// device not listed takes [`default_headphones`].
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty", deserialize_with = "lenient_routes")]
    pub headphones: std::collections::BTreeMap<String, HeadphoneRoute>,
}

/// The kept headphone routes, dropping any entry that isn't one (a malformed or newer
/// preference never loses the rest of the audio choice).
fn lenient_routes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<std::collections::BTreeMap<String, HeadphoneRoute>, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Route(HeadphoneRoute),
        Other(serde::de::IgnoredAny),
    }
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Routes {
        Map(std::collections::BTreeMap<String, Entry>),
        Other(serde::de::IgnoredAny),
    }
    let all = match <Routes as serde::Deserialize>::deserialize(d)? {
        Routes::Map(m) => m,
        Routes::Other(_) => Default::default(),
    };
    Ok(all
        .into_iter()
        .filter_map(|(k, v)| match v {
            Entry::Route(r) => Some((k, r)),
            Entry::Other(_) => None,
        })
        .collect())
}

impl AudioConfig {
    /// Where the headphones play on output device `device` with `max_channels` outputs: the
    /// choice kept for it, else [`default_headphones`].
    pub fn headphones_for(&self, device: &str, max_channels: u16) -> HeadphoneRoute {
        self.headphones.get(device).copied().unwrap_or_else(|| default_headphones(device, max_channels))
    }
}

/// DJ controllers whose built-in sound card has the headphones on outputs 3–4 (matched in the
/// output device's name, any case).
pub const DJ_CONTROLLERS: [&str; 1] = ["DDJ-FLX4"];

/// Where the decks' headphone output (the cue mix) plays on the output device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadphoneRoute {
    /// No headphone output.
    None,
    /// A pair of the device's outputs: `n` and `n + 1` (1-based, from 3: outputs 1–2 are the
    /// master's).
    Outputs(u16),
    /// A 2-channel device split for a splitter cable: the left plays the headphone mix in mono,
    /// the right the master in mono.
    Split,
}

impl HeadphoneRoute {
    /// The route on a device opened with `channels` outputs: a pair it doesn't have (or one
    /// over the master's outputs) is none, and so is a split without two outputs.
    pub fn fit(self, channels: u16) -> HeadphoneRoute {
        match self {
            HeadphoneRoute::Outputs(n) if n >= 3 && u32::from(n) < u32::from(channels) => self,
            HeadphoneRoute::Split if channels >= 2 => self,
            _ => HeadphoneRoute::None,
        }
    }

    /// Output channels the route needs the device opened with (0: no more than the mix needs).
    pub fn channels(self) -> u16 {
        match self {
            HeadphoneRoute::Outputs(n) => n.saturating_add(1),
            HeadphoneRoute::None | HeadphoneRoute::Split => 0,
        }
    }

    /// The routes a device with `max_channels` outputs offers: none, split, then each pair from
    /// outputs 3–4 on.
    pub fn choices(max_channels: u16) -> Vec<HeadphoneRoute> {
        let mut v = vec![HeadphoneRoute::None];
        if max_channels >= 2 {
            v.push(HeadphoneRoute::Split);
        }
        v.extend((3..max_channels).step_by(2).map(HeadphoneRoute::Outputs));
        v
    }

    /// The route in words, for the UI and `decks.state`: "outputs 3-4", "split", "none".
    pub fn label(self) -> String {
        match self {
            HeadphoneRoute::None => "none".into(),
            HeadphoneRoute::Outputs(n) => format!("outputs {n}-{}", n.saturating_add(1)),
            HeadphoneRoute::Split => "split".into(),
        }
    }
}

/// The headphone route a device takes until one is chosen for it: outputs 3–4 on a known DJ
/// controller ([`DJ_CONTROLLERS`]) with at least 4 outputs, else none.
pub fn default_headphones(device: &str, max_channels: u16) -> HeadphoneRoute {
    let name = device.to_ascii_uppercase();
    if max_channels >= 4 && DJ_CONTROLLERS.iter().any(|c| name.contains(c)) { HeadphoneRoute::Outputs(3) } else { HeadphoneRoute::None }
}

/// One device as the settings window lists it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DeviceInfo {
    pub name: String,
    pub is_default: bool,
    /// Most channels any usable configuration offers.
    pub max_channels: u16,
    /// Standard rates the device accepts (in a format the streams convert).
    pub sample_rates: Vec<u32>,
    /// Buffer range the device reports (`None` when it does not say).
    pub buffer_min: Option<u32>,
    pub buffer_max: Option<u32>,
}

impl DeviceInfo {
    /// Standard buffer sizes inside the device's range (all of them when the range is unknown).
    pub fn buffer_sizes(&self) -> Vec<u32> {
        STANDARD_BUFFERS.iter().copied().filter(|b| self.buffer_min.is_none_or(|m| *b >= m) && self.buffer_max.is_none_or(|m| *b <= m)).collect()
    }
}

/// An audio system and its devices.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AudioSystem {
    pub name: String,
    pub is_default: bool,
    pub outputs: Vec<DeviceInfo>,
    pub inputs: Vec<DeviceInfo>,
}

#[allow(deprecated)]
fn device_name(d: &cpal::Device) -> Option<String> {
    d.name().ok()
}

fn describe(d: &cpal::Device, default_name: Option<&str>, output: bool) -> Option<DeviceInfo> {
    let name = device_name(d)?;
    let ranges: Vec<cpal::SupportedStreamConfigRange> = if output {
        d.supported_output_configs().ok()?.filter(|c| format_rank(c.sample_format()).is_some()).collect()
    } else {
        d.supported_input_configs().ok()?.filter(|c| format_rank(c.sample_format()).is_some()).collect()
    };
    let max_channels = ranges.iter().map(cpal::SupportedStreamConfigRange::channels).max().unwrap_or(0);
    let sample_rates =
        STANDARD_RATES.iter().copied().filter(|r| ranges.iter().any(|c| c.min_sample_rate().0 <= *r && c.max_sample_rate().0 >= *r)).collect();
    let (mut buffer_min, mut buffer_max) = (None::<u32>, None::<u32>);
    for c in &ranges {
        if let cpal::SupportedBufferSize::Range { min, max } = c.buffer_size() {
            buffer_min = Some(buffer_min.map_or(*min, |m| m.min(*min)));
            buffer_max = Some(buffer_max.map_or(*max, |m| m.max(*max)));
        }
    }
    Some(DeviceInfo { is_default: default_name == Some(name.as_str()), name, max_channels, sample_rates, buffer_min, buffer_max })
}

/// Makes sure cpal's device enumerator outlives the thread that first asks for a device.
///
/// On Windows cpal keeps one WASAPI device enumerator for the whole process, made on the first
/// thread to list or open a device under that thread's COM initialisation, which ends with the
/// thread. Once that thread has gone, the next device call on any thread can crash the process
/// (an access violation). So the enumerator is made here, on a thread that never ends. Every cpal
/// host in this crate comes from [`host`] or [`audio_systems`], which call this first.
///
/// The thread joins a single-threaded apartment (cpal initialises COM that way) and never pumps
/// messages. That is safe only because nothing is marshalled to it: cpal marks the enumerator
/// `Send + Sync` and calls it directly from whichever thread asks, which holds because the
/// MMDevice enumerator is a free-threaded object. All this thread has to do is keep the
/// apartment it was made in alive. Should cpal start marshalling calls to the creating thread,
/// they would block here.
///
/// The caller waits at most [`ENUMERATOR_WAIT`] for the enumerator (a driver can hang in
/// enumeration); a panic while making it is caught and logged, and the thread parks either way.
#[cfg(windows)]
fn keep_device_enumerator() {
    static KEPT: std::sync::Once = std::sync::Once::new();
    KEPT.call_once(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("audio devices".into()).spawn(move || {
            // Asking for the default output makes cpal's enumerator, on this thread, with the
            // least work (no device is opened or probed).
            let made = std::panic::catch_unwind(|| {
                let _ = cpal::default_host().default_output_device();
            });
            if made.is_err() {
                log::error!("making the audio device enumerator panicked; audio devices may be unavailable");
            }
            // The caller waits for this; should it be gone (timed out), nothing is lost.
            let _ = tx.send(());
            loop {
                std::thread::park();
            }
        });
        match spawned {
            // Wait until the enumerator is made there, so no caller makes it first.
            Ok(_) => {
                if rx.recv_timeout(ENUMERATOR_WAIT).is_err() {
                    log::warn!(
                        "the audio device enumerator took over {} s to make (or its thread ended); carrying on without waiting",
                        ENUMERATOR_WAIT.as_secs()
                    );
                }
            }
            Err(e) => log::warn!("no thread to keep the audio devices on: {e}"),
        }
    });
}

/// How long the first device call waits for the enumerator's thread.
#[cfg(windows)]
const ENUMERATOR_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(not(windows))]
fn keep_device_enumerator() {}

/// Every audio system on this machine with its devices. Enumerating can take a moment (some
/// systems probe each device): call it when the settings window opens, not every frame.
pub fn audio_systems() -> Vec<AudioSystem> {
    keep_device_enumerator();
    let default_id = cpal::default_host().id();
    cpal::available_hosts()
        .into_iter()
        .filter_map(|id| {
            let host = cpal::host_from_id(id).ok()?;
            let def_out = host.default_output_device().as_ref().and_then(device_name);
            let def_in = host.default_input_device().as_ref().and_then(device_name);
            let outputs = host.output_devices().map(|ds| ds.filter_map(|d| describe(&d, def_out.as_deref(), true)).collect()).unwrap_or_default();
            let inputs = host.input_devices().map(|ds| ds.filter_map(|d| describe(&d, def_in.as_deref(), false)).collect()).unwrap_or_default();
            Some(AudioSystem { name: id.name().to_string(), is_default: id == default_id, outputs, inputs })
        })
        .collect()
}

/// The chosen audio system, else the default one.
pub(crate) fn host(cfg: &AudioConfig) -> cpal::Host {
    keep_device_enumerator();
    cfg.system
        .as_deref()
        .and_then(|want| cpal::available_hosts().into_iter().find(|id| id.name().eq_ignore_ascii_case(want)))
        .and_then(|id| cpal::host_from_id(id).ok())
        .unwrap_or_else(|| {
            if let Some(s) = &cfg.system {
                log::warn!("audio system `{s}` is not available; using the default");
            }
            cpal::default_host()
        })
}

/// The named output (or input) device of `host`, else its default device.
pub(crate) fn device(host: &cpal::Host, name: Option<&str>, output: bool) -> Option<cpal::Device> {
    if let Some(want) = name {
        let found = if output { host.output_devices().ok() } else { host.input_devices().ok() }
            .and_then(|mut ds| ds.find(|d| device_name(d).as_deref() == Some(want)));
        if found.is_some() {
            return found;
        }
        log::warn!("audio device `{want}` is not available; using the default");
    }
    if output { host.default_output_device() } else { host.default_input_device() }
}

/// The buffer to request: the chosen size when the device's range allows it, else the default.
pub(crate) fn buffer_size(want: Option<u32>, supported: &cpal::SupportedBufferSize) -> cpal::BufferSize {
    match (want, supported) {
        (Some(n), cpal::SupportedBufferSize::Range { min, max }) if (*min..=*max).contains(&n) => cpal::BufferSize::Fixed(n),
        // The device does not say: ask anyway (cpal reports an error if it cannot, and the caller
        // falls back to the default).
        (Some(n), cpal::SupportedBufferSize::Unknown) if (16..=8192).contains(&n) => cpal::BufferSize::Fixed(n),
        _ => cpal::BufferSize::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_tolerates_missing_fields() {
        let c = AudioConfig {
            system: Some("WASAPI".into()),
            output: Some("Speakers".into()),
            input: None,
            sample_rate: Some(48_000),
            buffer_size: Some(256),
            headphones: Default::default(),
        };
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<AudioConfig>(&s).unwrap(), c);
        assert_eq!(serde_json::from_str::<AudioConfig>("{}").unwrap(), AudioConfig::default());
        assert_eq!(serde_json::from_str::<AudioConfig>(r#"{"output":"X","unknown":1}"#).unwrap().output.as_deref(), Some("X"));
    }

    #[test]
    fn headphone_routes_are_kept_per_device_and_default_to_3_4_on_a_dj_controller() {
        let mut c = AudioConfig::default();
        assert_eq!(c.headphones_for("Speakers (DDJ-FLX4)", 4), HeadphoneRoute::Outputs(3), "WASAPI's name for it");
        assert_eq!(c.headphones_for("ddj-flx4", 4), HeadphoneRoute::Outputs(3), "any case");
        assert_eq!(c.headphones_for("DDJ-FLX4", 2), HeadphoneRoute::None, "set to stereo: no outputs 3-4");
        assert_eq!(c.headphones_for("Scarlett 4i4", 4), HeadphoneRoute::None, "not a known DJ controller");
        c.headphones.insert("Speakers (DDJ-FLX4)".into(), HeadphoneRoute::None);
        c.headphones.insert("Realtek".into(), HeadphoneRoute::Split);
        c.headphones.insert("MOTU".into(), HeadphoneRoute::Outputs(5));
        assert_eq!(c.headphones_for("Speakers (DDJ-FLX4)", 4), HeadphoneRoute::None, "the choice kept wins");
        assert_eq!(c.headphones_for("MOTU", 8), HeadphoneRoute::Outputs(5));
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains(r#""MOTU":{"outputs":5}"#) && s.contains(r#""Realtek":"split""#), "{s}");
        assert_eq!(serde_json::from_str::<AudioConfig>(&s).unwrap(), c, "round-trips");
        assert!(!serde_json::to_string(&AudioConfig::default()).unwrap().contains("headphones"), "nothing written until chosen");
        // A malformed entry is dropped, the rest kept.
        let odd: AudioConfig =
            serde_json::from_str(r#"{"output":"X","headphones":{"A":{"outputs":3},"B":"sideways","C":{"outputs":-1},"D":7}}"#).unwrap();
        assert_eq!(odd.output.as_deref(), Some("X"));
        assert_eq!(odd.headphones.into_iter().collect::<Vec<_>>(), [("A".to_string(), HeadphoneRoute::Outputs(3))]);
        let not_a_map: AudioConfig = serde_json::from_str(r#"{"output":"X","headphones":[1,2]}"#).unwrap();
        assert!(not_a_map.headphones.is_empty() && not_a_map.output.as_deref() == Some("X"));
        // What a device can take.
        assert_eq!(HeadphoneRoute::Outputs(3).fit(4), HeadphoneRoute::Outputs(3));
        assert_eq!(HeadphoneRoute::Outputs(3).fit(2), HeadphoneRoute::None);
        assert_eq!(HeadphoneRoute::Outputs(1).fit(4), HeadphoneRoute::None, "never over the master");
        assert_eq!(HeadphoneRoute::Split.fit(2), HeadphoneRoute::Split);
        assert_eq!(HeadphoneRoute::Split.fit(1), HeadphoneRoute::None);
        assert_eq!((HeadphoneRoute::Outputs(3).channels(), HeadphoneRoute::Split.channels()), (4, 0));
        assert_eq!(HeadphoneRoute::choices(2), [HeadphoneRoute::None, HeadphoneRoute::Split]);
        assert_eq!(HeadphoneRoute::choices(6), [HeadphoneRoute::None, HeadphoneRoute::Split, HeadphoneRoute::Outputs(3), HeadphoneRoute::Outputs(5)]);
        assert_eq!(HeadphoneRoute::choices(5).len(), 3, "an odd output out has no pair");
        assert_eq!(HeadphoneRoute::Outputs(3).label(), "outputs 3-4");
    }

    #[test]
    fn buffer_sizes_follow_the_device_range() {
        let r = cpal::SupportedBufferSize::Range { min: 64, max: 1024 };
        assert_eq!(buffer_size(Some(256), &r), cpal::BufferSize::Fixed(256));
        assert_eq!(buffer_size(Some(32), &r), cpal::BufferSize::Default);
        assert_eq!(buffer_size(Some(4096), &r), cpal::BufferSize::Default);
        assert_eq!(buffer_size(None, &r), cpal::BufferSize::Default);
        assert_eq!(buffer_size(Some(128), &cpal::SupportedBufferSize::Unknown), cpal::BufferSize::Fixed(128));
        assert_eq!(buffer_size(Some(1 << 20), &cpal::SupportedBufferSize::Unknown), cpal::BufferSize::Default);
        let d = DeviceInfo {
            name: "x".into(),
            is_default: true,
            max_channels: 2,
            sample_rates: vec![48_000],
            buffer_min: Some(64),
            buffer_max: Some(1024),
        };
        assert_eq!(d.buffer_sizes(), [64, 128, 256, 512, 1024]);
        let open = DeviceInfo { buffer_min: None, buffer_max: None, ..d };
        assert_eq!(open.buffer_sizes().len(), STANDARD_BUFFERS.len());
    }

    #[test]
    fn unknown_systems_and_devices_fall_back_to_the_defaults() {
        let cfg = AudioConfig { system: Some("No Such System".into()), ..Default::default() };
        assert_eq!(host(&cfg).id(), cpal::default_host().id());
        // No device with this name exists: the default (or none, on a machine without audio).
        let h = cpal::default_host();
        let fallback = device(&h, Some("No Such Device 1234"), true).as_ref().and_then(device_name);
        assert_eq!(fallback, h.default_output_device().as_ref().and_then(device_name));
    }

    #[test]
    fn enumeration_does_not_fail() {
        let systems = audio_systems();
        // Every listed system has a name; on a machine without audio the lists are just empty.
        assert!(systems.iter().all(|s| !s.name.is_empty()));
        assert!(systems.iter().filter(|s| s.is_default).count() <= 1);
    }
}
