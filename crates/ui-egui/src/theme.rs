//! Design tokens. djforge Dark is built from mixforge's studio palette; Light and SoundCraft
//! Classic (SoundCraft's grey dark look) were chosen by eye. They are ours.

use egui::{Color32, FontFamily, FontId};
use std::sync::atomic::{AtomicU8, Ordering};

/// The active [`Palette`] (its [`Palette::index`]). djforge Dark is the default.
static ACTIVE_PALETTE: AtomicU8 = AtomicU8::new(0);

/// Unit tests that change or read the process-wide palette must own it until their assertions
/// finish. The lock is re-entrant per thread, so a test may hold it and still build apps
/// (`SoundApp::new` takes it too) without deadlocking itself.
#[cfg(test)]
pub(crate) fn lock_test_palette() -> TestPaletteGuard {
    TestPaletteGuard::acquire()
}

/// RAII handle for the re-entrant, per-thread test palette lock; see [`lock_test_palette`].
#[cfg(test)]
pub(crate) struct TestPaletteGuard;

#[cfg(test)]
impl TestPaletteGuard {
    fn acquire() -> Self {
        palette_lock::acquire();
        Self
    }
}

#[cfg(test)]
impl Drop for TestPaletteGuard {
    fn drop(&mut self) {
        palette_lock::release();
    }
}

#[cfg(test)]
mod palette_lock {
    use std::cell::{Cell, RefCell};
    use std::sync::{Mutex, MutexGuard, PoisonError};

    static PALETTE: Mutex<()> = Mutex::new(());

    thread_local! {
        /// How many guards this thread holds; the OS lock is taken on 0→1 and released on 1→0.
        static DEPTH: Cell<usize> = const { Cell::new(0) };
        static GUARD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
    }

    pub(super) fn acquire() {
        DEPTH.with(|depth| {
            if depth.get() == 0 {
                GUARD.with(|guard| *guard.borrow_mut() = Some(PALETTE.lock().unwrap_or_else(PoisonError::into_inner)));
            }
            depth.set(depth.get() + 1);
        });
    }

    pub(super) fn release() {
        DEPTH.with(|depth| {
            let left = depth.get().saturating_sub(1);
            depth.set(left);
            if left == 0 {
                GUARD.with(|guard| *guard.borrow_mut() = None);
            }
        });
    }
}

/// The Appearance setting (`ui.theme {mode}`). djforge Dark by default; Light, System (Light or
/// djforge Dark, as the OS is) and SoundCraft Classic are opt-in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    System,
    Light,
    /// djforge Dark. Prefs saved as `dark` before it existed get it too.
    #[default]
    Dark,
    /// SoundCraft's grey dark look.
    Classic,
}

impl ThemeMode {
    /// In the order the theme menu lists them.
    pub const ALL: [Self; 4] = [Self::Dark, Self::Light, Self::System, Self::Classic];

    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "djforge Dark",
            Self::Classic => "SoundCraft Classic",
        }
    }

    /// The id used in `ui.theme {"mode": …}` and in the saved UI prefs.
    pub const fn id(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
            Self::Classic => "classic",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.id().eq_ignore_ascii_case(s.trim()))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Tokens {
    pub window_bg: Color32,
    pub panel_bg: Color32,
    pub panel_bg2: Color32,
    pub toolbar_bg: Color32,
    pub toolbar_group: Color32,
    pub border: Color32,
    pub border_light: Color32,
    /// The outline of an idle control's box: an unticked checkbox or radio button, a text field,
    /// a button. At least 3:1 on the panel and on a window's fill, so an empty box reads.
    pub widget_edge: Color32,
    /// The lit top/left edge of raised panels: track headers and Mix strips.
    pub edge_light: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_dark: Color32,
    pub header_text: Color32,
    pub counter_bg: Color32,
    pub counter_text: Color32,
    pub counter_label: Color32,
    pub accent: Color32,
    pub accent_dark: Color32,
    pub mode_on: Color32,
    pub mode_on_text: Color32,
    pub mode_off_text: Color32,
    pub button: Color32,
    pub button_hi: Color32,
    pub button_border: Color32,
    pub name_field: Color32,
    pub name_field_sel: Color32,
    pub ruler_bg: Color32,
    pub ruler_label_bg: Color32,
    pub ruler_text: Color32,
    pub ruler_tick: Color32,
    pub tempo_ruler: Color32,
    pub meter_ruler: Color32,
    pub marker_ruler: Color32,
    pub playlist_bg: Color32,
    pub playlist_alt: Color32,
    pub grid_line: Color32,
    pub bar_line: Color32,
    pub selection: Color32,
    pub playhead: Color32,
    pub insertion: Color32,
    pub solo: Color32,
    pub mute: Color32,
    pub rec: Color32,
    pub input: Color32,
    pub auto_read: Color32,
    pub auto_write: Color32,
    pub auto_touch: Color32,
    pub auto_latch: Color32,
    pub meter_green: Color32,
    pub meter_yellow: Color32,
    pub meter_red: Color32,
    pub fader_track: Color32,
    pub strip_bg: Color32,
    pub strip_section: Color32,
    pub slot_bg: Color32,
    pub automation_line: Color32,
    /// Menu bar background.
    pub menu_bar: Color32,
    /// Outline of toolbar groups.
    pub group_border: Color32,
    /// Edit-mode cell, not selected.
    pub mode_off: Color32,
    /// Counter-panel toggle (Grid, Count Off), off.
    pub counter_off: Color32,
    /// Outline of the counter panels.
    pub counter_border: Color32,
    /// Divider inside the counter panels.
    pub counter_rule: Color32,
    /// Smart-tool bracket, off.
    pub tool_bracket_off: Color32,
    /// Selected row in the Tracks, Groups and Clips lists.
    pub row_selected: Color32,
    /// Track-visible dot in the Tracks list.
    pub visible_dot: Color32,
    /// Big Counter background.
    pub big_counter_bg: Color32,
    /// Edit-window track header, selected.
    pub header_selected: Color32,
    /// Mix-window strip, selected.
    pub strip_selected: Color32,
    /// Marker names on the marker ruler.
    pub marker_text: Color32,
    /// Alternate-playlist lanes.
    pub playlist_lane: Color32,
    /// Universe overview background.
    pub universe_bg: Color32,
    /// Universe visible-area outline.
    pub universe_view: Color32,
    /// Meter background.
    pub meter_bg: Color32,
    /// Meter clip indicator, not clipped.
    pub meter_clip_off: Color32,
    /// Pan knob body.
    pub knob_bg: Color32,
    /// Pan knob outline.
    pub knob_ring: Color32,
    /// Pan knob pointer.
    pub knob_pointer: Color32,
    /// Mix-window section captions (INSERTS A-E, I/O…).
    pub section_label: Color32,
    /// Plugin slots, as Pro Tools draws them: the well they sit in, an empty slot, a used slot's
    /// face (top, bottom), a bypassed one's, and the dots, letters and arrows on them.
    pub slot_well: Color32,
    pub slot_empty: Color32,
    pub slot_face: [Color32; 2],
    pub slot_bypassed: [Color32; 2],
    pub slot_mark: Color32,
    /// Text and icons on an accent fill (a default dialog button, a selected icon button).
    pub accent_text: Color32,
    /// The open tab in the Edit window's dock tab strip (its label is [`Tokens::accent_text`]).
    pub tab_on: Color32,
    /// The transport's Play key while playing, and its icon.
    pub transport_play: Color32,
    pub transport_play_icon: Color32,
    /// The transport's Record key while recording.
    pub transport_rec: Color32,
    /// Ink on the black volume/pan readouts in Edit-window track headers.
    pub readout_ink: Color32,
    /// The MIDI editor's rubber-band selection (outline; its fill is a faint wash of it).
    pub marquee: Color32,
    /// Piano-roll rows, `[white key, black key]`: outside the key, in its scale, on its tonic.
    pub roll_rows: [[Color32; 2]; 3],
    /// Piano-roll lines: beat, bar, octave.
    pub roll_lines: [Color32; 3],
    /// The MIDI editor's velocity lane.
    pub roll_lane: Color32,
    /// Empty step-sequencer cells, alternating by beat.
    pub step_cells: [Color32; 2],
    /// Selected text and the label of a selected menu row (on [`Tokens::accent_dark`]).
    pub selected_text: Color32,
    /// The tempo editor graph: background, line and points, scale labels.
    pub tempo_graph: [Color32; 3],
    /// The key-signature ruler: background, default-key text, change line, change text.
    pub key_ruler: [Color32; 4],
    /// The chords ruler's background.
    pub chords_ruler: Color32,
    /// A marker without a colour of its own.
    pub marker_default: Color32,
    /// The "FX" badge on clips with clip effects.
    pub fx_badge: Color32,
    /// Floating popups painted by hand (the clip-gain fader): face and outline.
    pub popup_bg: Color32,
    pub popup_border: Color32,
    /// A track's timebase button: ticks (bars and beats) and samples.
    pub timebase_ticks: Color32,
    pub timebase_samples: Color32,
    /// The surround panner's divergence circle: fill (drawn faint) and outline.
    pub panner_spread: [Color32; 2],
    /// A held key on the on-screen MIDI keyboard.
    pub key_held: Color32,
    /// A send's level box in a Mix slot: empty and filled.
    pub level_well: [Color32; 2],
    /// The record-enable button while off: its dot, top and bottom.
    pub rec_idle: [Color32; 2],
    /// DJ transition markers: mix-in, bass hit and mix-out (each 3:1 or more on the rulers and
    /// the lanes).
    pub dj_marks: [Color32; 3],
    /// The outline of a transition marker's flag, and (translucent) the halo along its line.
    pub dj_mark_edge: Color32,
}

