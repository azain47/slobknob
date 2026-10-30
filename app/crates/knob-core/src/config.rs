use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::keys::KeyCombo;
use crate::modes::{mode_def, ModeDef};
use crate::types::{DeviceInfo, KnobKeys, MediaKey};

/// Key of the wheel used when no per-app wheel matches.
pub const DEFAULT_WHEEL_KEY: &str = "default";

/// What the knob press does to open the wheel. Independent of
/// `modifier_trigger`, which also opens it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    /// Double-tap opens the wheel; turn to highlight; picks after
    /// `auto_confirm_ms` or on tap.
    DoubleTap,
    /// Hold opens the wheel; turn while held and release, or release, turn, tap.
    Hold,
}

/// A wheel used while a matching app is frontmost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppWheel {
    /// Wheel key; also shown in the tray.
    pub name: String,
    /// App names to match, case-insensitive: the macOS app name
    /// ("DaVinci Resolve") or the Windows executable without ".exe" ("Resolve").
    pub matches: Vec<String>,
    pub modes: Vec<String>,
}

/// What a custom slice does when picked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ActionKind {
    /// Shortcut text, e.g. "cmd+shift+t" or "ctrl+k, ctrl+c" (see `keys.rs`).
    Keys { keys: String },
    /// A URL, a file or folder path, or an app name.
    Open { target: String },
    Text { text: String },
    Media { key: MediaKey },
    /// A sub-wheel with these slices (mode or action ids).
    Wheel { slices: Vec<String> },
}

/// A one-off action (or sub-wheel) that can be put on any wheel by its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomAction {
    pub id: String,
    pub name: String,
    /// Icon name from ui/icons.js.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Hex color for the slice highlight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(flatten)]
    pub kind: ActionKind,
}

impl CustomAction {
    fn problem(&self) -> Option<String> {
        if self.id.trim().is_empty() {
            return Some(format!("action \"{}\" has no id", self.name));
        }
        if mode_def(&self.id).is_some() {
            return Some(format!("action id \"{}\" is a built-in mode id", self.id));
        }
        match &self.kind {
            ActionKind::Keys { keys } => KeyCombo::parse_sequence(keys).err().map(|e| format!("action \"{}\": {e}", self.id)),
            ActionKind::Open { target } if target.trim().is_empty() => Some(format!("action \"{}\": nothing to open", self.id)),
            ActionKind::Text { text } if text.is_empty() => Some(format!("action \"{}\": no text", self.id)),
            _ => None,
        }
    }
}

/// A wheel entry: a built-in mode or a custom action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceDef<'a> {
    Mode(&'static ModeDef),
    Action(&'a CustomAction),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub trigger: Trigger,
    /// Hold Cmd (macOS) / Win (Windows) and turn to open the wheel; release
    /// the modifier to pick.
    pub modifier_trigger: bool,
    /// Max gap between two taps. Single taps wait this long before acting.
    pub double_tap_ms: u64,
    /// `trigger = hold`: hold this long to open the wheel.
    pub hold_ms: u64,
    /// Close an untouched wheel after this long.
    pub wheel_timeout_ms: u64,
    /// After a turn in the wheel, pick the highlight this long later. 0 = tap only.
    pub auto_confirm_ms: u64,
    /// Idle time before falling back to volume mode. 0 = never.
    pub revert_after_ms: u64,
    pub scroll_lines: i32,
    /// Scroll mode glides in pixels instead of jumping whole lines.
    pub smooth_scroll: bool,
    /// Smooth scroll distance per knob click, before acceleration.
    pub scroll_pixels: i32,
    /// Fast turning multiplies the scroll step up to this much. 1 = off.
    pub scroll_acceleration: f32,
    pub seek_seconds: i32,
    pub drag_pixels: i32,
    /// Apps mode: release the switcher modifier this long after the last turn.
    pub app_switch_commit_ms: u64,
    /// Drag mode: release the mouse button this long after the last turn.
    pub drag_release_ms: u64,
    pub dim_screen: bool,
    /// "pastel" (a soft color per mode), "system" (OS accent) or a hex color like "#3478f6".
    pub accent: String,
    /// Slices of the default wheel, clockwise from the top.
    pub wheel: Vec<String>,
    pub app_wheels: Vec<AppWheel>,
    /// Custom slices, referenced by id from `wheel`, `app_wheels` and sub-wheels.
    pub actions: Vec<CustomAction>,
    /// Which media keys the knob sends ("learn my knob" fills this in).
    pub knob_keys: KnobKeys,
    /// Swap clockwise and counter-clockwise for everything the knob does.
    pub invert_direction: bool,
    /// The keyboard the knob is on, if learned.
    pub knob_device: Option<DeviceInfo>,
    /// Only treat keys from `knob_device` as the knob, so volume keys on other
    /// keyboards keep working. Needs the platform's device listener (macOS:
    /// Input Monitoring permission).
    pub only_knob_device: bool,
    pub launch_at_login: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            trigger: Trigger::DoubleTap,
            modifier_trigger: true,
            double_tap_ms: 280,
            hold_ms: 350,
            wheel_timeout_ms: 5000,
            auto_confirm_ms: 600,
            revert_after_ms: 0,
            scroll_lines: 3,
            smooth_scroll: true,
            scroll_pixels: 40,
            scroll_acceleration: 4.0,
            seek_seconds: 5,
            drag_pixels: 4,
            app_switch_commit_ms: 800,
            drag_release_ms: 350,
            dim_screen: true,
            accent: "pastel".into(),
            wheel: ["volume", "brightness", "scroll", "zoom", "apps", "tabs", "spaces", "media", "seek"]
                .map(String::from)
                .to_vec(),
            app_wheels: vec![AppWheel {
                name: "DaVinci Resolve".into(),
                matches: vec!["DaVinci Resolve".into(), "Resolve".into()],
                modes: ["volume", "jog", "edits", "zoom", "drag", "scroll", "apps", "media"]
                    .map(String::from)
                    .to_vec(),
            }],
            actions: Vec::new(),
            knob_keys: KnobKeys::default(),
            invert_direction: false,
            knob_device: None,
            only_knob_device: false,
            launch_at_login: false,
        }
    }
}

