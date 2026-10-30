# SlobKnob

A radial mode wheel for keyboard volume knobs (built for the AULA F75, works with any knob that
sends volume up / down / mute). Pick a mode on the wheel, then the knob scrolls, zooms, switches
apps, tabs or desktops, skips or seeks media, steps frames in DaVinci Resolve, and so on.

## Using it

| Do this                         | Result                                                        |
| ------------------------------- | ------------------------------------------------------------- |
| Hold Cmd (macOS) / Win and turn | wheel opens; keep turning; let go of the modifier to pick     |
| Double-tap the knob             | wheel opens; turn to highlight; picks 0.6 s after the last turn, or tap to pick now |
| Esc, or 5 s without turning     | close the wheel without picking                               |
| Tap                             | the mode's tap action (Volume: mute, Media: play/pause, ...) |
| Turn                            | the mode's action                                             |

Per-app wheels: while DaVinci Resolve is in front, the wheel shows Resolve modes (jog, edit points,
drag a value under the mouse, ...) and remembers its own mode.

**Action slices** run something once instead of changing what the knob does: press a shortcut
(`cmd+shift+t`, or a sequence like `ctrl+k, ctrl+c`; `mod` is Cmd on macOS and Ctrl on Windows), open
an app, file or URL, type a piece of text, press a media key, or open a **sub-wheel** with more slices.
Pausing on an action never runs it; tap the knob (or let go of Cmd / Win) to run it.

**Settings window** (tray > Settings…, or `slobknob --settings`): edit wheels and actions with a live
preview that follows the real knob while the window is in front (turn off Knob > Preview in Settings to
use the knob normally there instead), learn your knob, and change timing,
scrolling and appearance. Everything is saved to `config.json` (macOS:
`~/Library/Application Support/app.slobknob/`, Windows: `%APPDATA%\app.slobknob\`), which can also be
edited by hand ("Reload Config" in the tray).

**Learn my knob** (Settings > Knob): turn right, turn left, press. SlobKnob records which media keys the
knob sends (some knobs press Play/Pause instead of Mute) and which keyboard it is on.

**Only use the knob's keyboard**: with a learned keyboard, volume keys on other keyboards (e.g. a
MacBook's own keys) keep doing volume whatever the knob's mode is. SlobKnob matches each media key to
the HID report that sent it, which needs Input Monitoring permission on macOS. It cannot separate the
knob from volume keys on the *same* keyboard if those send identical reports (likely, not yet checked).

Also: invert direction, smooth scrolling that speeds up when you spin fast, launch at login, pause.

## Layout

```
crates/knob-core       state machine, modes, config. Pure Rust, unit tested, no OS code.
crates/knob-platform   per-OS hook + actions (macos.rs, windows.rs), runtime threads, overlay window helpers
src-tauri              the app: overlay window, tray, settings files
ui                     the wheel and HUD (plain HTML/SVG/JS, Lucide icons)
```

The hook only decides "swallow or pass" from in-memory state and returns at once; actions and UI
updates run on a worker thread. On macOS the event tap only listens to media keys and modifier
changes, so normal typing never passes through it. On Windows the low-level keyboard hook returns
immediately for every key except volume up / down / mute and the Win key.

## Build

```sh
npm install
cargo test -p knob-core
npx tauri dev      # run
npx tauri build    # .app / .dmg on macOS, .exe / .msi on Windows
```

macOS needs Accessibility permission (System Settings > Privacy & Security > Accessibility).
`cargo run -p knob-platform --example knobcli` runs the hook and engine without UI and prints events.

### Windows .exe from Linux / WSL (no Visual Studio)

```sh
rustup target add x86_64-pc-windows-msvc
cargo install --locked cargo-xwin   # downloads the MSVC CRT + Windows SDK on first build
npx tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --no-bundle
# -> target/x86_64-pc-windows-msvc/release/slobknob.exe
```

Needs `clang-cl`, `llvm-rc` and `llvm-lib` (LLVM) and `lld-link` on PATH. Without LLVM's lld, symlink
rustup's `rust-lld` as `lld-link`; rustup's own `gcc-ld/lld-link` wrapper fails (rustc already passes
`-flavor link`). `--no-bundle` skips the installer, which would need NSIS.

### Releases

CI (`.github/workflows/build.yml`) runs tests and clippy and builds the installers on every push. To
release, set `version` in `src-tauri/tauri.conf.json`, commit, and push a matching tag (`git tag v0.2.0 &&
git push origin v0.2.0`): CI attaches a universal macOS `.dmg` and the Windows `-setup.exe` / `.msi` to a
draft GitHub Release, which you then publish. Builds are unsigned (macOS ad-hoc): macOS needs right-click >
Open the first time, and Windows SmartScreen asks once.

## Status

- macOS: tested with simulated knob events (hook, wheel, modes, action and sub-wheel slices, overlay,
  settings window). Device matching was checked with a real AULA F75 in a standalone test, not yet in
  the app.
- Windows: cross-compiled from WSL (above) and run on Windows 11 with a real AULA F75 on its 2.4G
  receiver: hook, settings window, wheel overlay and modes work. "Learn my knob" passed with simulated
  keys but is not yet confirmed with the real knob; "Only use the knob's keyboard" is untested. The
  F75's knob keys reach the low-level hook flagged as injected (Windows' HID service synthesizes them),
  so they look like software key presses. A native build (`npx tauri build` on Windows) is untried.
- Linux: not supported yet.
- Shortcuts use US-layout key positions, so punctuation shortcuts may differ on other layouts. Typed
  text is layout independent.
- Brightness on macOS uses DisplayServices with brightness keys as fallback; apps that take over
  brightness (e.g. BetterDisplay) can block both.