/// SoundCraft's piano-roll rows, lines and step cells: dark in Light and Classic.
const ROLL_ROWS: [[Color32; 2]; 3] = [
    [Color32::from_rgb(40, 40, 42), Color32::from_rgb(30, 30, 32)],
    [Color32::from_rgb(44, 50, 60), Color32::from_rgb(34, 40, 50)],
    [Color32::from_rgb(50, 60, 76), Color32::from_rgb(40, 50, 66)],
];
const ROLL_LINES: [Color32; 3] = [Color32::from_rgb(50, 50, 54), Color32::from_rgb(80, 80, 86), Color32::from_rgb(64, 64, 68)];
const STEP_CELLS: [Color32; 2] = [Color32::from_rgb(46, 46, 50), Color32::from_rgb(36, 36, 40)];

/// mixforge's studio palette, which djforge Dark is built from.
pub mod studio {
    use egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(0x11, 0x13, 0x17);
    pub const PANEL: Color32 = Color32::from_rgb(0x19, 0x1c, 0x22);
    pub const PANEL2: Color32 = Color32::from_rgb(0x20, 0x24, 0x2c);
    pub const LINE: Color32 = Color32::from_rgb(0x2c, 0x31, 0x3b);
    pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe8, 0xec);
    pub const DIM: Color32 = Color32::from_rgb(0x8b, 0x93, 0xa3);
    /// Yellow: selection, the playhead, the counters, active buttons.
    pub const ACCENT: Color32 = Color32::from_rgb(0xf8, 0xd2, 0x00);
    /// EQ band colours: low, mid and high.
    pub const LOW: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);
    pub const MID: Color32 = Color32::from_rgb(0x46, 0xc4, 0x6a);
    pub const HIGH: Color32 = Color32::from_rgb(0x4f, 0x8f, 0xf7);
    /// Green: input, playing, meters below the warning level.
    pub const IN: Color32 = Color32::from_rgb(0x32, 0xbe, 0x44);
    /// Red: record arm, recording, clipping.
    pub const OUT: Color32 = Color32::from_rgb(0xe0, 0x34, 0x2c);
    /// djforge Dark's one selection colour, a dark yellow-brown: behind selected rows, headers,
    /// strips and text. Text, dim text and the accent all read on it.
    pub const SELECTED: Color32 = Color32::from_rgb(72, 62, 6);
    /// DJ deck tints: Deck A cyan, Deck B magenta. Both read on [`BG`] (9:1 and 4.9:1) and stay
    /// clear of the band colours and of in, out and the accent (CIE76 ΔE 54 or more from each;
    /// the tests ask for 50), so a tint never reads as a frequency or a state.
    pub const DECK_A: Color32 = Color32::from_rgb(0x26, 0xc6, 0xda);
    pub const DECK_B: Color32 = Color32::from_rgb(0xc6, 0x4f, 0xd6);
}

impl Tokens {
    /// djforge Dark, the default: mixforge's studio palette ([`studio`]).
    pub const DARK: Tokens = {
        use studio::*;
        Tokens {
            window_bg: BG,
            panel_bg: PANEL,
            panel_bg2: PANEL2,
            toolbar_bg: BG,
            toolbar_group: Color32::from_rgb(13, 15, 18),
            border: Color32::from_rgb(8, 9, 11),
            border_light: LINE,
            widget_edge: Color32::from_rgb(0x6e, 0x76, 0x86),
            edge_light: Color32::from_rgba_premultiplied(22, 24, 28, 22),
            text: TEXT,
            text_dim: DIM,
            text_dark: BG,
            header_text: Color32::from_rgb(210, 214, 222),
            counter_bg: Color32::from_rgb(8, 9, 11),
            counter_text: ACCENT,
            counter_label: Color32::from_rgb(200, 204, 212),
            accent: ACCENT,
            // Behind selected text and selected list rows, which are drawn in the accent.
            accent_dark: SELECTED,
            mode_on: ACCENT,
            mode_on_text: BG,
            mode_off_text: Color32::from_rgb(214, 182, 20),
            button: Color32::from_rgb(40, 45, 54),
            button_hi: Color32::from_rgb(54, 60, 72),
            button_border: Color32::from_rgb(8, 9, 11),
            name_field: Color32::from_rgb(220, 223, 229),
            name_field_sel: Color32::from_rgb(236, 238, 242),
            ruler_bg: Color32::from_rgb(28, 31, 38),
            ruler_label_bg: Color32::from_rgb(28, 31, 38),
            ruler_text: Color32::from_rgb(178, 184, 196),
            ruler_tick: Color32::from_rgb(80, 88, 102),
            tempo_ruler: Color32::from_rgb(30, 96, 50),
            meter_ruler: Color32::from_rgb(36, 76, 140),
            marker_ruler: Color32::from_rgb(34, 38, 46),
            playlist_bg: Color32::from_rgb(22, 25, 30),
            playlist_alt: Color32::from_rgb(26, 29, 35),
            // The lane with a white veil (6 % and 13 %): grid lines are drawn as one, over clips too.
            grid_line: Color32::from_rgb(36, 39, 44),
            bar_line: Color32::from_rgb(52, 55, 59),
            // The accent at 30 %, premultiplied.
            selection: Color32::from_rgba_premultiplied(74, 63, 0, 77),
            playhead: ACCENT,
            // Brighter than text, so the blinking insertion point is told apart from it.
            insertion: Color32::from_rgb(244, 246, 248),
            solo: Color32::from_rgb(222, 196, 52),
            mute: Color32::from_rgb(232, 148, 40),
            rec: OUT,
            input: IN,
            auto_read: MID,
            auto_write: OUT,
            auto_touch: Color32::from_rgb(230, 170, 50),
            auto_latch: Color32::from_rgb(160, 120, 220),
            meter_green: IN,
            meter_yellow: ACCENT,
            meter_red: OUT,
            fader_track: Color32::from_rgb(8, 9, 11),
            strip_bg: PANEL2,
            strip_section: PANEL,
            slot_bg: Color32::from_rgb(19, 21, 26),
            automation_line: Color32::from_rgb(240, 240, 240),
            menu_bar: Color32::from_rgb(13, 15, 18),
            group_border: Color32::from_rgb(6, 7, 9),
            mode_off: Color32::from_rgb(34, 32, 20),
            counter_off: Color32::from_rgb(34, 32, 20),
            counter_border: LINE,
            counter_rule: PANEL2,
            tool_bracket_off: Color32::from_rgb(56, 62, 74),
            row_selected: SELECTED,
            visible_dot: Color32::from_rgb(200, 204, 212),
            big_counter_bg: Color32::from_rgb(8, 9, 11),
            header_selected: SELECTED,
            strip_selected: SELECTED,
            marker_text: Color32::from_rgb(236, 238, 242),
            playlist_lane: Color32::from_rgb(19, 21, 26),
            universe_bg: Color32::from_rgb(13, 15, 18),
            universe_view: TEXT,
            meter_bg: Color32::from_rgb(6, 7, 9),
            meter_clip_off: Color32::from_rgb(60, 20, 20),
            knob_bg: Color32::from_rgb(19, 21, 26),
            knob_ring: DIM,
            knob_pointer: TEXT,
            section_label: Color32::from_rgb(200, 204, 212),
            slot_well: PANEL2,
            slot_empty: Color32::from_rgb(28, 31, 38),
            slot_face: [Color32::from_rgb(76, 84, 98), Color32::from_rgb(62, 69, 81)],
            slot_bypassed: [Color32::from_rgb(60, 100, 170), Color32::from_rgb(46, 80, 140)],
            slot_mark: Color32::from_rgb(159, 166, 180),
            accent_text: BG,
            tab_on: ACCENT,
            transport_play: Color32::from_rgb(22, 70, 32),
            transport_play_icon: IN,
            transport_rec: Color32::from_rgb(96, 24, 20),
            readout_ink: ACCENT,
            marquee: ACCENT,
            roll_rows: [
                [PANEL2, PANEL],
                [Color32::from_rgb(40, 46, 58), Color32::from_rgb(31, 36, 46)],
                [Color32::from_rgb(50, 58, 76), Color32::from_rgb(41, 48, 64)],
            ],
            roll_lines: [Color32::from_rgb(40, 45, 54), Color32::from_rgb(66, 73, 88), Color32::from_rgb(56, 62, 74)],
            roll_lane: Color32::from_rgb(19, 21, 26),
            step_cells: [Color32::from_rgb(40, 45, 54), Color32::from_rgb(32, 36, 44)],
            selected_text: ACCENT,
            tempo_graph: [Color32::from_rgb(20, 40, 28), MID, Color32::from_rgb(126, 170, 138)],
            // A muted violet that sits on the near-black.
            key_ruler: [
                Color32::from_rgb(46, 38, 66),
                Color32::from_rgb(214, 208, 232),
                Color32::from_rgb(168, 148, 220),
                Color32::from_rgb(232, 226, 248),
            ],
            chords_ruler: Color32::from_rgb(34, 44, 58),
            marker_default: ACCENT,
            fx_badge: ACCENT,
            popup_bg: PANEL2,
            popup_border: LINE,
            timebase_ticks: ACCENT,
            timebase_samples: HIGH,
            panner_spread: [HIGH, HIGH],
            key_held: ACCENT,
            level_well: [PANEL, LINE],
            rec_idle: [Color32::from_rgb(236, 112, 104), Color32::from_rgb(180, 44, 38)],
            dj_marks: [IN, ACCENT, OUT],
            dj_mark_edge: BG,
        }
    };

