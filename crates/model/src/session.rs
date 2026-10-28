//! The session: the whole document.

use crate::clip::{bound, bound_sequence};
use crate::{
    Bus, BusId, ChannelFormat, Clip, ClipId, DjState, Group, GroupId, LiveState, MarkerId, MarkerKind, MemoryLocation, OutputPath, Source, SourceId,
    SourcePool, TRACK_COLORS, Track, TrackId, TrackKind, VideoSource,
};
use soundcraft_time::{FrameRate, GridValue, Range, SampleRate, Samples, TempoMap, TimeFormat};

/// Native session file extension.
pub const SESSION_EXTENSION: &str = "scraft";

/// `EditState::values` key holding the feet+frames reading of sample 0, in frames.
pub const FEET_FRAMES_START: &str = "feet_frames.start";

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SessionError {
    #[error("not a SoundCraft session: {0}")]
    Format(String),
    #[error("session file version {0} is newer than this SoundCraft supports")]
    Version(u32),
    #[error("{0}")]
    Invalid(String),
}

/// Session bit depth for recording/rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub enum BitDepthSetting {
    Int16,
    #[default]
    Int24,
    Float32,
}

/// Edit modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub enum EditMode {
    Shuffle,
    #[default]
    Slip,
    Spot,
    Grid,
    /// Relative grid: moves keep their offset from the grid.
    GridRelative,
}

impl EditMode {
    pub fn label(self) -> &'static str {
        match self {
            EditMode::Shuffle => "SHUFFLE",
            EditMode::Slip => "SLIP",
            EditMode::Spot => "SPOT",
            EditMode::Grid => "GRID",
            EditMode::GridRelative => "REL GRID",
        }
    }
    pub fn from_id(s: &str) -> Option<EditMode> {
        match s.to_ascii_lowercase().as_str() {
            "shuffle" => Some(EditMode::Shuffle),
            "slip" => Some(EditMode::Slip),
            "spot" => Some(EditMode::Spot),
            "grid" | "absolute_grid" => Some(EditMode::Grid),
            "relative_grid" | "rel_grid" | "grid_relative" => Some(EditMode::GridRelative),
            _ => None,
        }
    }
    pub fn is_grid(self) -> bool {
        matches!(self, EditMode::Grid | EditMode::GridRelative)
    }
}

/// Edit tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub enum Tool {
    Zoom,
    Trim,
    #[default]
    Selector,
    Grabber,
    Scrubber,
    Pencil,
    Smart,
}

impl Tool {
    pub const ALL: [Tool; 7] = [Tool::Zoom, Tool::Trim, Tool::Selector, Tool::Grabber, Tool::Scrubber, Tool::Pencil, Tool::Smart];
    pub fn id(self) -> &'static str {
        match self {
            Tool::Zoom => "zoom",
            Tool::Trim => "trim",
            Tool::Selector => "selector",
            Tool::Grabber => "grabber",
            Tool::Scrubber => "scrubber",
            Tool::Pencil => "pencil",
            Tool::Smart => "smart",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Tool::Zoom => "Zoomer",
            Tool::Trim => "Trim",
            Tool::Selector => "Selector",
            Tool::Grabber => "Grabber",
            Tool::Scrubber => "Scrubber",
            Tool::Pencil => "Pencil",
            Tool::Smart => "Smart Tool",
        }
    }
    pub fn from_id(s: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.id() == s.to_ascii_lowercase())
    }
    /// F-key shortcut (F5..F10) like the incumbent.
    pub fn fkey(self) -> Option<&'static str> {
        match self {
            Tool::Zoom => Some("F5"),
            Tool::Trim => Some("F6"),
            Tool::Selector => Some("F7"),
            Tool::Grabber => Some("F8"),
            Tool::Scrubber => Some("F9"),
            Tool::Pencil => Some("F10"),
            Tool::Smart => None,
        }
    }
}

/// Track view lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TrackView {
    Waveform,
    Blocks,
    Volume,
    Pan,
    Mute,
}

/// Horizontal zoom + scroll.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ZoomState {
    /// Samples per pixel (point).
    pub samples_per_px: f64,
    /// Left edge of the visible timeline.
    pub scroll: Samples,
    /// Waveform vertical zoom factor.
    pub waveform_zoom: f32,
    /// Zoom preset memories 1..5.
    pub presets: [f64; 5],
}

impl Default for ZoomState {
    fn default() -> Self {
        ZoomState { samples_per_px: 1024.0, scroll: 0, waveform_zoom: 1.0, presets: [64.0, 256.0, 1024.0, 4096.0, 16384.0] }
    }
}

/// The view Zoom Toggle goes back to.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ZoomToggle {
    pub samples_per_px: f64,
    pub scroll: Samples,
    pub heights: Vec<(TrackId, crate::TrackHeight)>,
}

