SlobKnob
========

app/          The cross-platform app (Rust + Tauri). See app/README.md.
              Build: npm install && npx tauri build   (needs Rust and Node)
              Run without UI: cargo run -p knob-platform --example knobcli

hammerspoon/  The Hammerspoon version that runs on macOS today.
              Install: copy init.lua, knobwheel.lua and knobwheel-icons/ into ~/.hammerspoon/,
              install Hammerspoon (brew install --cask hammerspoon), grant it Accessibility,
              then Reload Config.
              Pause / resume:  open -g "hammerspoon://knobwheel?action=pause"   (or resume)