    /// SoundCraft Classic: SoundCraft's grey dark palette, unchanged.
    pub const CLASSIC: Tokens = Tokens {
        window_bg: Color32::from_rgb(30, 30, 30),
        panel_bg: Color32::from_rgb(37, 37, 38),
        panel_bg2: Color32::from_rgb(44, 44, 45),
        toolbar_bg: Color32::from_rgb(30, 30, 31),
        toolbar_group: Color32::from_rgb(24, 24, 25),
        border: Color32::from_rgb(14, 14, 14),
        border_light: Color32::from_rgb(64, 64, 66),
        widget_edge: Color32::from_rgb(120, 120, 123),
        edge_light: Color32::from_rgba_premultiplied(20, 20, 20, 20),
        text: Color32::from_rgb(214, 214, 214),
        text_dim: Color32::from_rgb(150, 150, 152),
        text_dark: Color32::from_rgb(16, 16, 16),
        header_text: Color32::from_rgb(196, 196, 198),
        counter_bg: Color32::from_rgb(4, 4, 4),
        counter_text: Color32::from_rgb(104, 220, 120),
        counter_label: Color32::from_rgb(200, 200, 200),
        accent: Color32::from_rgb(64, 132, 196),
        accent_dark: Color32::from_rgb(40, 92, 146),
        mode_on: Color32::from_rgb(92, 196, 108),
        mode_on_text: Color32::from_rgb(10, 30, 12),
        mode_off_text: Color32::from_rgb(98, 196, 112),
        button: Color32::from_rgb(58, 58, 60),
        button_hi: Color32::from_rgb(78, 78, 80),
        button_border: Color32::from_rgb(20, 20, 20),
        name_field: Color32::from_rgb(208, 208, 208),
        name_field_sel: Color32::from_rgb(232, 232, 232),
        ruler_bg: Color32::from_rgb(40, 40, 41),
        ruler_label_bg: Color32::from_rgb(40, 40, 41),
        ruler_text: Color32::from_rgb(178, 178, 180),
        ruler_tick: Color32::from_rgb(96, 96, 98),
        tempo_ruler: Color32::from_rgb(52, 128, 88),
        meter_ruler: Color32::from_rgb(44, 108, 140),
        marker_ruler: Color32::from_rgb(46, 46, 48),
        playlist_bg: Color32::from_rgb(36, 36, 37),
        playlist_alt: Color32::from_rgb(40, 40, 41),
        grid_line: Color32::from_rgb(52, 52, 54),
        bar_line: Color32::from_rgb(66, 66, 70),
        selection: Color32::from_rgba_premultiplied(40, 70, 110, 110),
        playhead: Color32::from_rgb(232, 60, 50),
        insertion: Color32::from_rgb(230, 230, 230),
        solo: Color32::from_rgb(222, 196, 52),
        mute: Color32::from_rgb(232, 148, 40),
        rec: Color32::from_rgb(214, 52, 46),
        input: Color32::from_rgb(70, 170, 90),
        auto_read: Color32::from_rgb(96, 200, 110),
        auto_write: Color32::from_rgb(220, 70, 60),
        auto_touch: Color32::from_rgb(230, 170, 50),
        auto_latch: Color32::from_rgb(160, 120, 220),
        meter_green: Color32::from_rgb(60, 200, 80),
        meter_yellow: Color32::from_rgb(230, 210, 60),
        meter_red: Color32::from_rgb(230, 50, 40),
        fader_track: Color32::from_rgb(12, 12, 12),
        strip_bg: Color32::from_rgb(46, 46, 48),
        strip_section: Color32::from_rgb(36, 36, 38),
        slot_bg: Color32::from_rgb(28, 28, 29),
        automation_line: Color32::from_rgb(240, 240, 240),
        menu_bar: Color32::from_rgb(22, 22, 23),
        group_border: Color32::from_rgb(8, 8, 8),
        mode_off: Color32::from_rgb(20, 36, 22),
        counter_off: Color32::from_rgb(30, 50, 32),
        counter_border: Color32::from_rgb(50, 50, 50),
        counter_rule: Color32::from_rgb(40, 40, 40),
        tool_bracket_off: Color32::from_rgb(64, 64, 66),
        row_selected: Color32::from_rgb(52, 70, 96),
        visible_dot: Color32::from_rgb(200, 200, 200),
        big_counter_bg: Color32::from_rgb(0, 0, 0),
        header_selected: Color32::from_rgb(52, 58, 66),
        strip_selected: Color32::from_rgb(54, 58, 64),
        marker_text: Color32::from_rgb(236, 236, 236),
        playlist_lane: Color32::from_rgb(30, 30, 31),
        universe_bg: Color32::from_rgb(18, 18, 19),
        universe_view: Color32::from_rgb(255, 255, 255),
        meter_bg: Color32::from_rgb(8, 8, 8),
        meter_clip_off: Color32::from_rgb(60, 20, 20),
        knob_bg: Color32::from_rgb(26, 26, 28),
        knob_ring: Color32::from_rgb(150, 150, 154),
        knob_pointer: Color32::from_rgb(230, 230, 230),
        section_label: Color32::from_rgb(200, 200, 200),
        slot_well: Color32::from_rgb(43, 43, 43),
        slot_empty: Color32::from_rgb(39, 39, 39),
        slot_face: [Color32::from_rgb(92, 92, 92), Color32::from_rgb(75, 75, 75)],
        slot_bypassed: [Color32::from_rgb(66, 96, 146), Color32::from_rgb(50, 76, 120)],
        slot_mark: Color32::from_rgb(159, 159, 159),
        accent_text: Color32::WHITE,
        tab_on: Color32::from_rgb(40, 92, 146),
        transport_play: Color32::from_rgb(40, 90, 50),
        transport_play_icon: Color32::from_rgb(120, 255, 140),
        transport_rec: Color32::from_rgb(110, 30, 30),
        readout_ink: Color32::from_rgb(104, 220, 120),
        marquee: Color32::from_rgb(120, 170, 255),
        roll_rows: ROLL_ROWS,
        roll_lines: ROLL_LINES,
        roll_lane: Color32::from_rgb(28, 28, 30),
        step_cells: STEP_CELLS,
        selected_text: Color32::WHITE,
        tempo_graph: [Color32::from_rgb(24, 40, 32), Color32::from_rgb(140, 230, 160), Color32::from_rgb(120, 160, 130)],
        key_ruler: [
            Color32::from_rgb(64, 52, 88),
            Color32::from_rgb(220, 214, 236),
            Color32::from_rgb(200, 180, 240),
            Color32::from_rgb(236, 230, 250),
        ],
        chords_ruler: Color32::from_rgb(40, 56, 60),
        marker_default: Color32::from_rgb(232, 200, 64),
        fx_badge: Color32::from_rgb(250, 230, 120),
        popup_bg: Color32::from_rgb(38, 38, 40),
        popup_border: Color32::from_rgb(10, 10, 10),
        timebase_ticks: Color32::from_rgb(86, 172, 103),
        timebase_samples: Color32::from_rgb(100, 184, 249),
        panner_spread: [Color32::from_rgb(80, 160, 230), Color32::from_rgb(70, 130, 190)],
        key_held: Color32::from_rgb(90, 160, 240),
        level_well: [Color32::from_rgb(36, 38, 8), Color32::from_rgb(51, 49, 11)],
        rec_idle: [Color32::from_rgb(218, 112, 106), Color32::from_rgb(176, 76, 72)],
        // New with djforge (SoundCraft had no DJ markers): djforge Dark's, which read on its greys.
        dj_marks: [studio::IN, studio::ACCENT, studio::OUT],
        dj_mark_edge: Color32::from_rgb(10, 10, 10),
    };

