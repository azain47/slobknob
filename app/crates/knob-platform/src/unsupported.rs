//! Placeholder for platforms without a hook yet (Linux).

use crate::{Action, KeyHandler, PlatformError};

pub struct Hook;

pub fn start_hook(_handler: KeyHandler) -> Result<Hook, PlatformError> {
    Err(PlatformError::Unsupported)
}

pub fn perform(_action: &Action) {}

pub fn frontmost_app() -> Option<String> {
    None
}

pub fn accent_color() -> Option<[u8; 3]> {
    None
}

pub fn input_permission(_prompt: bool) -> bool {
    false
}

pub fn device_permission(_prompt: bool) -> bool {
    false
}

pub fn set_device_listener(_on: bool) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}
