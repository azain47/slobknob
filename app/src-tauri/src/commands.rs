//! Commands for the settings window.

use knob_core::{Config, MODES};
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::{settings, theme, AppState, Theme};

#[derive(Serialize)]
pub struct ModeInfo {
    id: &'static str,
    name: &'static str,
    hint: &'static str,
}

#[derive(Serialize)]
pub struct SettingsData {
    /// config.json as written (not sanitized), or the defaults if it can't be read.
    config: Config,
    problems: Vec<String>,
    /// Set when config.json is not valid JSON; saving would replace it.
    parse_error: Option<String>,
    modes: Vec<ModeInfo>,
    /// OS accent as hex, for the "system" accent in the preview.
    system_accent: String,
    platform: &'static str,
    config_path: String,
    paused: bool,
    running: bool,
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> SettingsData {
    let (config, parse_error) = match settings::load_raw(&state.paths.config) {
        Ok(cfg) => (cfg, None),
        Err(e) => (state.config.lock().unwrap().clone(), Some(e)),
    };
    let system = Config { accent: "system".into(), ..Config::default() };
    SettingsData {
        problems: config.problems(),
        config,
        parse_error,
        modes: MODES.iter().map(|m| ModeInfo { id: m.id, name: m.name, hint: m.hint }).collect(),
        system_accent: theme(&system).accent,
        platform: std::env::consts::OS,
        config_path: state.paths.config.display().to_string(),
        paused: state.paused.load(std::sync::atomic::Ordering::Acquire),
        running: state.runtime.lock().unwrap().is_some(),
    }
}

#[derive(Serialize)]
pub struct Saved {
    problems: Vec<String>,
    theme: Theme,
    /// Launch at login could not be changed (message for the user).
    autostart_error: Option<String>,
}

/// Writes config.json as given (the runtime gets the sanitized version) and
/// applies it right away.
#[tauri::command]
pub fn save_settings(app: AppHandle, config: Config) -> Result<Saved, String> {
    let state = app.state::<AppState>();
    let was_login = state.config.lock().unwrap().launch_at_login;
    settings::save_config(&state.paths.config, &config)?;
    crate::reload_config(&app);
    let autostart_error = if config.launch_at_login != was_login {
        set_autostart(&app, config.launch_at_login).err()
    } else {
        None
    };
    Ok(Saved { problems: config.problems(), theme: theme(&config.clone().sanitized()), autostart_error })
}

fn set_autostart(app: &AppHandle, on: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let al = app.autolaunch();
    let result = if on { al.enable() } else { al.disable() };
    result.map_err(|e| format!("launch at login: {e}"))
}

#[tauri::command]
pub fn start_learn(state: State<'_, AppState>) -> Result<(), String> {
    match state.runtime.lock().unwrap().as_ref() {
        Some(rt) => {
            rt.start_learn();
            Ok(())
        }
        None if state.paused.load(std::sync::atomic::Ordering::Acquire) => {
            Err("SlobKnob is paused. Resume it from the menu bar first.".into())
        }
        None => Err("The knob hook isn't running yet (Accessibility permission?).".into()),
    }
}

#[tauri::command]
pub fn cancel_learn(state: State<'_, AppState>) {
    if let Some(rt) = state.runtime.lock().unwrap().as_ref() {
        rt.cancel_learn();
    }
}

#[derive(Serialize)]
pub struct Permissions {
    /// macOS Accessibility (the hook). Always true elsewhere.
    accessibility: bool,
    /// macOS Input Monitoring (the device filter). Always true elsewhere.
    input_monitoring: bool,
}

#[tauri::command]
pub async fn permissions() -> Permissions {
    Permissions {
        accessibility: knob_platform::input_permission(false),
        input_monitoring: knob_platform::device_permission(false),
    }
}

/// Asks the OS for a permission ("accessibility" or "input_monitoring").
#[tauri::command]
pub async fn request_permission(kind: String) -> bool {
    match kind.as_str() {
        "accessibility" => knob_platform::input_permission(true),
        _ => knob_platform::device_permission(true),
    }
}

/// Opens System Settings > Privacy & Security at the given pane.
#[tauri::command]
pub fn open_privacy(kind: String) {
    #[cfg(target_os = "macos")]
    {
        let pane = if kind == "accessibility" { "Privacy_Accessibility" } else { "Privacy_ListenEvent" };
        crate::tray::open_target(&format!("x-apple.systempreferences:com.apple.preference.security?{pane}"));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = kind;
}

#[tauri::command]
pub fn open_config_file(state: State<'_, AppState>) {
    crate::tray::open_target(&state.paths.config.display().to_string());
}

/// Theme for the preview, for the accent being edited.
#[tauri::command]
pub fn preview_theme(accent: String, dim: bool) -> Theme {
    theme(&Config { accent, dim_screen: dim, ..Config::default() })
}
