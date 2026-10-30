//! config.json (user-editable settings) and state.json (selected modes).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use knob_core::Config;
use tauri::{AppHandle, Manager};

pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
}

impl Paths {
    pub fn new(app: &AppHandle) -> Paths {
        let dir = app.path().app_config_dir().unwrap_or_else(|_| PathBuf::from("."));
        migrate_from_old_name(&dir);
        Paths { config: dir.join("config.json"), state: dir.join("state.json") }
    }
}

/// The app was called KnobWheel (identifier app.knobwheel); copy its settings
/// over once, the first time SlobKnob runs.
fn migrate_from_old_name(dir: &Path) {
    let Some(old) = dir.parent().map(|p| p.join("app.knobwheel")) else { return };
    if dir.join("config.json").exists() || !old.join("config.json").exists() {
        return;
    }
    let _ = fs::create_dir_all(dir);
    for name in ["config.json", "state.json"] {
        if let Err(e) = fs::copy(old.join(name), dir.join(name)) {
            log::warn!("copying {name} from {}: {e}", old.display());
        }
    }
    log::info!("copied settings from {}", old.display());
}

/// Reads config.json, writing the defaults first if it does not exist. A broken
/// file is reported and left alone so the user's edits are not lost.
pub fn load_config(path: &Path) -> Config {
    match fs::read_to_string(path) {
        Ok(text) => Config::from_json(&text).unwrap_or_else(|e| {
            log::error!("{}: {e}; using defaults", path.display());
            Config::default()
        }),
        Err(_) => {
            let cfg = Config::default();
            write(path, &cfg.to_json());
            cfg
        }
    }
}

/// The file as written, without [`Config::sanitized`], so the settings window
/// keeps half-finished actions instead of losing them on the next autosave.
/// Err: the file exists but is not valid JSON (message for the user).
pub fn load_raw(path: &Path) -> Result<Config, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<Config>(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(_) => Ok(Config::default()),
    }
}

pub fn save_config(path: &Path, cfg: &Config) -> Result<(), String> {
    write_checked(path, &cfg.to_json())
}

pub fn load_modes(path: &Path) -> HashMap<String, String> {
    fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save_modes(path: &Path, modes: &HashMap<String, String>) {
    write(path, &serde_json::to_string_pretty(modes).expect("modes serialize"));
}

fn write(path: &Path, text: &str) {
    if let Err(e) = write_checked(path, text) {
        log::error!("{e}");
    }
}

/// Writes via a temporary file and a rename, so a crash never leaves half a file.
fn write_checked(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(|e| format!("writing {}: {e}", path.display()))
}