    pub const LIGHT: Tokens = Tokens {
        window_bg: Color32::from_rgb(184, 186, 190),
        panel_bg: Color32::from_rgb(204, 206, 210),
        panel_bg2: Color32::from_rgb(216, 218, 222),
        toolbar_bg: Color32::from_rgb(194, 196, 200),
        toolbar_group: Color32::from_rgb(208, 210, 214),
        border: Color32::from_rgb(100, 103, 109),
        border_light: Color32::from_rgb(150, 153, 159),
        widget_edge: Color32::from_rgb(100, 103, 109),
        edge_light: Color32::from_rgba_premultiplied(150, 150, 150, 150),
        text: Color32::from_rgb(24, 25, 28),
        text_dim: Color32::from_rgb(68, 70, 76),
        text_dark: Color32::from_rgb(16, 17, 20),
        header_text: Color32::from_rgb(30, 32, 36),
        counter_bg: Color32::from_rgb(224, 226, 228),
        counter_text: Color32::from_rgb(18, 108, 40),
        counter_label: Color32::from_rgb(54, 57, 63),
        accent: Color32::from_rgb(62, 116, 180),
        accent_dark: Color32::from_rgb(43, 88, 145),
        // Dark enough for white text (4.5:1).
        mode_on: Color32::from_rgb(52, 128, 66),
        mode_on_text: Color32::from_rgb(255, 255, 255),
        mode_off_text: Color32::from_rgb(30, 96, 44),
        button: Color32::from_rgb(214, 216, 220),
        button_hi: Color32::from_rgb(200, 203, 208),
        button_border: Color32::from_rgb(108, 111, 117),
        name_field: Color32::from_rgb(236, 237, 239),
        name_field_sel: Color32::from_rgb(224, 232, 244),
        ruler_bg: Color32::from_rgb(198, 200, 204),
        ruler_label_bg: Color32::from_rgb(198, 200, 204),
        ruler_text: Color32::from_rgb(48, 50, 56),
        ruler_tick: Color32::from_rgb(108, 111, 118),
        tempo_ruler: Color32::from_rgb(92, 156, 112),
        meter_ruler: Color32::from_rgb(80, 144, 172),
        marker_ruler: Color32::from_rgb(186, 189, 194),
        playlist_bg: Color32::from_rgb(210, 212, 216),
        playlist_alt: Color32::from_rgb(202, 204, 208),
        grid_line: Color32::from_rgb(188, 191, 196),
        bar_line: Color32::from_rgb(162, 166, 172),
        selection: Color32::from_rgba_premultiplied(100, 145, 205, 90),
        playhead: Color32::from_rgb(210, 48, 42),
        insertion: Color32::from_rgb(35, 36, 40),
        solo: Color32::from_rgb(178, 145, 20),
        mute: Color32::from_rgb(194, 106, 22),
        rec: Color32::from_rgb(190, 42, 38),
        input: Color32::from_rgb(45, 126, 64),
        auto_read: Color32::from_rgb(52, 142, 68),
        auto_write: Color32::from_rgb(190, 50, 45),
        auto_touch: Color32::from_rgb(180, 122, 24),
        auto_latch: Color32::from_rgb(118, 80, 170),
        meter_green: Color32::from_rgb(36, 160, 58),
        meter_yellow: Color32::from_rgb(190, 162, 20),
        meter_red: Color32::from_rgb(195, 42, 35),
        fader_track: Color32::from_rgb(120, 123, 129),
        strip_bg: Color32::from_rgb(198, 200, 204),
        strip_section: Color32::from_rgb(210, 212, 216),
        slot_bg: Color32::from_rgb(194, 196, 201),
        automation_line: Color32::from_rgb(30, 32, 38),
        menu_bar: Color32::from_rgb(204, 206, 210),
        group_border: Color32::from_rgb(118, 121, 127),
        mode_off: Color32::from_rgb(202, 212, 204),
        counter_off: Color32::from_rgb(192, 212, 198),
        counter_border: Color32::from_rgb(118, 121, 127),
        counter_rule: Color32::from_rgb(188, 191, 196),
        tool_bracket_off: Color32::from_rgb(172, 175, 181),
        row_selected: Color32::from_rgb(174, 196, 226),
        visible_dot: Color32::from_rgb(86, 89, 96),
        big_counter_bg: Color32::from_rgb(224, 226, 228),
        header_selected: Color32::from_rgb(184, 200, 224),
        strip_selected: Color32::from_rgb(184, 200, 224),
        marker_text: Color32::from_rgb(24, 25, 28),
        playlist_lane: Color32::from_rgb(200, 202, 206),
        universe_bg: Color32::from_rgb(198, 200, 204),
        universe_view: Color32::from_rgb(40, 42, 48),
        meter_bg: Color32::from_rgb(52, 54, 58),
        meter_clip_off: Color32::from_rgb(92, 54, 54),
        knob_bg: Color32::from_rgb(232, 233, 236),
        knob_ring: Color32::from_rgb(116, 120, 128),
        knob_pointer: Color32::from_rgb(30, 32, 38),
        section_label: Color32::from_rgb(56, 58, 64),
        slot_well: Color32::from_rgb(212, 214, 218),
        slot_empty: Color32::from_rgb(198, 200, 205),
        slot_face: [Color32::from_rgb(244, 245, 247), Color32::from_rgb(214, 216, 220)],
        slot_bypassed: [Color32::from_rgb(190, 210, 240), Color32::from_rgb(160, 186, 228)],
        slot_mark: Color32::from_rgb(92, 95, 102),
        accent_text: Color32::WHITE,
        tab_on: Color32::from_rgb(43, 88, 145),
        transport_play: Color32::from_rgb(40, 90, 50),
        transport_play_icon: Color32::from_rgb(120, 255, 140),
        transport_rec: Color32::from_rgb(110, 30, 30),
        // The readout is black in every palette.
        readout_ink: Color32::from_rgb(104, 220, 120),
        marquee: Color32::from_rgb(120, 170, 255),
        // The MIDI editor stays dark in Light, as it always has.
        roll_rows: ROLL_ROWS,
        roll_lines: ROLL_LINES,
        roll_lane: Color32::from_rgb(28, 28, 30),
        step_cells: STEP_CELLS,
        selected_text: Color32::WHITE,
        tempo_graph: [Color32::from_rgb(24, 40, 32), Color32::from_rgb(140, 230, 160), Color32::from_rgb(120, 160, 130)],
        key_ruler: [
            Color32::from_rgb(64, 52, 88),
            Color32::from_rgb(220, 214, 236),
            Color32::from_rgb(200, 180, 240),
            Color32::from_rgb(236, 230, 250),
        ],
        chords_ruler: Color32::from_rgb(40, 56, 60),
        marker_default: Color32::from_rgb(232, 200, 64),
        fx_badge: Color32::from_rgb(250, 230, 120),
        // Light, so the counter-green readout on it is readable (it was the dark grey).
        popup_bg: Color32::from_rgb(216, 218, 222),
        popup_border: Color32::from_rgb(100, 103, 109),
        timebase_ticks: Color32::from_rgb(86, 172, 103),
        timebase_samples: Color32::from_rgb(100, 184, 249),
        panner_spread: [Color32::from_rgb(80, 160, 230), Color32::from_rgb(70, 130, 190)],
        key_held: Color32::from_rgb(90, 160, 240),
        level_well: [Color32::from_rgb(36, 38, 8), Color32::from_rgb(51, 49, 11)],
        rec_idle: [Color32::from_rgb(218, 112, 106), Color32::from_rgb(176, 76, 72)],
        // Darker than djforge Dark's, to read on the pale rulers and lanes: green, amber, red.
        dj_marks: [Color32::from_rgb(0x1e, 0x7a, 0x2e), Color32::from_rgb(0x7a, 0x5c, 0x00), Color32::from_rgb(0xb3, 0x26, 0x1e)],
        dj_mark_edge: Color32::WHITE,
    };

    pub const fn of(palette: Palette) -> Tokens {
        match palette {
            Palette::Dark => Self::DARK,
            Palette::Light => Self::LIGHT,
            Palette::Classic => Self::CLASSIC,
        }
    }

    pub fn current() -> Tokens {
        Self::of(palette())
    }
}

/// A palette the UI is drawn in; [`ThemeMode::System`] resolves to Dark or Light.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Palette {
    Dark,
    Light,
    Classic,
}

impl Palette {
    pub const ALL: [Self; 3] = [Self::Dark, Self::Light, Self::Classic];

    /// Whether waveforms are coloured by frequency unless the UI prefs say otherwise: in
    /// djforge Dark only.
    pub const fn colours_waveforms(self) -> bool {
        matches!(self, Self::Dark)
    }

    /// Whether the DJ decks' clips are tinted with their deck's colour unless the UI prefs say
    /// otherwise: in djforge Dark only (Light's clip bodies are pale and Classic is unchanged).
    pub const fn tints_decks(self) -> bool {
        matches!(self, Self::Dark)
    }

    const fn index(self) -> u8 {
        match self {
            Self::Dark => 0,
            Self::Light => 1,
            Self::Classic => 2,
        }
    }

    const fn from_index(index: u8) -> Self {
        match index {
            1 => Self::Light,
            2 => Self::Classic,
            _ => Self::Dark,
        }
    }
}

/// The active palette.
pub fn palette() -> Palette {
    Palette::from_index(ACTIVE_PALETTE.load(Ordering::Relaxed))
}

/// Whether the Light palette is the active one.
pub fn is_light() -> bool {
    palette() == Palette::Light
}

