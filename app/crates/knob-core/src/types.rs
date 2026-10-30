use serde::{Deserialize, Serialize};

use crate::keys::KeyCombo;

/// One of the three things a knob can send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnobKey {
    /// Volume up.
    Clockwise,
    /// Volume down.
    CounterClockwise,
    /// Mute.
    Press,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnobEvent {
    pub key: KnobKey,
    pub down: bool,
    pub repeat: bool,
    /// The wheel modifier (Cmd on macOS, Win on Windows) is physically held.
    pub modifier: bool,
}

impl KnobEvent {
    pub fn down(key: KnobKey) -> Self {
        Self { key, down: true, repeat: false, modifier: false }
    }

    pub fn up(key: KnobKey) -> Self {
        Self { key, down: false, repeat: false, modifier: false }
    }

    pub fn with_modifier(self, modifier: bool) -> Self {
        Self { modifier, ..self }
    }
}

/// A keyboard (or other HID device), as reported by the platform's device
/// listener. Matched on vendor and product id; the name is for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub vendor_id: u32,
    pub product_id: u32,
    #[serde(default)]
    pub name: String,
}

impl DeviceInfo {
    pub fn same_device(&self, other: &DeviceInfo) -> bool {
        self.vendor_id == other.vendor_id && self.product_id == other.product_id
    }
}

/// A raw media key from the platform hook, before the engine decides whether
/// it is the knob (see `Config::knob_keys` and `Config::knob_device`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEvent {
    pub key: MediaKey,
    pub down: bool,
    pub repeat: bool,
    /// The wheel modifier (Cmd on macOS, Win on Windows) is physically held.
    pub modifier: bool,
    /// Device that sent the key, when the platform's device listener is on
    /// and saw the matching HID report. None otherwise.
    pub device: Option<DeviceInfo>,
}

impl InputEvent {
    pub fn new(key: MediaKey, down: bool) -> Self {
        Self { key, down, repeat: false, modifier: false, device: None }
    }
}

/// How the wheel was opened; decides how it picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenVia {
    /// Double-tap: picks `auto_confirm_ms` after the last turn, or on tap.
    Tap,
    /// Held knob: release after turning picks, or release, turn, tap.
    Hold,
    /// Modifier + turn: releasing the modifier picks.
    Modifier,
}

/// What the hook should do with the original key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Drop it; the OS never sees it.
    Swallow,
    /// Let the OS handle it as usual.
    Pass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    Vertical,
    Horizontal,
}

impl Axis {
    pub fn toggled(self) -> Self {
        match self {
            Axis::Vertical => Axis::Horizontal,
            Axis::Horizontal => Axis::Vertical,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Axis::Vertical => "vertical",
            Axis::Horizontal => "horizontal",
        }
    }
}

/// System media keys. The hook reports the ones it sees as [`InputEvent`]s;
/// [`Action::Media`] posts them as synthetic events, which platforms must mark
/// so their own hook lets them through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKey {
    VolumeUp,
    VolumeDown,
    Mute,
    PlayPause,
    Next,
    Previous,
    BrightnessUp,
    BrightnessDown,
}

