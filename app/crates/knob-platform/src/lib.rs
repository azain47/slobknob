//! Per-OS glue for SlobKnob: a global hook that sees the knob's media keys
//! and can swallow them, plus the OS side of every [`Action`].
//!
//! Every platform module exposes the same free functions, re-exported here:
//!
//! - [`start_hook`]: install the hook on its own thread. The handler runs on
//!   that thread for each [`HookEvent`] and must return quickly; the hook
//!   swallows the key on [`Decision::Swallow`] or if the handler panics.
//!   Every media key the platform recognizes is reported (the engine decides
//!   which ones are the knob). Events carry whether the wheel modifier
//!   (Cmd / Win) is physically held; synthetic modifier presses from
//!   [`perform`] must not count.
//! - [`set_device_listener`]: start or stop watching which HID device sends
//!   each media key. While on, [`InputEvent::device`] is filled in (None if no
//!   device report matched within a short wait). Off by default, and only
//!   turned on when the user enables the device filter or runs "learn my
//!   knob", because on macOS it needs the Input Monitoring permission.
//!   While it is on, media keys from devices without a matching report wait
//!   for it: up to ~40 ms on macOS (in the tap), ~80 ms on Windows, where the
//!   hook swallows every media key and a resolver thread calls the handler
//!   (including for `ModifierReleased`) and re-injects keys it passes.
//! - [`device_permission`]: whether the device listener may run (macOS
//!   Input Monitoring; always true elsewhere); with `prompt` the OS asks.
//! - [`perform`]: do an action. Call it from a worker thread, never from the
//!   hook handler. Synthetic media keys it posts are marked so the hook skips
//!   them.
//! - [`frontmost_app`]: name of the frontmost app, matched against
//!   `AppWheel::matches` (macOS: app name, Windows: exe name without ".exe").
//!   Cheap enough to call from the hook handler.
//! - [`accent_color`]: the OS accent color, if there is one.
//! - [`input_permission`]: whether the hook is allowed to run (macOS
//!   Accessibility); with `prompt` the OS asks the user.

use std::fmt;

pub use knob_core::{Action, Decision, InputEvent};

pub mod overlay;
mod runtime;
pub use runtime::{Callbacks, Runtime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    Key(InputEvent),
    /// The physical wheel modifier (Cmd on macOS, either Win key on Windows)
    /// went up. The modifier event itself always passes through.
    ModifierReleased,
}

/// Runs on the hook thread for every [`HookEvent`].
pub type KeyHandler = Box<dyn Fn(HookEvent) -> Decision + Send + Sync + 'static>;

#[derive(Debug)]
pub enum PlatformError {
    /// The OS refused the hook (macOS: Accessibility not granted).
    PermissionDenied,
    Unsupported,
    Other(String),
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlatformError::PermissionDenied => write!(f, "input monitoring permission not granted"),
            PlatformError::Unsupported => write!(f, "this platform is not supported yet"),
            PlatformError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for PlatformError {}

/// Calls the handler, treating a panic as "swallow" so a bug never leaks a
/// knob key to the OS (which would e.g. mute the system).
pub(crate) fn call_handler(handler: &KeyHandler, ev: HookEvent) -> Decision {
    let desc = format!("{ev:?}");
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(ev))).unwrap_or_else(|_| {
        log::error!("knob handler panicked; swallowing {desc}");
        Decision::Swallow
    })
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{accent_color, device_permission, frontmost_app, input_permission, perform, set_device_listener, start_hook, Hook};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    accent_color, device_permission, frontmost_app, input_permission, is_foreground, perform, set_device_listener, start_hook,
    Hook,
};

#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported;
#[cfg(not(any(target_os = "macos", windows)))]
pub use unsupported::{accent_color, device_permission, frontmost_app, input_permission, perform, set_device_listener, start_hook, Hook};