/// The palette `mode` asks for, resolved each frame from the integration's current OS
/// appearance: System with a dark or unknown appearance is djforge Dark.
pub fn resolve(ctx: &egui::Context, mode: ThemeMode) -> Palette {
    match mode {
        ThemeMode::Light => Palette::Light,
        ThemeMode::Dark => Palette::Dark,
        ThemeMode::Classic => Palette::Classic,
        ThemeMode::System if ctx.system_theme() == Some(egui::Theme::Light) => Palette::Light,
        ThemeMode::System => Palette::Dark,
    }
}

/// A track's header and lane backgrounds in the Edit window, tinted with its colour by `tint` (0..1).
pub fn track_backgrounds(color: [u8; 3], selected: bool, tint: f32) -> (Color32, Color32) {
    let t = Tokens::current();
    let k = tint.clamp(0.0, 1.0);
    if k <= 0.0 {
        return if selected { (t.header_selected, t.playlist_alt) } else { (t.panel_bg2, t.playlist_bg) };
    }
    let c = rgb(color);
    let header = t.panel_bg2.lerp_to_gamma(c, k * 0.55);
    let header = if selected { crate::widgets::shade(header, 0.1) } else { header };
    let lane = if selected { t.playlist_alt } else { t.playlist_bg }.lerp_to_gamma(c, k * 0.2);
    (header, lane)
}

/// Track colours tinted for clip bodies.
pub fn clip_colors(rgb: [u8; 3], selected: bool) -> (Color32, Color32, Color32) {
    let [r, g, b] = rgb;
    let shade = |k: f32| Color32::from_rgb((f32::from(r) * k) as u8, (f32::from(g) * k) as u8, (f32::from(b) * k) as u8);
    let tint = |k: f32| {
        let t = |c: u8| (f32::from(c) + (255.0 - f32::from(c)) * k) as u8;
        Color32::from_rgb(t(r), t(g), t(b))
    };
    let bar = shade(0.9);
    if is_light() {
        // A paler body and a waveform in a deep shade of the clip's own colour.
        return if selected { (tint(0.2), bar, shade(0.22)) } else { (tint(0.45), bar, shade(0.32)) };
    }
    // A dark body and a light waveform in the clip's own colour; selection inverts them.
    if selected { (shade(0.7), bar, Color32::from_rgb(16, 16, 18)) } else { (shade(0.34), bar, tint(0.35)) }
}

/// How a clip whose waveform is coloured by frequency is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandClip {
    /// A dark tint of the clip's colour under the band colours, in every palette: the band colours
    /// need a dark ground, so a clip coloured by frequency in Light or SoundCraft Classic (where
    /// it is off unless asked for) is dark there too.
    pub body: Color32,
    /// The name bar (or the strip along the top when names are hidden): the clip's colour, pale
    /// when selected.
    pub bar: Color32,
    /// The selected clip's outline, in the palette's accent.
    pub outline: Option<Color32>,
    /// The line between a stereo clip's channels.
    pub divider: Color32,
}

/// [`BandClip`] for a clip of colour `rgb`.
pub fn band_clip(rgb: [u8; 3], selected: bool) -> BandClip {
    let c = self::rgb(rgb);
    let body = studio::BG.lerp_to_gamma(c, if selected { 0.24 } else { 0.15 });
    let (_, bar, _) = clip_colors(rgb, false);
    BandClip {
        body,
        bar: if selected { c.lerp_to_gamma(Color32::WHITE, 0.55) } else { bar },
        outline: selected.then(|| Tokens::current().accent),
        divider: body.lerp_to_gamma(Color32::WHITE, 0.3),
    }
}

pub fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Make Light (or djforge Dark) the active palette and apply its visuals ([`apply_palette`]).
pub fn apply(ctx: &egui::Context, light: bool) {
    apply_palette(ctx, if light { Palette::Light } else { Palette::Dark });
}