/// Edit-window state saved with the session (selection, tools, modes, view settings).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EditState {
    pub tool: Tool,
    pub edit_mode: EditMode,
    /// The edit selection (a point = insertion position).
    pub selection: Range,
    /// Tracks in the edit selection.
    pub selected_tracks: Vec<TrackId>,
    /// Clips explicitly selected (Grabber selection).
    #[serde(default)]
    pub selected_clips: Vec<ClipId>,
    /// Audio files selected in the Clip List.
    #[serde(default)]
    pub selected_sources: Vec<SourceId>,
    /// Playhead position when stopped.
    pub playhead: Samples,
    pub grid: GridValue,
    pub nudge: GridValue,
    #[serde(default = "yes")]
    pub grid_lines: bool,
    pub main_counter: TimeFormat,
    pub sub_counter: Option<TimeFormat>,
    pub zoom: ZoomState,
    /// Set while Zoom Toggle is on.
    pub zoom_toggle: Option<ZoomToggle>,
    /// Visible rulers.
    pub rulers: Vec<String>,
    pub link_timeline_edit: bool,
    pub link_track_edit: bool,
    pub insertion_follows_playback: bool,
    pub loop_playback: bool,
    pub pre_post_roll: bool,
    pub pre_roll: Samples,
    pub post_roll: Samples,
    pub tab_to_transient: bool,
    pub automation_follows_edit: bool,
    pub markers_follow_edit: bool,
    pub layered_editing: bool,
    pub mirrored_midi: bool,
    pub click: bool,
    pub countoff: bool,
    pub countoff_bars: u32,
    pub midi_merge: bool,
    pub record_mode: String,
    pub loop_record: bool,
    pub quickpunch: bool,
    pub scrolling: String,
    pub solo_mode: String,
    pub pre_fader_metering: bool,
    pub delay_compensation: bool,
    pub keyboard_focus: String,
    /// Timeline selection when it is unlinked from the edit selection.
    pub timeline_selection: Range,
    /// Generic on/off view and option flags keyed by id (e.g. `view.clip.name`, `waveform.rectified`,
    /// `options.midi_thru`). Saved with the session so every view setting persists.
    #[serde(default)]
    pub flags: std::collections::BTreeSet<String>,
    /// Numeric settings keyed by id (e.g. `click.volume_db`, `engine.buffer_size`).
    #[serde(default)]
    pub values: std::collections::BTreeMap<String, f64>,
}

impl EditState {
    pub fn flag(&self, id: &str) -> bool {
        self.flags.contains(id)
    }
    pub fn set_flag(&mut self, id: &str, on: bool) {
        if on {
            self.flags.insert(id.to_string());
        } else {
            self.flags.remove(id);
        }
    }
    pub fn value(&self, id: &str, default: f64) -> f64 {
        self.values.get(id).copied().filter(|v| v.is_finite()).unwrap_or(default)
    }
}

fn yes() -> bool {
    true
}

impl Default for EditState {
    fn default() -> Self {
        EditState {
            tool: Tool::Smart,
            edit_mode: EditMode::Slip,
            selection: Range::default(),
            selected_tracks: Vec::new(),
            selected_clips: Vec::new(),
            selected_sources: Vec::new(),
            playhead: 0,
            grid: GridValue::default(),
            nudge: GridValue::Seconds(1.0),
            grid_lines: true,
            main_counter: TimeFormat::MinSecs,
            sub_counter: Some(TimeFormat::Samples),
            zoom: ZoomState::default(),
            zoom_toggle: None,
            rulers: vec!["bars_beats".into(), "min_secs".into(), "tempo".into(), "meter".into(), "markers".into()],
            link_timeline_edit: true,
            link_track_edit: false,
            insertion_follows_playback: false,
            loop_playback: false,
            pre_post_roll: false,
            pre_roll: 96_000,
            post_roll: 96_000,
            tab_to_transient: false,
            automation_follows_edit: true,
            markers_follow_edit: false,
            layered_editing: false,
            mirrored_midi: false,
            click: false,
            countoff: false,
            countoff_bars: 2,
            midi_merge: false,
            record_mode: "normal".into(),
            loop_record: false,
            quickpunch: false,
            scrolling: "page".into(),
            solo_mode: "sip".into(),
            pre_fader_metering: false,
            delay_compensation: true,
            keyboard_focus: "commands".into(),
            timeline_selection: Range::default(),
            flags: ["view.clip.name", "view.clip.gain_line", "waveform.peak", "view.clip.overlap_shadows", "view.marker.ruler_lines"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            values: std::collections::BTreeMap::new(),
        }
    }
}

/// The document.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub format_version: u32,
    pub name: String,
    pub sample_rate: SampleRate,
    pub bit_depth: BitDepthSetting,
    pub frame_rate: FrameRate,
    /// Timecode of sample 0 (session start), as a sample count.
    pub timecode_start: Samples,
    pub tempo: TempoMap,
    #[serde(default)]
    pub key_signatures: Vec<(Samples, String)>,
    pub tracks: Vec<Track>,
    pub sources: Vec<Source>,
    /// Movies referenced by Video track clips.
    #[serde(default)]
    pub videos: Vec<VideoSource>,
    pub busses: Vec<Bus>,
    pub outputs: Vec<OutputPath>,
    pub markers: Vec<MemoryLocation>,
    pub groups: Vec<Group>,
    pub edit: EditState,
    pub next_id: u64,
    #[serde(default)]
    pub comments: String,
    /// DJ arrangement data: transitions between beat-locked tracks of an imported set.
    #[serde(default, skip_serializing_if = "DjState::is_empty")]
    pub dj: DjState,
    /// The Session view's Live columns and their clip slots.
    #[serde(default, skip_serializing_if = "LiveState::is_empty")]
    pub live: LiveState,
    /// Original MIDI performances keyed by clip id (Event › MIDI Operations › Restore/Flatten
    /// Performance). Independent of undo so a restore survives later edits.
    #[serde(default)]
    pub midi_originals: std::collections::BTreeMap<u64, soundcraft_midi::Sequence>,
    #[serde(skip)]
    pub pool: SourcePool,
    /// The folder relative source paths point into: where the session was opened or last saved.
    #[serde(skip)]
    pub folder: Option<std::path::PathBuf>,
}

