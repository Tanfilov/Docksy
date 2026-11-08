# Seelena

A lightweight native taskbar for Windows 11, with configurable height and icon size.
Built for ARM64 (Snapdragon / NVIDIA N1X); also builds for x64.

- one small native process (~0.5 MB exe), Direct2D drawing, no WebView
- one bar per monitor; it reserves its screen edge like the real taskbar
- pinned apps (imported from your Windows taskbar on first run) and running apps,
  grouped per app, with focus/attention indicators
- system tray icons, quick-settings button (Win+A), clock with date (Win+N),
  Start button (Win; right-click = Win+X), show-desktop strip (Win+D)

## Build

Needs Rust, plus Visual Studio Build Tools with **MSVC ARM64 build tools**.

```bash
cargo build --release
```

Output: `target/aarch64-pc-windows-msvc/release/seelena.exe` and `seelena_trayhook.dll`
(keep both in the same folder).

## Use

- Run `seelena.exe`. It hides the Windows taskbar while running.
- Right-click empty space on the bar for: Task Manager, settings, pinned apps,
  Start with Windows, restart, and **Exit and restore Windows taskbar**.
- If it ever crashes and the Windows taskbar stays hidden: `seelena.exe --restore`.
- `seelena.exe --dump` writes how pins and windows are detected to the log.

Settings: `%APPDATA%\seelena\config.toml` (applies on save) — `height`, `icon_size`,
`tray_icon_size`, position, colors, backdrop, clock format.
Pins: `%APPDATA%\seelena\pins.txt`. Log: `%APPDATA%\seelena\seelena.log`.

App buttons: click = focus/minimize (several windows: cycle through them),
middle-click = new instance, right-click = window list, pin/unpin, close.

## How the tray works

Explorer keeps owning the real tray (its taskbar is only hidden). `seelena_trayhook.dll`
is hooked into explorer's tray thread (`WH_CALLWNDPROC`) and copies every
`Shell_NotifyIcon` message to Seelena; clicks are forwarded to the owning apps.
The DLL must match explorer's architecture (ARM64 here).

## Origin and license

Window filtering, pinned-app import, native-taskbar hiding and the tray hook are
ported from [Seelen UI](https://github.com/eythaann/seelen-ui) (AGPL-3.0-or-later),
so this project is AGPL-3.0-or-later as well.