impl Config {
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        Ok(serde_json::from_str::<Config>(text)?.sanitized())
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("config serializes")
    }

    /// Drops invalid actions and unknown slice ids, and replaces an empty
    /// default wheel, so the engine can rely on every wheel having at least one
    /// known slice. [`Config::problems`] says what was wrong.
    pub fn sanitized(mut self) -> Self {
        let mut seen = HashSet::new();
        self.actions.retain(|a| a.problem().is_none() && seen.insert(a.id.clone()));
        // Dropping an emptied sub-wheel can empty the one that contains it, so repeat.
        loop {
            let known: HashSet<String> = self.actions.iter().map(|a| a.id.clone()).collect();
            let keep = |id: &String| mode_def(id).is_some() || known.contains(id);
            self.wheel.retain(keep);
            for w in &mut self.app_wheels {
                w.modes.retain(keep);
            }
            for a in &mut self.actions {
                if let ActionKind::Wheel { slices } = &mut a.kind {
                    slices.retain(keep);
                }
            }
            let before = self.actions.len();
            self.actions.retain(|a| !matches!(&a.kind, ActionKind::Wheel { slices } if slices.is_empty()));
            if self.actions.len() == before {
                break;
            }
        }
        if self.wheel.is_empty() {
            self.wheel = Config::default().wheel;
        }
        self.app_wheels.retain(|w| !w.modes.is_empty() && !w.matches.is_empty());
        self.scroll_acceleration = self.scroll_acceleration.clamp(1.0, 10.0);
        self.scroll_pixels = self.scroll_pixels.clamp(1, 1000);
        self
    }

    /// Human-readable reasons [`Config::sanitized`] would drop something.
    pub fn problems(&self) -> Vec<String> {
        let mut out: Vec<String> = self.actions.iter().filter_map(CustomAction::problem).collect();
        let mut seen = HashSet::new();
        for a in &self.actions {
            if !seen.insert(&a.id) {
                out.push(format!("action id \"{}\" is used twice", a.id));
            }
        }
        let lists = std::iter::once(("wheel".to_string(), &self.wheel))
            .chain(self.app_wheels.iter().map(|w| (format!("wheel \"{}\"", w.name), &w.modes)))
            .chain(self.actions.iter().filter_map(|a| match &a.kind {
                ActionKind::Wheel { slices } => Some((format!("sub-wheel \"{}\"", a.id), slices)),
                _ => None,
            }));
        for (name, list) in lists {
            for id in list {
                if self.slice_def(id).is_none() {
                    out.push(format!("{name}: unknown slice \"{id}\""));
                }
            }
        }
        for w in &self.app_wheels {
            if w.matches.iter().all(|m| m.trim().is_empty()) {
                out.push(format!("wheel \"{}\" has no app names, so it is never used", w.name));
            }
            if w.modes.is_empty() {
                out.push(format!("wheel \"{}\" has no slices, so it is never used", w.name));
            }
        }
        out
    }

    pub fn action(&self, id: &str) -> Option<&CustomAction> {
        self.actions.iter().find(|a| a.id == id)
    }

    pub fn slice_def(&self, id: &str) -> Option<SliceDef<'_>> {
        mode_def(id).map(SliceDef::Mode).or_else(|| self.action(id).map(SliceDef::Action))
    }

    /// Wheel key for the frontmost app.
    pub fn wheel_key_for(&self, front_app: Option<&str>) -> &str {
        if let Some(app) = front_app {
            for w in &self.app_wheels {
                if w.matches.iter().any(|m| m.eq_ignore_ascii_case(app)) {
                    return &w.name;
                }
            }
        }
        DEFAULT_WHEEL_KEY
    }

    pub fn wheel_list(&self, key: &str) -> &[String] {
        self.app_wheels
            .iter()
            .find(|w| w.name == key)
            .map(|w| w.modes.as_slice())
            .unwrap_or(&self.wheel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_json_fills_defaults_and_drops_unknown_modes() {
        let cfg = Config::from_json(r#"{ "auto_confirm_ms": 0, "wheel": ["scroll", "nope"] }"#).unwrap();
        assert_eq!(cfg.auto_confirm_ms, 0);
        assert_eq!(cfg.wheel, vec!["scroll"]);
        assert_eq!(cfg.double_tap_ms, 280);
    }

    #[test]
    fn empty_wheel_falls_back_to_default() {
        let cfg = Config::from_json(r#"{ "wheel": ["nope"] }"#).unwrap();
        assert_eq!(cfg.wheel, Config::default().wheel);
    }

    #[test]
    fn app_wheel_matches_case_insensitively() {
        let cfg = Config::default();
        assert_eq!(cfg.wheel_key_for(Some("resolve")), "DaVinci Resolve");
        assert_eq!(cfg.wheel_key_for(Some("DaVinci Resolve")), "DaVinci Resolve");
        assert_eq!(cfg.wheel_key_for(Some("Safari")), DEFAULT_WHEEL_KEY);
        assert_eq!(cfg.wheel_key_for(None), DEFAULT_WHEEL_KEY);
    }

    #[test]
    fn actions_load_and_bad_ones_are_dropped_with_a_reason() {
        let cfg = Config::from_json(
            r#"{
              "wheel": ["volume", "term", "bad", "dev", "ghost"],
              "actions": [
                { "id": "term", "name": "Terminal", "icon": "terminal", "type": "open", "target": "Ghostty" },
                { "id": "bad", "name": "Bad", "type": "keys", "keys": "hyper+x" },
                { "id": "dev", "name": "Dev", "type": "wheel", "slices": ["term", "scroll", "nope"] },
                { "id": "sig", "name": "Sig", "type": "text", "text": "Thanks!" },
                { "id": "pp", "name": "Play", "type": "media", "key": "play_pause" }
              ]
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.wheel, vec!["volume", "term", "dev"]);
        assert_eq!(cfg.action("dev").unwrap().kind, ActionKind::Wheel { slices: vec!["term".into(), "scroll".into()] });
        assert_eq!(cfg.action("pp").unwrap().kind, ActionKind::Media { key: MediaKey::PlayPause });
        assert!(cfg.action("bad").is_none());
        assert!(matches!(cfg.slice_def("term"), Some(SliceDef::Action(a)) if a.icon.as_deref() == Some("terminal")));
        assert!(matches!(cfg.slice_def("scroll"), Some(SliceDef::Mode(_))));

        let raw: Config = serde_json::from_str(r#"{ "wheel": ["ghost"], "actions": [{ "id": "volume", "name": "x", "type": "text", "text": "y" }] }"#).unwrap();
        let problems = raw.problems();
        assert!(problems.iter().any(|p| p.contains("built-in mode id")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("unknown slice \"ghost\"")), "{problems:?}");

        let raw: Config = serde_json::from_str(r#"{ "app_wheels": [{ "name": "X", "matches": [], "modes": ["zoom"] }] }"#).unwrap();
        assert!(raw.problems().iter().any(|p| p.contains("no app names")));
    }

    #[test]
    fn emptied_sub_wheel_is_removed_everywhere() {
        let cfg = Config::from_json(
            r#"{ "wheel": ["sub", "zoom"], "actions": [{ "id": "sub", "name": "Sub", "type": "wheel", "slices": ["nope"] }] }"#,
        )
        .unwrap();
        assert_eq!(cfg.wheel, vec!["zoom"]);
        assert!(cfg.actions.is_empty());
    }

    #[test]
    fn knob_keys_use_neutral_names() {
        let cfg = Config::from_json(r#"{ "knob_keys": { "clockwise": "next", "counter_clockwise": "previous", "press": "play_pause" } }"#).unwrap();
        assert_eq!(cfg.knob_keys.press, MediaKey::PlayPause);
        let json = Config::default().to_json();
        assert!(json.contains(r#""clockwise": "volume_up""#), "{json}");
    }

    #[test]
    fn round_trips_through_json() {
        let mut cfg = Config::default();
        cfg.actions.push(CustomAction {
            id: "tab".into(),
            name: "Reopen tab".into(),
            icon: None,
            color: Some("#ffcc00".into()),
            kind: ActionKind::Keys { keys: "cmd+shift+t".into() },
        });
        cfg.wheel.push("tab".into());
        cfg.knob_device = Some(DeviceInfo { vendor_id: 1, product_id: 2, name: "Knob".into() });
        assert_eq!(Config::from_json(&cfg.to_json()).unwrap(), cfg);
    }
}