impl Default for Session {
    fn default() -> Self {
        Session::new("Untitled", SampleRate::HZ_48000)
    }
}

impl Session {
    pub const FORMAT_VERSION: u32 = 1;

    pub fn new(name: impl Into<String>, sample_rate: SampleRate) -> Self {
        let mut s = Session {
            format_version: Self::FORMAT_VERSION,
            name: name.into(),
            sample_rate,
            bit_depth: BitDepthSetting::Int24,
            frame_rate: FrameRate::Fps30,
            timecode_start: 0,
            tempo: TempoMap::default(),
            key_signatures: Vec::new(),
            tracks: Vec::new(),
            sources: Vec::new(),
            videos: Vec::new(),
            busses: Vec::new(),
            outputs: vec![OutputPath { name: "Out 1-2".into(), first_channel: 0, format: ChannelFormat::Stereo }],
            markers: Vec::new(),
            groups: Vec::new(),
            edit: EditState::default(),
            next_id: 1,
            comments: String::new(),
            dj: DjState::default(),
            live: LiveState::default(),
            midi_originals: std::collections::BTreeMap::new(),
            pool: SourcePool::default(),
            folder: None,
        };
        s.edit.pre_roll = sample_rate.samples(2.0);
        s.edit.post_roll = sample_rate.samples(2.0);
        s
    }

