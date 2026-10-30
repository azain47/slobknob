/// Static description of a knob mode. What a mode *does* lives in the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeDef {
    pub id: &'static str,
    pub name: &'static str,
    /// Shown in the wheel hub: what turning and tapping do.
    pub hint: &'static str,
}

pub const MODES: &[ModeDef] = &[
    ModeDef { id: "volume", name: "Volume", hint: "turn: volume\ntap: mute" },
    ModeDef { id: "brightness", name: "Brightness", hint: "turn: screen brightness" },
    ModeDef { id: "scroll", name: "Scroll", hint: "turn: scroll\ntap: switch axis" },
    ModeDef { id: "zoom", name: "Zoom", hint: "turn: zoom in / out\ntap: reset" },
    ModeDef { id: "apps", name: "Apps", hint: "turn: switch app\ntap: pick" },
    ModeDef { id: "windows", name: "Windows", hint: "turn: cycle app windows" },
    ModeDef { id: "tabs", name: "Tabs", hint: "turn: switch tab" },
    ModeDef { id: "spaces", name: "Desktops", hint: "turn: switch desktop\ntap: overview" },
    ModeDef { id: "media", name: "Media", hint: "turn: skip track\ntap: play / pause" },
    ModeDef { id: "seek", name: "Seek", hint: "turn: seek\ntap: play / pause" },
    ModeDef { id: "drag", name: "Drag", hint: "turn: drag value under mouse\ntap: switch axis" },
    ModeDef { id: "jog", name: "Jog", hint: "turn: step frames\ntap: play / pause" },
    ModeDef { id: "edits", name: "Edits", hint: "turn: jump edit points\ntap: play / pause" },
    ModeDef { id: "undo", name: "Undo", hint: "turn: undo / redo" },
];

pub fn mode_def(id: &str) -> Option<&'static ModeDef> {
    MODES.iter().find(|m| m.id == id)
}
