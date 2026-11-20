//! Keyboard lights (see `knob_platform::set_keyboard_lights`) in the colours
//! the wheel gives each mode.

use std::sync::Mutex;

use knob_core::{Config, Slice, UiEvent};
use knob_platform::KeyboardLights;

/// The "pastel" accent: a soft colour per mode. Same table as `PASTEL` and
/// `PASTEL_FALLBACK` in ui/icons.js.
const PASTEL: [(&str, &str); 14] = [
    ("volume", "#f5adb9"),
    ("brightness", "#f5e3ad"),
    ("scroll", "#adf5dd"),
    ("zoom", "#b9adf5"),
    ("apps", "#f5cbad"),
    ("tabs", "#ade9f5"),
    ("spaces", "#ddadf5"),
    ("media", "#f5addd"),
    ("seek", "#c5f5ad"),
    ("windows", "#adcbf5"),
    ("drag", "#adf5b9"),
    ("jog", "#f5b7ad"),
    ("edits", "#f5adf5"),
    ("undo", "#f3f5ad"),
];
const PASTEL_FALLBACK: &str = "#e2d6f5";

/// Colours of the open wheel's slices, so the glow can follow the highlight.
static WHEEL: Mutex<Vec<[u8; 3]>> = Mutex::new(Vec::new());

/// Turns the lights on with the current colours, or off.
pub fn apply(cfg: &Config) {
    let lights = cfg.keyboard_lights.then(|| KeyboardLights {
        volume: mode_color(cfg, "volume"),
        brightness: mode_color(cfg, "brightness"),
    });
    knob_platform::set_keyboard_lights(lights);
}

/// While a wheel is open, the keyboard glows in the highlighted slice's colour.
pub fn on_ui(cfg: &Config, ui: &UiEvent) {
    if !cfg.keyboard_lights {
        return;
    }
    let mut wheel = WHEEL.lock().unwrap();
    let selected = match ui {
        UiEvent::Open { slices, selected, .. } => {
            *wheel = slices.iter().map(|s| slice_color(cfg, s)).collect();
            *selected
        }
        UiEvent::Select { selected } => *selected,
        UiEvent::Close => {
            wheel.clear();
            knob_platform::glow_keyboard(None);
            return;
        }
        UiEvent::Hud { .. } => return,
    };
    if let Some(&color) = wheel.get(selected) {
        knob_platform::glow_keyboard(Some(color));
    }
}

/// As the wheel draws it: the slice's own colour, else its mode's.
fn slice_color(cfg: &Config, slice: &Slice) -> [u8; 3] {
    slice.color.as_deref().and_then(hex).unwrap_or_else(|| mode_color(cfg, &slice.id))
}

/// The accent of `mode` on the wheel: the theme's colour (system accent or a
/// custom one), else the mode's pastel.
fn mode_color(cfg: &Config, mode: &str) -> [u8; 3] {
    hex(&crate::theme(cfg).accent).unwrap_or_else(|| {
        let pastel = PASTEL.iter().find(|(id, _)| *id == mode).map_or(PASTEL_FALLBACK, |(_, color)| color);
        hex(pastel).unwrap_or([255; 3])
    })
}

/// `#rrggbb`, as the UI accepts it.
fn hex(text: &str) -> Option<[u8; 3]> {
    let digits = text.strip_prefix('#').filter(|d| d.len() == 6 && d.bytes().all(|b| b.is_ascii_hexdigit()))?;
    let [_, r, g, b] = u32::from_str_radix(digits, 16).ok()?.to_be_bytes();
    Some([r, g, b])
}