    /// Allocate a fresh id.
    pub fn alloc(&mut self) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id.saturating_add(1);
        id
    }

    // ---- lookups -------------------------------------------------------------------------

    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }
    pub fn track_mut(&mut self, id: TrackId) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }
    pub fn track_index(&self, id: TrackId) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == id)
    }
    pub fn track_by_name(&self, name: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.name == name).or_else(|| self.tracks.iter().find(|t| t.name.eq_ignore_ascii_case(name)))
    }
    pub fn master(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Master)
    }
    pub fn source(&self, id: SourceId) -> Option<&Source> {
        self.sources.iter().find(|s| s.id == id)
    }
    pub fn video(&self, id: SourceId) -> Option<&VideoSource> {
        self.videos.iter().find(|v| v.id == id)
    }
    pub fn bus(&self, id: BusId) -> Option<&Bus> {
        self.busses.iter().find(|b| b.id == id)
    }
    pub fn bus_by_name(&self, name: &str) -> Option<&Bus> {
        self.busses.iter().find(|b| b.name.eq_ignore_ascii_case(name))
    }
    pub fn group(&self, id: GroupId) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }
    pub fn marker(&self, id: MarkerId) -> Option<&MemoryLocation> {
        self.markers.iter().find(|m| m.id == id)
    }

    /// Find a clip anywhere (active playlists).
    pub fn find_clip(&self, id: ClipId) -> Option<(TrackId, &Clip)> {
        self.tracks.iter().find_map(|t| t.clips().iter().find(|c| c.id == id).map(|c| (t.id, c)))
    }

    pub fn find_clip_mut(&mut self, id: ClipId) -> Option<&mut Clip> {
        self.tracks.iter_mut().find_map(|t| t.playlist_mut().and_then(|p| p.clip_mut(id)))
    }

    /// Clips on every playlist, alternates included.
    pub fn all_clips(&self) -> impl Iterator<Item = &Clip> {
        self.tracks.iter().flat_map(|t| t.playlists.iter().flat_map(|p| &p.clips))
    }

    /// Clips (any playlist) that play `src`, as their source or as one of their stems.
    pub fn clips_using(&self, src: SourceId) -> impl Iterator<Item = &Clip> {
        self.all_clips().filter(move |c| c.audio_sources().any(|x| x == src))
    }

    /// Session view slots that play `src`, as their source or as one of their stems, with their
    /// column and row.
    pub fn slots_using(&self, src: SourceId) -> impl Iterator<Item = (usize, usize, &crate::Slot)> {
        self.live.slots().filter(move |(_, _, s)| s.audio_sources().any(|x| x == src))
    }

    /// Every audio source the timeline, the Session view's slots and the Samplers play (with
    /// repeats).
    pub fn used_sources(&self) -> impl Iterator<Item = SourceId> + '_ {
        self.all_clips()
            .flat_map(Clip::audio_sources)
            .chain(self.live.slots().flat_map(|(_, _, s)| s.audio_sources()))
            .chain(self.tracks.iter().filter_map(|t| t.instrument.as_ref().and_then(|i| i.sample)))
    }

    /// End of the last clip on any track (session length).
    pub fn content_end(&self) -> Samples {
        self.tracks.iter().filter_map(|t| t.playlist().map(|p| p.end())).max().unwrap_or(0)
    }

    /// Feet+frames reading of sample 0, in frames (Setup › Current Feet+Frames Position).
    pub fn feet_frames_start(&self) -> i64 {
        self.edit.value(FEET_FRAMES_START, 0.0) as i64
    }

    /// Format a position against this session's timecode and feet+frames references.
    pub fn format_position(&self, at: Samples, fmt: TimeFormat) -> String {
        soundcraft_time::format_position(at, fmt, self.sample_rate, &self.tempo, self.frame_rate, self.timecode_start, self.feet_frames_start())
    }

    /// Parse a position typed in `fmt` against this session's references.
    pub fn parse_position(&self, text: &str, fmt: TimeFormat) -> Result<Samples, soundcraft_time::TimeError> {
        soundcraft_time::parse_position(text, fmt, self.sample_rate, &self.tempo, self.frame_rate, self.timecode_start, self.feet_frames_start())
    }

    pub fn next_track_color(&self) -> [u8; 3] {
        let n = self.tracks.iter().filter(|t| t.kind != TrackKind::Master).count();
        TRACK_COLORS.get(n % TRACK_COLORS.len()).copied().unwrap_or([128, 128, 128])
    }

    /// A unique track name: `base`, or `base 1`, `base 2` … like the incumbent ("Audio 1").
    pub fn unique_track_name(&self, base: &str) -> String {
        let taken = |n: &str| self.tracks.iter().any(|t| t.name == n);
        if !base.is_empty() && !taken(base) && !matches!(base, "Audio" | "Aux" | "MIDI" | "Inst" | "VCA" | "Folder" | "Master") {
            return base.to_string();
        }
        let prefix = format!("{base} ");
        let used: std::collections::HashSet<u64> = self
            .tracks
            .iter()
            .filter_map(|t| t.name.strip_prefix(&prefix))
            .filter_map(|n| n.parse().ok().filter(|i: &u64| i.to_string() == n))
            .collect();
        let i = (1..).find(|i| !used.contains(i)).unwrap_or(1);
        format!("{base} {i}")
    }

    /// Add a track with defaults; returns its id.
    pub fn add_track(&mut self, kind: TrackKind, format: ChannelFormat, name: Option<&str>) -> TrackId {
        let id = TrackId(self.alloc());
        let name = self.unique_track_name(name.unwrap_or(kind.default_name()));
        let color = if kind == TrackKind::Master { [140, 140, 140] } else { self.next_track_color() };
        let mut t = Track::new(id, name, kind, format, color);
        if kind == TrackKind::Master {
            t.mixer.output = crate::Route::Main;
            t.mixer.input = crate::Route::Main;
        }
        // Master faders sit at the bottom like the incumbent; others go before any master.
        let pos = if kind == TrackKind::Master {
            self.tracks.len()
        } else {
            self.tracks.iter().position(|t| t.kind == TrackKind::Master).unwrap_or(self.tracks.len())
        };
        self.tracks.insert(pos, t);
        id
    }

    pub fn add_bus(&mut self, name: &str, format: ChannelFormat) -> BusId {
        if let Some(b) = self.bus_by_name(name) {
            return b.id;
        }
        let id = BusId(self.alloc());
        self.busses.push(Bus { id, name: name.to_string(), format });
        id
    }

    /// Add a memory location; returns its id.
    pub fn add_marker(&mut self, name: &str, kind: MarkerKind, start: Samples, end: Samples) -> MarkerId {
        let id = MarkerId(self.alloc());
        let number = self.markers.iter().map(|m| m.number).max().unwrap_or(0).saturating_add(1);
        self.markers.push(MemoryLocation {
            id,
            number,
            name: name.to_string(),
            kind,
            start,
            end: end.max(start),
            comments: String::new(),
            color: None,
            ruler: 1,
            track: None,
        });
        self.markers.sort_by_key(|m| m.start);
        id
    }

    pub fn new_clip_id(&mut self) -> ClipId {
        ClipId(self.alloc())
    }

    // ---- persistence ---------------------------------------------------------------------

    pub fn to_json(&self) -> Result<String, SessionError> {
        serde_json::to_string_pretty(self).map_err(|e| SessionError::Format(e.to_string()))
    }

    /// Parse a session; decoded audio must be attached separately (`pool`).
    pub fn from_json(text: &str) -> Result<Session, SessionError> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| SessionError::Format(e.to_string()))?;
        let ver = v.get("format_version").and_then(serde_json::Value::as_u64).ok_or_else(|| SessionError::Format("missing format_version".into()))?;
        if ver > u64::from(Self::FORMAT_VERSION) {
            return Err(SessionError::Version(u32::try_from(ver).unwrap_or(u32::MAX)));
        }
        let mut s: Session = serde_json::from_value(v).map_err(|e| SessionError::Format(e.to_string()))?;
        s.sanitize();
        Ok(s)
    }

    /// Repair invariants after loading untrusted data.
    pub fn sanitize(&mut self) {
        let max_id = self
            .tracks
            .iter()
            .map(|t| t.id.0)
            .chain(self.tracks.iter().flat_map(|t| t.playlists.iter().flat_map(|p| p.clips.iter().map(|c| c.id.0))))
            .chain(self.sources.iter().map(|s| s.id.0))
            .chain(self.videos.iter().map(|v| v.id.0))
            .chain(self.busses.iter().map(|b| b.id.0))
            .chain(self.markers.iter().map(|m| m.id.0))
            .chain(self.groups.iter().map(|g| g.id.0))
            // A lost Live column's track id is never handed out again.
            .chain(self.live.columns.iter().map(|c| c.track.0))
            .max()
            .unwrap_or(0);
        self.next_id = self.next_id.max(max_id.saturating_add(1));
        truncate_chars(&mut self.name, MAX_NAME_CHARS);
        truncate_chars(&mut self.comments, MAX_COMMENT_CHARS);
        for t in &mut self.tracks {
            t.mixer.sanitize();
            truncate_chars(&mut t.name, MAX_NAME_CHARS);
            truncate_chars(&mut t.comments, MAX_COMMENT_CHARS);
            if t.playlists.is_empty() {
                t.playlists.push(crate::Playlist::new(t.name.clone()));
            }
            if t.active_playlist >= t.playlists.len() {
                t.active_playlist = 0;
            }
            for p in &mut t.playlists {
                truncate_chars(&mut p.name, MAX_NAME_CHARS);
                p.clips.retain(|c| c.length > 0);
                p.clips.iter_mut().for_each(Clip::sanitize);
                p.sort();
            }
            // A stem lane past the last stem controls nothing.
            t.automation.retain(|l| !matches!(l.param, crate::AutoParam::Stem(i) if usize::from(i) >= crate::STEMS.len()));
            for l in &mut t.automation {
                l.points.retain(|p| p.value.is_finite());
                // Stem faders run -144..+6 dB, however the file wrote them.
                if matches!(l.param, crate::AutoParam::Stem(_)) {
                    let (lo, hi, _) = l.param.range();
                    l.points.iter_mut().for_each(|p| p.value = p.value.clamp(lo, hi));
                }
                l.points.iter_mut().for_each(|p| bound(&mut p.at));
                l.points.sort_by_key(|p| p.at);
            }
        }
        for m in &mut self.markers {
            bound(&mut m.start);
            bound(&mut m.end);
            truncate_chars(&mut m.name, MAX_NAME_CHARS);
            truncate_chars(&mut m.comments, MAX_COMMENT_CHARS);
        }
        for (at, key) in &mut self.key_signatures {
            bound(at);
            truncate_chars(key, MAX_NAME_CHARS);
        }
        self.midi_originals.values_mut().for_each(bound_sequence);
        let e = &mut self.edit;
        for at in [
            &mut e.selection.start,
            &mut e.selection.end,
            &mut e.timeline_selection.start,
            &mut e.timeline_selection.end,
            &mut e.playhead,
            &mut e.pre_roll,
            &mut e.post_roll,
            &mut e.zoom.scroll,
            &mut self.timecode_start,
        ] {
            bound(at);
        }
        for v in &mut self.videos {
            if !v.frame_rate.is_finite() || v.frame_rate < 0.0 {
                v.frame_rate = 0.0;
            }
            if !v.duration.is_finite() || v.duration < 0.0 {
                v.duration = 0.0;
            }
            truncate_chars(&mut v.name, MAX_NAME_CHARS);
        }
        self.busses.iter_mut().for_each(|b| truncate_chars(&mut b.name, MAX_NAME_CHARS));
        self.outputs.iter_mut().for_each(|o| truncate_chars(&mut o.name, MAX_NAME_CHARS));
        self.groups.iter_mut().for_each(|g| truncate_chars(&mut g.name, MAX_NAME_CHARS));
        self.sources.iter_mut().for_each(|s| truncate_chars(&mut s.name, MAX_NAME_CHARS));
        // Drop performances whose clip no longer exists anywhere (any playlist).
        let clip_ids: std::collections::BTreeSet<u64> =
            self.tracks.iter().flat_map(|t| t.playlists.iter().flat_map(|p| p.clips.iter().map(|c| c.id.0))).collect();
        self.midi_originals.retain(|id, _| clip_ids.contains(id));
        self.dj.sanitize(&self.tracks);
        self.live.sanitize(&self.sources);
        if !self.edit.zoom.samples_per_px.is_finite() || self.edit.zoom.samples_per_px <= 0.0 {
            self.edit.zoom.samples_per_px = 1024.0;
        }
        if SampleRate::new(self.sample_rate.hz()).is_err() {
            self.sample_rate = SampleRate::default();
        }
        self.tempo.sanitize();
    }
}

