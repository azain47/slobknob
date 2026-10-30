//! Runs the hook + engine without any UI and prints what would be shown.
//! `cargo run -p knob-platform --example knobcli`

use std::collections::HashMap;

use knob_core::Config;
use knob_platform::{input_permission, Callbacks, Runtime};

fn main() {
    if !input_permission(true) {
        eprintln!("needs Accessibility permission for this terminal / binary; grant it and rerun");
    }
    let callbacks = Callbacks {
        on_ui: Box::new(|ui| println!("ui   {}", serde_json::to_string(&ui).unwrap())),
        on_mode: Box::new(|wheel, mode| println!("mode {wheel} -> {mode}")),
        on_learn: Box::new(|l| println!("learn {}", serde_json::to_string(&l).unwrap())),
        on_preview: Box::new(|key, down| println!("preview {key:?} {down}")),
    };
    let _rt = match Runtime::start(Config::default(), HashMap::new(), callbacks) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("could not start: {e}");
            std::process::exit(1);
        }
    };
    println!("ready");
    loop {
        std::thread::park();
    }
}