/// Make `palette` the active one and apply its visuals. Call only when it changes.
pub fn apply_palette(ctx: &egui::Context, palette: Palette) {
    ACTIVE_PALETTE.store(palette.index(), Ordering::Relaxed);
    let t = Tokens::of(palette);
    let mut v = if palette == Palette::Light { egui::Visuals::light() } else { egui::Visuals::dark() };
    v.panel_fill = t.panel_bg;
    v.window_fill = t.panel_bg2;
    v.extreme_bg_color = t.slot_bg;
    v.faint_bg_color = t.panel_bg2;
    v.selection.bg_fill = t.accent;
    v.widgets.inactive.weak_bg_fill = t.button;
    v.widgets.inactive.bg_fill = t.button;
    v.widgets.hovered.weak_bg_fill = t.button_hi;
    v.widgets.noninteractive.fg_stroke.color = t.text;
    v.widgets.inactive.fg_stroke.color = t.text;
    // egui draws an idle control's box without an outline, so an unticked checkbox, a radio
    // button or an empty text field vanished into the panel.
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, t.widget_edge);
    if palette == Palette::Dark {
        // Selected text and rows: the accent on a dark yellow, readable either way round. The
        // same accent outlines a focused text field.
        v.selection.bg_fill = t.accent_dark;
        v.selection.stroke = egui::Stroke::new(1.0, t.selected_text);
        // A focused button is drawn in the active state: outline it in the accent.
        v.widgets.active.bg_stroke = egui::Stroke::new(1.0, t.accent);
        v.hyperlink_color = t.accent;
        // egui's neutral greys, in the palette's blue-greys.
        v.window_stroke = egui::Stroke::new(1.0, t.border_light);
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, t.border_light);
        v.widgets.noninteractive.bg_fill = t.panel_bg2;
        v.widgets.noninteractive.weak_bg_fill = t.panel_bg2;
        v.widgets.hovered.bg_fill = t.button_hi;
        v.widgets.active.bg_fill = t.button_hi;
        v.widgets.active.weak_bg_fill = t.button_hi;
        v.widgets.open.bg_fill = t.button_hi;
        v.widgets.open.weak_bg_fill = t.button_hi;
        v.widgets.open.bg_stroke = egui::Stroke::new(1.0, t.border_light);
        v.code_bg_color = t.slot_bg;
    }
    v.window_corner_radius = egui::CornerRadius::same(6);
    v.menu_corner_radius = egui::CornerRadius::same(4);
    // Keep receiving native OS appearance changes even when our palette is manual. On macOS,
    // pinning egui's theme also pins the window appearance and hides later OS changes.
    ctx.set_theme(egui::ThemePreference::System);
    // Both egui style branches must use our resolved palette: egui can select either branch
    // as the OS changes, including while SoundCraft's manual Light/Dark choice stays fixed.
    ctx.all_styles_mut(|s| {
        s.visuals = v.clone();
        s.spacing.item_spacing = egui::vec2(6.0, 4.0);
        s.spacing.button_padding = egui::vec2(6.0, 2.0);
        s.interaction.tooltip_delay = 0.4;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Services, SoundApp, UiState};
    use serde_json::json;

    /// Every colour SoundCraft's dark palette had, in declaration order, folded into an FNV-1a hash.
    fn soundcraft_fingerprint(t: &Tokens) -> u64 {
        let mut colors = vec![
            t.window_bg,
            t.panel_bg,
            t.panel_bg2,
            t.toolbar_bg,
            t.toolbar_group,
            t.border,
            t.border_light,
            t.edge_light,
            t.text,
            t.text_dim,
            t.text_dark,
            t.header_text,
            t.counter_bg,
            t.counter_text,
            t.counter_label,
            t.accent,
            t.accent_dark,
            t.mode_on,
            t.mode_on_text,
            t.mode_off_text,
            t.button,
            t.button_hi,
            t.button_border,
            t.name_field,
            t.name_field_sel,
            t.ruler_bg,
            t.ruler_label_bg,
            t.ruler_text,
            t.ruler_tick,
            t.tempo_ruler,
            t.meter_ruler,
            t.marker_ruler,
            t.playlist_bg,
            t.playlist_alt,
            t.grid_line,
            t.bar_line,
            t.selection,
            t.playhead,
            t.insertion,
            t.solo,
            t.mute,
            t.rec,
            t.input,
            t.auto_read,
            t.auto_write,
            t.auto_touch,
            t.auto_latch,
            t.meter_green,
            t.meter_yellow,
            t.meter_red,
            t.fader_track,
            t.strip_bg,
            t.strip_section,
            t.slot_bg,
            t.automation_line,
            t.menu_bar,
            t.group_border,
            t.mode_off,
            t.counter_off,
            t.counter_border,
            t.counter_rule,
            t.tool_bracket_off,
            t.row_selected,
            t.visible_dot,
            t.big_counter_bg,
            t.header_selected,
            t.strip_selected,
            t.marker_text,
            t.playlist_lane,
            t.universe_bg,
            t.universe_view,
            t.meter_bg,
            t.meter_clip_off,
            t.knob_bg,
            t.knob_ring,
            t.knob_pointer,
            t.section_label,
            t.slot_well,
            t.slot_empty,
            t.slot_mark,
        ];
        colors.extend(t.slot_face);
        colors.extend(t.slot_bypassed);
        colors.iter().flat_map(|c| c.to_array()).fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
    }

    #[test]
    fn test_palette_lock_is_reentrant_and_excludes_other_threads() {
        // Regression for the parallel-run flakes: the process-wide palette must be owned for a
        // whole test, re-entrant on the owning thread (so `SoundApp::new` can take it too), and
        // exclusive against other threads (so a frame cannot flip it mid-test).
        let outer = lock_test_palette();
        let inner = lock_test_palette();
        let app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _held = lock_test_palette();
            let _ = tx.send(());
        });
        assert!(rx.recv_timeout(std::time::Duration::from_millis(100)).is_err(), "another thread must wait for the palette lock");
        drop(inner);
        drop(outer);
        drop(app);
        // Other tests compete for the lock too, and some hold it for seconds: allow for a queue.
        assert!(rx.recv_timeout(std::time::Duration::from_secs(120)).is_ok(), "the palette lock must be released when every holder drops it");
    }

    #[test]
    fn a_selected_clip_coloured_by_frequency_stands_out() {
        let _palette = lock_test_palette();
        for rgb in [[230, 72, 77], [70, 196, 106], [79, 143, 247], [248, 210, 0], [128, 128, 128], [40, 40, 44], [255, 255, 255]] {
            let (on, off) = (band_clip(rgb, true), band_clip(rgb, false));
            let outline = on.outline.unwrap_or(off.body);
            assert!(off.outline.is_none(), "{rgb:?}");
            assert!(contrast(outline, off.body) >= 3.0 && contrast(outline, on.body) >= 3.0, "{rgb:?}: outline {outline:?}");
            assert!(contrast(on.bar, off.bar) >= 1.4 || contrast(on.bar, on.body) >= 3.0, "{rgb:?}: bars {:?} {:?}", on.bar, off.bar);
            assert!(contrast(on.divider, on.body) >= 1.5 && contrast(off.divider, off.body) >= 1.5, "{rgb:?}: divider");
            // The band colours read on the body.
            for band in [studio::LOW, studio::MID, studio::HIGH] {
                assert!(contrast(band, on.body) >= 2.0 && contrast(band, off.body) >= 3.0, "{rgb:?}: {band:?}");
            }
        }
    }

    /// WCAG 2 contrast ratio of two opaque colours.
    fn contrast(a: Color32, b: Color32) -> f64 {
        let luminance = |c: Color32| {
            let linear = |channel: u8| {
                let s = f64::from(channel) / 255.0;
                if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * linear(c.r()) + 0.7152 * linear(c.g()) + 0.0722 * linear(c.b())
        };
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn djforge_dark_is_mixforges_studio_palette() {
        let t = Tokens::DARK;
        let hex = |c: Color32| format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
        for (name, color, want) in [
            ("bg", t.window_bg, "#111317"),
            ("panel", t.panel_bg, "#191c22"),
            ("panel2", t.panel_bg2, "#20242c"),
            ("line", t.border_light, "#2c313b"),
            ("text", t.text, "#e6e8ec"),
            ("dim", t.text_dim, "#8b93a3"),
            ("accent", t.accent, "#f8d200"),
            ("selection outline", t.marquee, "#f8d200"),
            ("playhead", t.playhead, "#f8d200"),
            ("counter", t.counter_text, "#f8d200"),
            ("mode on", t.mode_on, "#f8d200"),
            ("mode on text", t.mode_on_text, "#111317"),
            ("open tab", t.tab_on, "#f8d200"),
            ("record", t.rec, "#e0342c"),
            ("input", t.input, "#32be44"),
            ("meter green", t.meter_green, "#32be44"),
            ("meter yellow", t.meter_yellow, "#f8d200"),
            ("meter red", t.meter_red, "#e0342c"),
        ] {
            assert_eq!(hex(color), want, "{name}");
        }
        assert_eq!([studio::LOW, studio::MID, studio::HIGH].map(hex), ["#e5484d", "#46c46a", "#4f8ff7"]);
        // The selection wash is the accent, translucent.
        let wash = t.selection.to_srgba_unmultiplied();
        assert!(wash[0] > wash[2] && wash[1] > wash[2] && wash[3] < 128, "{wash:?}");
    }

    #[test]
    fn soundcraft_classic_is_soundcraft_s_dark_look_unchanged() {
        assert_eq!(ThemeMode::parse("classic"), Some(ThemeMode::Classic));
        assert_eq!(ThemeMode::parse("Classic"), Some(ThemeMode::Classic));
        assert_eq!(ThemeMode::Classic.label(), "SoundCraft Classic");
        let ui: UiState = serde_json::from_value(json!({"theme": "classic"})).unwrap_or_default();
        assert_eq!(ui.theme, ThemeMode::Classic);
        // Every colour of SoundCraft's dark palette, byte for byte, as of the rebrand.
        assert_eq!(soundcraft_fingerprint(&Tokens::CLASSIC), 0xddcb_58b6_7158_84c5);
        assert_ne!(soundcraft_fingerprint(&Tokens::DARK), soundcraft_fingerprint(&Tokens::CLASSIC));
        // The colours routed through new tokens keep the values SoundCraft drew them in.
        let c = Tokens::CLASSIC;
        assert_eq!(c.accent_text, Color32::WHITE);
        assert_eq!(c.tab_on, c.accent_dark);
        assert_eq!(
            (c.transport_play, c.transport_play_icon, c.transport_rec),
            (Color32::from_rgb(40, 90, 50), Color32::from_rgb(120, 255, 140), Color32::from_rgb(110, 30, 30))
        );
        assert_eq!(c.readout_ink, c.counter_text);
        assert_eq!(c.marquee, Color32::from_rgb(120, 170, 255));
        assert_eq!((c.roll_rows, c.roll_lines, c.roll_lane, c.step_cells), (ROLL_ROWS, ROLL_LINES, Color32::from_rgb(28, 28, 30), STEP_CELLS));
        assert_eq!(
            (c.selected_text, c.marker_default, c.level_well),
            (Color32::WHITE, Color32::from_rgb(232, 200, 64), [Color32::from_rgb(36, 38, 8), Color32::from_rgb(51, 49, 11)])
        );
        // Its visuals are SoundCraft's too: egui's dark ones with the accent as selection.
        let _palette = lock_test_palette();
        let _restore = RestorePalette(palette());
        let ctx = egui::Context::default();
        apply_palette(&ctx, Palette::Classic);
        assert_eq!(palette(), Palette::Classic);
        assert_eq!(Tokens::current().panel_bg, c.panel_bg);
        let v = ctx.global_style().visuals.clone();
        let mut want = egui::Visuals::dark();
        want.selection.bg_fill = c.accent;
        assert_eq!(
            (v.selection, v.widgets.active.bg_stroke, v.hyperlink_color),
            (want.selection, want.widgets.active.bg_stroke, want.hyperlink_color)
        );
        assert_eq!((v.window_stroke, v.code_bg_color), (want.window_stroke, want.code_bg_color));
        assert_eq!(
            (v.widgets.noninteractive.bg_stroke, v.widgets.open, v.widgets.active.bg_fill),
            (want.widgets.noninteractive.bg_stroke, want.widgets.open, want.widgets.active.bg_fill)
        );
    }

    /// Puts the process-wide palette back when a test that changed it ends, even by panicking.
    struct RestorePalette(Palette);

    impl Drop for RestorePalette {
        fn drop(&mut self) {
            ACTIVE_PALETTE.store(self.0.index(), Ordering::Relaxed);
        }
    }

    #[test]
    fn waveforms_are_coloured_by_frequency_in_djforge_dark_unless_the_prefs_say_otherwise() {
        let _palette = lock_test_palette();
        let _restore = RestorePalette(palette());
        assert_eq!(Palette::ALL.map(Palette::colours_waveforms), [true, false, false]);
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        assert_eq!(app.ui.waveform_bands, None);
        let ctx = egui::Context::default();
        for (p, on) in [(Palette::Dark, true), (Palette::Light, false), (Palette::Classic, false)] {
            apply_palette(&ctx, p);
            assert_eq!(app.waveform_bands(), on, "{p:?}");
        }
        assert_eq!(app.run("ui.waveform_bands", json!({"value": true})), Ok(json!({"value": true, "follows_theme": false})));
        assert!(app.waveform_bands(), "the choice beats Classic's default");
        assert!(app.run("ui.waveform_bands", json!({"value": "yes"})).is_err());
        // Saved with the UI prefs, and an old prefs file follows the theme.
        let back: UiState = serde_json::from_value(serde_json::to_value(&app.ui).unwrap_or_default()).unwrap_or_default();
        assert_eq!(back.waveform_bands, Some(true));
        let old: UiState = serde_json::from_value(json!({"theme": "classic"})).unwrap_or_default();
        assert_eq!(old.waveform_bands, None);
        apply_palette(&ctx, Palette::Dark);
        assert_eq!(app.run("ui.waveform_bands", json!({"value": false})), Ok(json!({"value": false, "follows_theme": false})));
        assert!(!app.waveform_bands());
        assert_eq!(app.run("ui.waveform_bands", json!({"value": null})), Ok(json!({"value": true, "follows_theme": true})));
        assert_eq!(app.run("ui.waveform_bands", json!({})), Ok(json!({"value": true, "follows_theme": true})));
    }

    /// CIE76 colour difference (ΔE*ab, D65) of two opaque colours.
    fn delta_e(a: Color32, b: Color32) -> f64 {
        let lab = |c: Color32| {
            let linear = |channel: u8| {
                let s = f64::from(channel) / 255.0;
                if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
            };
            let (r, g, b) = (linear(c.r()), linear(c.g()), linear(c.b()));
            let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
            let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
            let f = |t: f64| if t > 0.008856 { t.cbrt() } else { 7.787 * t + 16.0 / 116.0 };
            [116.0 * f(y) - 16.0, 500.0 * (f(x) - f(y)), 200.0 * (f(y) - f(z))]
        };
        let (a, b) = (lab(a), lab(b));
        a.iter().zip(&b).map(|(p, q)| (p - q).powi(2)).sum::<f64>().sqrt()
    }

    #[test]
    fn deck_tints_read_on_the_background_and_stay_clear_of_the_band_and_state_colours() {
        use studio::*;
        for deck in [DECK_A, DECK_B] {
            assert!(contrast(deck, BG) >= 4.5, "{deck:?} on the background: {:.2}", contrast(deck, BG));
            assert!(contrast(deck, Tokens::DARK.panel_bg2) >= 3.0, "{deck:?}");
            for (name, other) in [("low", LOW), ("mid", MID), ("high", HIGH), ("in", IN), ("out", OUT), ("accent", ACCENT)] {
                assert!(delta_e(deck, other) >= 50.0, "{deck:?} too near {name}: ΔE {:.1}", delta_e(deck, other));
            }
        }
        assert!(delta_e(DECK_A, DECK_B) >= 60.0, "the decks tell apart");
        // An orange Deck B would read as the low band or record red.
        let orange = Color32::from_rgb(0xe0, 0x66, 0x3a);
        assert!(delta_e(orange, LOW) < 40.0 && delta_e(orange, OUT) < 40.0);
    }

    #[test]
    fn transition_markers_read_on_every_palettes_rulers_and_lanes() {
        for p in Palette::ALL {
            let t = Tokens::of(p);
            for (kind, c) in ["mix-in", "hit", "mix-out"].into_iter().zip(t.dj_marks) {
                for (on, bg) in [("ruler", t.ruler_bg), ("lane", t.playlist_bg), ("flag edge", t.dj_mark_edge)] {
                    assert!(contrast(c, bg) >= 3.0, "{p:?} {kind} on the {on}: {:.2}", contrast(c, bg));
                }
            }
        }
        assert_eq!(Tokens::DARK.dj_marks, [studio::IN, studio::ACCENT, studio::OUT], "djforge Dark keeps the studio colours");
        assert_ne!(Tokens::LIGHT.dj_marks, Tokens::DARK.dj_marks);
    }

    #[test]
    fn deck_colours_are_on_in_djforge_dark_unless_the_prefs_say_otherwise() {
        let _palette = lock_test_palette();
        let _restore = RestorePalette(palette());
        assert_eq!(Palette::ALL.map(Palette::tints_decks), [true, false, false]);
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        assert_eq!(app.ui.deck_colours, None);
        let ctx = egui::Context::default();
        for (p, on) in [(Palette::Dark, true), (Palette::Light, false), (Palette::Classic, false)] {
            apply_palette(&ctx, p);
            assert_eq!(app.deck_colours(), on, "{p:?}");
        }
        assert_eq!(app.run("ui.deck_colours", json!({"value": true})), Ok(json!({"value": true, "follows_theme": false})));
        assert!(app.deck_colours(), "the choice beats Classic's default");
        assert!(app.run("ui.deck_colours", json!({"value": 1})).is_err());
        // Saved with the UI prefs; an old prefs file follows the theme.
        let back: UiState = serde_json::from_value(serde_json::to_value(&app.ui).unwrap_or_default()).unwrap_or_default();
        assert_eq!(back.deck_colours, Some(true));
        let old: UiState = serde_json::from_value(json!({"theme": "classic", "waveform_bands": true})).unwrap_or_default();
        assert_eq!(old.deck_colours, None);
        apply_palette(&ctx, Palette::Dark);
        assert_eq!(app.run("ui.deck_colours", json!({"value": false})), Ok(json!({"value": false, "follows_theme": false})));
        assert!(!app.deck_colours());
        assert_eq!(app.run("ui.deck_colours", json!({"value": null})), Ok(json!({"value": true, "follows_theme": true})));
        assert_eq!(app.run("ui.deck_colours", json!({})), Ok(json!({"value": true, "follows_theme": true})));
    }

    #[test]
    fn prefs_saved_as_dark_get_djforge_dark() {
        let ui: UiState = serde_json::from_value(json!({"theme": "dark"})).unwrap_or_default();
        assert_eq!(ui.theme, ThemeMode::Dark);
        assert_eq!(ThemeMode::Dark.label(), "djforge Dark");
        let ctx = egui::Context::default();
        assert_eq!(resolve(&ctx, ThemeMode::Dark), Palette::Dark);
        assert_eq!(resolve(&ctx, ThemeMode::Classic), Palette::Classic);
        // System with a dark (or unknown) OS appearance is djforge Dark, never Classic.
        assert_eq!(resolve(&ctx, ThemeMode::System), Palette::Dark);
        assert_eq!(Tokens::of(resolve(&ctx, ThemeMode::Dark)).panel_bg, Tokens::DARK.panel_bg);
    }

    #[test]
    fn djforge_dark_focus_and_selection_are_visible() {
        let _palette = lock_test_palette();
        let _restore = RestorePalette(palette());
        let ctx = egui::Context::default();
        apply_palette(&ctx, Palette::Dark);
        let t = Tokens::DARK;
        let v = ctx.global_style().visuals.clone();
        // A focused text field is outlined in selection.stroke; a focused button in the active state.
        for (what, stroke) in [("focused text field", v.selection.stroke), ("focused button", v.widgets.active.bg_stroke)] {
            assert!(stroke.width >= 1.0, "{what}");
            for bg in [t.window_bg, t.panel_bg, t.panel_bg2, t.slot_bg, t.button] {
                assert!(contrast(stroke.color, bg) >= 3.0, "{what} outline on {bg:?}: {:.2}", contrast(stroke.color, bg));
            }
        }
        // Selected text and selected rows are drawn in selection.stroke on selection.bg_fill.
        assert!(contrast(v.selection.stroke.color, v.selection.bg_fill) >= 4.5);
        assert!(contrast(t.text, v.selection.bg_fill) >= 4.5);
        // One selection colour, everywhere something is selected.
        assert_eq!([t.accent_dark, t.row_selected, t.header_selected, t.strip_selected], [studio::SELECTED; 4]);
        // egui's own greys are the palette's.
        assert_eq!(
            (v.window_stroke.color, v.widgets.noninteractive.bg_stroke.color, v.widgets.open.bg_fill),
            (t.border_light, t.border_light, t.button_hi)
        );
    }

    #[test]
    fn an_unticked_checkbox_reads_on_every_palettes_panels() {
        use egui::widget_style::{Classes, WidgetState};
        let _palette = lock_test_palette();
        let _restore = RestorePalette(palette());
        let ctx = egui::Context::default();
        for p in Palette::ALL {
            apply_palette(&ctx, p);
            let t = Tokens::of(p);
            let style = ctx.global_style();
            // The box egui draws for an idle checkbox (a radio button and a text field use the same stroke).
            let frame = style.checkbox_style(&Classes::default(), WidgetState::Inactive).checkbox_frame;
            assert!(frame.stroke.width >= 1.0, "{p:?} checkbox outline width");
            assert_eq!(style.visuals.widgets.inactive.bg_stroke, frame.stroke, "{p:?}");
            // On a docked panel, and inside a floating window.
            for (what, bg) in [("panel", t.panel_bg), ("window", style.visuals.window_fill)] {
                let c = contrast(frame.stroke.color, bg);
                assert!(c >= 3.0, "{p:?} checkbox outline on the {what}: {c:.2}");
            }
        }
    }

    #[test]
    fn text_is_readable_in_every_palette() {
        let mut failures = Vec::new();
        for palette in Palette::ALL {
            let t = Tokens::of(palette);
            let surfaces = [
                ("window", t.window_bg),
                ("panel", t.panel_bg),
                ("panel2", t.panel_bg2),
                ("toolbar", t.toolbar_bg),
                ("strip", t.strip_bg),
                ("menu bar", t.menu_bar),
                ("lane", t.playlist_bg),
                ("button", t.button),
            ];
            let mut pairs = vec![];
            for (name, bg) in surfaces {
                pairs.push((format!("text on {name}"), t.text, bg, 4.5));
                pairs.push((format!("dim text on {name}"), t.text_dim, bg, 3.0));
            }
            for (name, bg) in [
                ("hovered button", t.button_hi),
                ("selected row", t.row_selected),
                ("selected header", t.header_selected),
                ("selected strip", t.strip_selected),
            ] {
                pairs.push((format!("text on {name}"), t.text, bg, 4.5));
            }
            pairs.extend([
                ("header text".into(), t.header_text, t.panel_bg2, 4.5),
                ("header text, selected".into(), t.header_text, t.header_selected, 4.5),
                ("ruler text".into(), t.ruler_text, t.ruler_bg, 4.5),
                ("counter".into(), t.counter_text, t.counter_bg, 4.5),
                ("counter label".into(), t.counter_label, t.counter_bg, 4.5),
                ("mode on".into(), t.mode_on_text, t.mode_on, 4.5),
                ("mode off".into(), t.mode_off_text, t.mode_off, 4.5),
                ("dark text on the accent".into(), t.text_dark, t.mode_on, 3.0),
                ("open tab".into(), t.accent_text, t.tab_on, 4.5),
                ("section label".into(), t.section_label, t.strip_bg, 4.5),
            ]);
            // A bold label on an accent button. SoundCraft Classic keeps its 3.95:1 white on blue
            // (bold, so WCAG's large-text 3:1), as its look is preserved byte for byte.
            // Rulers with text on them.
            let [graph_bg, graph_line, graph_label] = t.tempo_graph;
            let [key_bg, key_default, key_line, key_text] = t.key_ruler;
            pairs.extend([
                ("tempo graph line".into(), graph_line, graph_bg, 3.0),
                ("tempo graph labels".into(), graph_label, graph_bg, 4.5),
                ("key ruler default".into(), key_default, key_bg, 4.5),
                ("key ruler change line".into(), key_line, key_bg, 3.0),
                ("key ruler change".into(), key_text, key_bg, 4.5),
                ("marker on the chords ruler".into(), t.marker_default, t.chords_ruler, 3.0),
                ("marker text on the marker ruler".into(), t.marker_text, t.marker_ruler, 4.5),
                ("clip-gain readout".into(), t.counter_text, t.popup_bg, 4.5),
            ]);
            // Selected rows (menus, lists, egui's selection): selected text, body and dim text.
            pairs.push(("dim text on selected row".into(), t.text_dim, t.row_selected, 3.0));
            if palette == Palette::Dark {
                // Only djforge Dark selects on accent_dark; the others select on the accent.
                pairs.push(("dim text on menu selection".into(), t.text_dim, t.accent_dark, 3.0));
                pairs.push(("selected text".into(), t.selected_text, t.accent_dark, 4.5));
            }
            pairs.push(("accent button label".into(), t.accent_text, t.accent, if palette == Palette::Classic { 3.0 } else { 4.5 }));
            for (what, fg, bg, min) in pairs {
                let ratio = contrast(fg, bg);
                if ratio < min {
                    failures.push(format!("{palette:?}: {what} is {ratio:.2}:1, below {min}:1"));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn the_theme_menu_lists_every_mode() {
        assert_eq!(ThemeMode::ALL.map(ThemeMode::label), ["djforge Dark", "Light", "System", "SoundCraft Classic"]);
        assert_eq!(ThemeMode::ALL.map(ThemeMode::id), ["dark", "light", "system", "classic"]);
    }

    #[test]
    fn dark_is_the_default() {
        assert_eq!(ThemeMode::default(), ThemeMode::Dark);
        assert_eq!(UiState::default().theme, ThemeMode::Dark);
        // Prefs saved before the setting existed load as Dark.
        let old: UiState = serde_json::from_value(json!({"show_tracks_list": true})).unwrap_or_default();
        assert_eq!(old.theme, ThemeMode::Dark);
    }

    #[test]
    fn prefs_saved_before_track_tint_get_the_default() {
        let ui: UiState = serde_json::from_value(json!({"theme": "light"})).unwrap();
        assert!((ui.track_tint - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn modes_round_trip_through_prefs() {
        for mode in ThemeMode::ALL {
            assert_eq!(ThemeMode::parse(mode.id()), Some(mode));
            let ui = UiState { theme: mode, ..UiState::default() };
            let back: UiState = serde_json::from_value(serde_json::to_value(&ui).unwrap_or_default()).unwrap_or_default();
            assert_eq!(back.theme, mode);
        }
        assert_eq!(ThemeMode::parse(" Light "), Some(ThemeMode::Light));
        assert_eq!(ThemeMode::parse("sepia"), None);
    }

    #[test]
    fn theme_command_sets_and_reports_the_mode() {
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        assert_eq!(app.run("ui.theme", json!({})), Ok(json!({"mode": "dark"})));
        assert_eq!(app.run("ui.theme", json!({"mode": "light"})), Ok(json!({"mode": "light"})));
        assert_eq!(app.ui.theme, ThemeMode::Light);
        assert!(app.run("ui.theme", json!({"mode": "sepia"})).is_err());
        assert_eq!(app.ui.theme, ThemeMode::Light);
        assert_eq!(app.run("ui.theme", json!({"mode": "system"})), Ok(json!({"mode": "system"})));
        assert_eq!(app.run("ui.theme", json!({"mode": "classic"})), Ok(json!({"mode": "classic"})));
        assert_eq!(app.ui.theme, ThemeMode::Classic);
        assert!(crate::menus::UI_COMMANDS.iter().any(|c| c.0 == "ui.theme"));
    }

    #[test]
    fn application_frames_follow_system_appearance_and_preserve_manual_palettes() {
        // Mixer rendering tests also change the process-wide custom-painted palette.
        let _palette = lock_test_palette();
        fn frame(app: &mut SoundApp, ctx: &egui::Context, system_theme: Option<egui::Theme>, light: bool) {
            let mode = app.ui.theme;
            let colors = |app: &SoundApp| {
                app.engine
                    .session()
                    .tracks
                    .iter()
                    .map(|track| {
                        (track.color, track.playlists.iter().flat_map(|playlist| &playlist.clips).map(|clip| clip.color).collect::<Vec<_>>())
                    })
                    .collect::<Vec<_>>()
            };
            let before = colors(app);
            let mut output = ctx.run_ui(egui::RawInput { system_theme, ..Default::default() }, |ui| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                let tokens = if light { Tokens::LIGHT } else { Tokens::DARK };
                assert_eq!(is_light(), light, "custom palette for {mode:?} with {system_theme:?}");
                assert_eq!(Tokens::current().panel_bg, tokens.panel_bg);
                assert_eq!(ctx.options(|options| options.theme_preference), egui::ThemePreference::System);
                for theme in [egui::Theme::Light, egui::Theme::Dark] {
                    let style = ctx.style_of(theme);
                    assert_eq!(style.visuals.dark_mode, !light);
                    assert_eq!(style.visuals.panel_fill, tokens.panel_bg);
                    assert_eq!(style.visuals.widgets.inactive.bg_fill, tokens.button);
                    assert_eq!(style.visuals.widgets.inactive.fg_stroke.color, tokens.text);
                    assert_eq!(style.spacing.button_padding, egui::vec2(6.0, 2.0));
                }
                app.ui(ui);
                assert_eq!(ui.visuals().panel_fill, tokens.panel_bg);
                assert_eq!(ui.visuals().widgets.inactive.bg_fill, tokens.button);
            });
            output.textures_delta.clear();
            assert_eq!(app.ui.theme, mode);
            assert_eq!(serde_json::to_value(&app.ui).unwrap()["theme"], mode.id());
            assert_eq!(colors(app), before, "appearance must preserve track and clip colors");
        }

        let appearances = [Some(egui::Theme::Light), Some(egui::Theme::Dark), Some(egui::Theme::Light), None];
        for initial in [Some(egui::Theme::Light), Some(egui::Theme::Dark), None] {
            let ctx = egui::Context::default();
            let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
            app.ui = serde_json::from_value(json!({"theme": "system"})).unwrap();
            frame(&mut app, &ctx, initial, initial == Some(egui::Theme::Light));
        }

        let ctx = egui::Context::default();
        let mut engine = soundcraft_engine::demo::demo_engine();
        let track = engine.session_mut().tracks.first_mut().unwrap();
        track.color = [19, 83, 147];
        track.playlists.iter_mut().flat_map(|playlist| &mut playlist.clips).next().unwrap().color = Some([211, 57, 99]);
        let mut app = SoundApp::new(engine, None, Services::default());
        for mode in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
            app.run("ui.theme", json!({"mode": mode.id()})).unwrap();
            for appearance in appearances {
                let light = mode == ThemeMode::Light || (mode == ThemeMode::System && appearance == Some(egui::Theme::Light));
                frame(&mut app, &ctx, appearance, light);
            }
            if mode != ThemeMode::System {
                let latest = if mode == ThemeMode::Light { egui::Theme::Dark } else { egui::Theme::Light };
                frame(&mut app, &ctx, Some(latest), mode == ThemeMode::Light);
                app.run("ui.theme", json!({"mode": "system"})).unwrap();
                // Resolve the current context immediately, without another OS appearance event.
                app.logic(&ctx);
                assert_eq!(is_light(), latest == egui::Theme::Light);
                assert_eq!(ctx.global_style().visuals.dark_mode, latest != egui::Theme::Light);
                frame(&mut app, &ctx, Some(latest), latest == egui::Theme::Light);
            }
        }
    }
}