/// Something to do on the OS. These are semantic; each platform picks the
/// right keystroke or API (e.g. zoom is Cmd+= on macOS and Ctrl+= on Windows).
/// `dir` / step values are +1 (clockwise / forward) or -1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Media(MediaKey),
    /// Positive lines scroll down (vertical) or right (horizontal).
    Scroll { axis: Axis, lines: i32 },
    /// Like [`Action::Scroll`], in pixels, eased over a few frames by the
    /// platform (on its own thread; `perform` must not block). Consecutive
    /// calls add to the remaining distance.
    SmoothScroll { axis: Axis, pixels: i32 },
    ZoomIn,
    ZoomOut,
    ZoomReset,
    /// Step the system app switcher, holding its modifier (Cmd / Alt) down
    /// until [`Action::AppSwitchEnd`].
    AppSwitchStep(i8),
    AppSwitchEnd,
    /// Cycle windows of the frontmost app.
    WindowCycle(i8),
    TabCycle(i8),
    DesktopStep(i8),
    /// Mission Control / Task View.
    DesktopOverview,
    /// Seek the playing media by this many seconds.
    Seek { seconds: i32 },
    /// Hold the left mouse button at the pointer and move it; ends with
    /// [`Action::DragEnd`], which also puts the pointer back.
    DragStep { axis: Axis, pixels: i32 },
    DragEnd,
    /// Left / right arrow.
    FrameStep(i8),
    /// Up / down arrow (previous / next edit point in editors like Resolve).
    EditPoint(i8),
    /// The space bar.
    SpaceKey,
    /// -1 undo, +1 redo.
    UndoRedo(i8),
    /// Press these shortcuts one after another (e.g. Cmd+K, then Cmd+C).
    Shortcut(Vec<KeyCombo>),
    /// Open a URL ("https://..."), a file or folder path, or an app by name.
    Open(String),
    /// Type this text into the frontmost app.
    TypeText(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SliceKind {
    /// Picking it switches what the knob does.
    Mode,
    /// Picking it runs a one-off action; the mode stays.
    Action,
    /// Picking it opens another wheel.
    Wheel,
}

/// A slice of the wheel as the UI needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Slice {
    pub id: String,
    pub name: String,
    pub hint: String,
    pub kind: SliceKind,
    /// Icon name (see ui/icons.js). None: the UI picks one from `id`.
    pub icon: Option<String>,
    /// Hex color. None: the theme's color for `id`.
    pub color: Option<String>,
}

/// Messages for the overlay UI. Serialized with a `type` tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum UiEvent {
    Open {
        /// Wheel key, or the sub-wheel's name.
        wheel: String,
        /// True for a sub-wheel opened from another wheel.
        nested: bool,
        slices: Vec<Slice>,
        selected: usize,
        /// Index of the mode currently in use, if it is on this wheel.
        active: Option<usize>,
        via: OpenVia,
        /// 0 when this wheel does not auto-confirm.
        auto_confirm_ms: u64,
    },
    Select {
        selected: usize,
    },
    Close,
    /// Short confirmation pill, e.g. after picking a mode.
    Hud {
        /// Mode or action id (picks the icon and color).
        mode: String,
        text: String,
        icon: Option<String>,
        color: Option<String>,
    },
}

/// Progress of "learn my knob".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LearnEvent {
    /// Waiting for this knob gesture.
    Prompt { step: KnobKey },
    /// Learned all three keys; `device` is set if the device listener saw it.
    Done { keys: KnobKeys, device: Option<DeviceInfo> },
    /// Timed out or cancelled; nothing changed.
    Cancelled,
}

/// Which media key the knob sends for each gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnobKeys {
    pub clockwise: MediaKey,
    pub counter_clockwise: MediaKey,
    pub press: MediaKey,
}

impl Default for KnobKeys {
    fn default() -> Self {
        Self { clockwise: MediaKey::VolumeUp, counter_clockwise: MediaKey::VolumeDown, press: MediaKey::Mute }
    }
}

impl KnobKeys {
    pub fn knob_key(&self, key: MediaKey) -> Option<KnobKey> {
        if key == self.clockwise {
            Some(KnobKey::Clockwise)
        } else if key == self.counter_clockwise {
            Some(KnobKey::CounterClockwise)
        } else if key == self.press {
            Some(KnobKey::Press)
        } else {
            None
        }
    }

    pub fn media_key(&self, key: KnobKey) -> MediaKey {
        match key {
            KnobKey::Clockwise => self.clockwise,
            KnobKey::CounterClockwise => self.counter_clockwise,
            KnobKey::Press => self.press,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Perform(Action),
    Ui(UiEvent),
    /// The mode of a wheel changed; persist it.
    ModeChanged { wheel: String, mode: String },
    Learn(LearnEvent),
    /// The knob moved while the preview is on (settings window in front);
    /// nothing else happened.
    Preview { key: KnobKey, down: bool },
}
