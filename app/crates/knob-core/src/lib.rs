//! Platform-independent core of SlobKnob.
//!
//! A keyboard knob sends three media keys: volume up (clockwise), volume down
//! (counter-clockwise) and mute (press). Platform hooks turn those into
//! [`KnobEvent`]s and ask the [`Engine`] what to do. The engine answers
//! synchronously with a [`Decision`] (swallow the key or let it through) plus a
//! list of [`Effect`]s that the caller runs later, off the hook thread.
//!
//! Timers (double-tap window, auto-confirm, idle close, ...) live inside the
//! engine. The caller asks [`Engine::next_deadline`] when to call
//! [`Engine::tick`] next. Time is always passed in, so everything here is
//! deterministic and unit-testable.

mod config;
mod engine;
pub mod keys;
mod modes;
mod types;

pub use config::{ActionKind, AppWheel, Config, CustomAction, SliceDef, Trigger, DEFAULT_WHEEL_KEY};
pub use engine::Engine;
pub use keys::{Key, KeyCombo, Mods, NamedKey};
pub use modes::{mode_def, ModeDef, MODES};
pub use types::{
    Action, Axis, Decision, DeviceInfo, Effect, InputEvent, KnobEvent, KnobKey, KnobKeys, LearnEvent, MediaKey,
    OpenVia, Slice, SliceKind, UiEvent,
};
