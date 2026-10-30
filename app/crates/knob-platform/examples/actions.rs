//! Runs one action through the platform, for manual testing without the hook.
//! `cargo run -p knob-platform --example actions -- scroll 400`
//! `... -- keys "cmd+shift+t"`, `... -- text "hello"`, `... -- open https://example.com`
//! `... -- listen` prints which device each media key comes from (Input Monitoring).

use knob_core::{Action, Axis, KeyCombo};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
    match arg(0).as_str() {
        "scroll" => {
            let total: i32 = arg(1).parse().unwrap_or(400);
            for _ in 0..(total.abs() / 40).max(1) {
                knob_platform::perform(&Action::SmoothScroll { axis: Axis::Vertical, pixels: 40 * total.signum() });
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        "keys" => {
            knob_platform::perform(&Action::Shortcut(KeyCombo::parse_sequence(&arg(1)).expect("shortcut")));
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        "text" => knob_platform::perform(&Action::TypeText(arg(1))),
        "open" => {
            knob_platform::perform(&Action::Open(arg(1)));
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        "listen" => {
            println!("input monitoring: {}", knob_platform::device_permission(true));
            knob_platform::set_device_listener(true).expect("listener");
            let _hook = knob_platform::start_hook(Box::new(|ev| {
                println!("{ev:?}");
                knob_core::Decision::Pass
            }))
            .expect("hook");
            println!("listening; Ctrl-C to stop");
            loop {
                std::thread::park();
            }
        }
        other => eprintln!("unknown command {other:?}"),
    }
}