/// The most characters kept of a name read from a file: longer ones can't be shown, and drawing
/// a name of a million characters overflows the renderer's buffers.
pub const MAX_NAME_CHARS: usize = 256;
/// The most characters kept of a comment read from a file.
pub const MAX_COMMENT_CHARS: usize = 4096;

/// Cut `s` to at most `max` characters.
pub fn truncate_chars(s: &mut String, max: usize) {
    if let Some((end, _)) = s.char_indices().nth(max) {
        s.truncate(end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_tracks_names_and_order() {
        let mut s = Session::default();
        let m = s.add_track(TrackKind::Master, ChannelFormat::Stereo, None);
        let a = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let b = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let v = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Vocal"));
        assert_eq!(s.track(a).map(|t| t.name.as_str()), Some("Audio 1"));
        assert_eq!(s.track(b).map(|t| t.name.as_str()), Some("Audio 2"));
        assert_eq!(s.track(v).map(|t| t.name.as_str()), Some("Vocal"));
        assert_eq!(s.tracks.last().map(|t| t.id), Some(m));
        let v2 = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Vocal"));
        assert_eq!(s.track(v2).map(|t| t.name.as_str()), Some("Vocal 1"));
    }

    #[test]
    fn json_round_trip() {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let cid = s.new_clip_id();
        if let Some(p) = s.track_mut(t).and_then(Track::playlist_mut) {
            p.clips.push(Clip::audio(cid, "c", SourceId(99), 0, 10, 100));
        }
        s.add_marker("Verse", MarkerKind::Marker, 48_000, 48_000);
        let text = s.to_json().unwrap();
        let back = Session::from_json(&text).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn video_clip_round_trips_and_old_sessions_load() {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Video, ChannelFormat::Mono, None);
        let vid = SourceId(s.alloc());
        s.videos.push(VideoSource {
            id: vid,
            name: "cut.mov".into(),
            path: "/films/cut.mov".into(),
            width: 1920,
            height: 1080,
            frame_rate: 23.976,
            duration: 12.5,
            codec: "prores".into(),
            offline: None,
        });
        let cid = s.new_clip_id();
        if let Some(p) = s.track_mut(t).and_then(Track::playlist_mut) {
            p.clips.push(Clip::video(cid, "cut", vid, 480, 0, 600_000));
        }
        let back = Session::from_json(&s.to_json().unwrap()).unwrap();
        assert_eq!(back, s);
        let c = &back.track(t).unwrap().clips()[0];
        assert_eq!((c.video_source(), c.source(), c.source_offset(), c.is_audio()), (Some(vid), None, 480, false));
        assert_eq!(back.track(t).unwrap().kind.label(), "Video Track");
        assert!(back.video(vid).is_some());
        // A session written before video support has no `videos` key.
        let mut v = serde_json::to_value(Session::default()).unwrap();
        v.as_object_mut().unwrap().remove("videos");
        assert!(Session::from_json(&v.to_string()).unwrap().videos.is_empty());
        // Trimming the start keeps the picture anchored.
        let mut c = c.clone();
        c.trim_start_to(100);
        assert_eq!(c.source_offset(), 580);
    }

    #[test]
    fn unique_track_names_take_the_first_free_number() {
        let mut s = Session::default();
        for name in ["Piano", "Piano 1", "Piano 3", "Piano 02", "Pianos 2"] {
            s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some(name));
        }
        assert_eq!(s.unique_track_name("Piano"), "Piano 2");
        assert_eq!(s.unique_track_name("Organ"), "Organ");
        assert_eq!(s.unique_track_name("Audio"), "Audio 1");
    }

    #[test]
    fn names_from_a_file_are_cut_to_a_length_that_can_be_drawn() {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        if let Some(tr) = s.track_mut(t) {
            tr.name = "é".repeat(1_000);
        }
        s.comments = "c".repeat(10_000);
        let back = Session::from_json(&s.to_json().unwrap()).unwrap();
        assert_eq!(back.track(t).map(|tr| tr.name.chars().count()), Some(MAX_NAME_CHARS));
        assert_eq!(back.comments.len(), MAX_COMMENT_CHARS);
    }

    #[test]
    fn a_bad_sample_rate_or_tempo_is_repaired_on_load() {
        let mut v = serde_json::to_value(Session::default()).unwrap();
        v["sample_rate"] = serde_json::json!(0);
        v["tempo"]["tempos"] = serde_json::json!([{"tick": 0, "bpm": 4e9}]);
        let s = Session::from_json(&v.to_string()).unwrap();
        assert_eq!((s.sample_rate, s.tempo), (SampleRate::HZ_48000, TempoMap::default()));
    }

    #[test]
    fn rejects_garbage_and_future_versions() {
        assert!(Session::from_json("not json").is_err());
        assert!(Session::from_json("{}").is_err());
        let mut v = serde_json::to_value(Session::default()).unwrap();
        v["format_version"] = serde_json::json!(999);
        assert!(matches!(Session::from_json(&v.to_string()), Err(SessionError::Version(999))));
    }

    #[test]
    fn sanitize_repairs() {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        if let Some(tr) = s.track_mut(t) {
            tr.active_playlist = 7;
            tr.mixer.volume_db = f32::NAN;
        }
        s.next_id = 0;
        s.sanitize();
        let tr = s.track(t).unwrap();
        assert_eq!(tr.active_playlist, 0);
        assert_eq!(tr.mixer.volume_db, 0.0);
        assert!(s.next_id > t.0);
    }

    /// A session with an audio track whose clip plays source 7, split into stems 11..14 with a
    /// vocals-off mask, a −6 dB bass fader and a `stem:bass` lane.
    fn split_session() -> (Session, TrackId, ClipId) {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let cid = s.new_clip_id();
        let mut c = Clip::audio(cid, "c", SourceId(7), 0, 10, 100);
        c.stems = Some(crate::StemSet { scale_log2: 0, key: "k".into(), sources: [SourceId(11), SourceId(12), SourceId(13), SourceId(14)] });
        c.stem_mask = [false, true, true, true];
        if let Some(tr) = s.track_mut(t) {
            tr.mixer.stem_gain_db[2] = -6.0;
            let mut lane = crate::AutomationLane::new(crate::AutoParam::Stem(2));
            lane.set_point(0, 0.0);
            lane.set_point(50, -144.0);
            tr.automation.push(lane);
        }
        if let Some(p) = s.track_mut(t).and_then(Track::playlist_mut) {
            p.clips.push(c);
        }
        (s, t, cid)
    }

    #[test]
    fn a_split_session_round_trips_and_one_saved_before_stems_loads_unsplit() {
        let (s, t, cid) = split_session();
        let back = Session::from_json(&s.to_json().unwrap()).unwrap();
        assert_eq!(back, s);
        // The same session as an older build wrote it: no stem fields anywhere.
        let mut v = serde_json::to_value(&s).unwrap();
        for tr in v["tracks"].as_array_mut().unwrap() {
            tr["mixer"].as_object_mut().unwrap().remove("stem_gain_db");
            tr["automation"] = serde_json::json!([]);
            for pl in tr["playlists"].as_array_mut().unwrap() {
                for c in pl["clips"].as_array_mut().unwrap() {
                    c.as_object_mut().unwrap().remove("stems");
                    c.as_object_mut().unwrap().remove("stem_mask");
                }
            }
        }
        let old = Session::from_json(&v.to_string()).unwrap();
        let c = old.find_clip(cid).unwrap().1;
        assert_eq!((&c.stems, c.stem_mask), (&None, [true; 4]));
        assert_eq!(old.track(t).unwrap().mixer.stem_gain_db, [0.0; 4]);
    }

    #[test]
    fn stem_files_count_as_used_by_the_clip() {
        let (s, _, cid) = split_session();
        for src in [7, 11, 12, 13, 14] {
            assert_eq!(s.clips_using(SourceId(src)).map(|c| c.id).collect::<Vec<_>>(), [cid], "source {src}");
        }
        assert_eq!(s.clips_using(SourceId(8)).count(), 0);
    }

    #[test]
    fn a_stem_lane_past_the_last_stem_is_dropped_on_load() {
        let (mut s, t, _) = split_session();
        if let Some(tr) = s.track_mut(t) {
            tr.automation.push(crate::AutomationLane::new(crate::AutoParam::Stem(4)));
            if let Some(lane) = tr.automation.first_mut() {
                lane.points.push(crate::AutomationPoint { at: 60, value: 40.0 });
                lane.points.push(crate::AutomationPoint { at: 70, value: -1e9 });
            }
        }
        let back = Session::from_json(&s.to_json().unwrap()).unwrap();
        let lanes = &back.track(t).unwrap().automation;
        assert_eq!(lanes.iter().map(|l| l.param.clone()).collect::<Vec<_>>(), [crate::AutoParam::Stem(2)]);
        // Points written out of range are clamped to the fader's.
        assert_eq!(lanes[0].points.iter().map(|p| p.value).collect::<Vec<_>>(), [0.0, -144.0, 6.0, -144.0]);
    }
    /// A session with two Live columns: the first holds a beat-locked slot of source 7 split into
    /// stems 11..14 (vocals only) in row 2; the second track's column holds nothing.
    fn live_session() -> (Session, TrackId, TrackId) {
        let (mut s, _, _) = split_session();
        for id in [7, 11, 12, 13, 14] {
            s.sources.push(crate::Source {
                id: SourceId(id),
                name: format!("s{id}"),
                path: format!("/a/{id}.wav"),
                channels: 2,
                frames: 100,
                sample_rate: 48_000,
                format: soundcraft_audio_io::FileFormat::Wav,
                time_reference: 0,
                unsaved: false,
                file_len: 0,
                file_modified_ms: 0,
                origin: None,
                origin_len: 0,
                origin_modified_ms: 0,
            });
        }
        let a = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Live 1"));
        let b = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Live 2"));
        let mut slot = crate::Slot::one_shot("Drums", [10, 20, 30], SourceId(7));
        slot.warp = crate::WarpMap::new(vec![0.0, 24_000.0, 48_000.0], 0.0);
        slot.mode = crate::SlotMode::Loop;
        (slot.start_beat, slot.length_beats, slot.gain_db) = (16.0, 16.0, -3.0);
        slot.stems = Some(crate::StemSet { scale_log2: 2, key: "k".into(), sources: [SourceId(11), SourceId(12), SourceId(13), SourceId(14)] });
        slot.stem_mask = [true, false, false, false];
        let mut col = crate::LiveColumn::new(a);
        col.slots[2] = Some(slot);
        s.live.columns = vec![col, crate::LiveColumn::new(b)];
        s.live.quantise = crate::Quantise::Bar(2);
        s.sanitize();
        (s, a, b)
    }

    #[test]
    fn live_slots_round_trip_and_an_older_session_loads_without_any() {
        let (s, _, _) = live_session();
        let back = Session::from_json(&s.to_json().unwrap()).unwrap();
        assert_eq!(back.live, s.live);
        assert_eq!(back, s);
        // A session written before the Session view has no `live` key.
        let mut v = serde_json::to_value(&s).unwrap();
        v.as_object_mut().unwrap().remove("live");
        assert_eq!(Session::from_json(&v.to_string()).unwrap().live, crate::LiveState::default());
        // A session without live state writes none, and a malformed one loads without it.
        assert!(serde_json::to_value(Session::default()).unwrap().get("live").is_none());
        let mut v = serde_json::to_value(&s).unwrap();
        v["live"] = serde_json::json!({"columns": "x", "quantise": {"Bar": 3}});
        assert_eq!(Session::from_json(&v.to_string()).unwrap().live, crate::LiveState::default());
    }

    #[test]
    fn loading_keeps_columns_whose_track_is_lost_and_drops_slots_without_a_source() {
        let (mut s, a, b) = live_session();
        s.tracks.retain(|t| t.id != b);
        let mut stray = crate::Slot::one_shot("Gone", [0; 3], SourceId(99));
        stray.length_beats = -4.0;
        s.live.columns[0].slots[0] = Some(stray);
        let mut lost_stem = s.live.columns[0].slots[2].clone();
        if let Some(set) = lost_stem.as_mut().and_then(|x| x.stems.as_mut()) {
            set.sources[3] = SourceId(98);
        }
        s.live.columns[0].slots[5] = lost_stem;
        s.live.columns[1].slots[4] = s.live.columns[0].slots[2].clone();
        let lost = s.live.columns[1].clone();
        let back = Session::from_json(&s.to_json().unwrap()).unwrap();
        // The column whose track is gone stays, slots and all, waiting for live.setup.
        assert_eq!(back.live.columns.iter().map(|c| c.track).collect::<Vec<_>>(), [a, b]);
        assert_eq!(back.live.columns[1], lost);
        assert_eq!(back.live.lost_columns(&back.tracks), [1]);
        let col = &back.live.columns[0];
        assert!(col.slots[0].is_none(), "its source is missing");
        let kept = col.slots[5].as_ref().unwrap();
        assert_eq!((&kept.stems, kept.stem_mask, kept.source), (&None, [true; 4], SourceId(7)), "a stem is missing: it plays its own audio");
        assert!(col.slots[2].as_ref().unwrap().stems.is_some());
    }

    #[test]
    fn slot_sources_and_their_stems_count_as_used() {
        let (mut s, _, _) = live_session();
        // Only the slot plays them now.
        s.tracks.iter_mut().flat_map(|t| t.playlists.iter_mut()).for_each(|p| p.clips.clear());
        for src in [7, 11, 12, 13, 14] {
            assert_eq!(s.slots_using(SourceId(src)).map(|(c, r, _)| (c, r)).collect::<Vec<_>>(), [(0, 2)], "source {src}");
            assert!(s.used_sources().any(|x| x == SourceId(src)), "source {src}");
        }
        assert_eq!(s.slots_using(SourceId(8)).count(), 0);
        assert!(!s.used_sources().any(|x| x == SourceId(8)));
    }
}
