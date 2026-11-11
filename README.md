# KemoLight

<p align="center">
  <img src="assets/logo.svg" alt="KemoLight logo" width="96">
</p>

KemoLight is a lightweight egui brightness controller. It supports Linux desktops and Windows external monitors that expose brightness control through DDC/CI.

## Ubuntu GNOME Support

- Main window: egui/eframe native window.
- Top bar/tray integration: freedesktop StatusNotifierItem through the Rust `ksni` crate.
- Internal laptop panels: `/sys/class/backlight`.
- External monitors: `ddcutil` using DDC/CI MCCS VCP code `0x10`.
- If tray registration fails, the app keeps running in normal window mode.
- Hide to Tray closes the UI and releases its renderer, fonts, and layout caches while keeping the tray and brightness worker alive. Opening from the tray recreates the window at its previous size and position; Quit exits the process.
- On Linux with tray support, the app prefers X11/XWayland for the main window because Wayland does not support hiding and later re-showing an existing window through winit.
- Tray menu contents are refreshed through StatusNotifierItem/DBusMenu update events after brightness state changes.

GNOME usually needs AppIndicator support enabled for tray/top-bar icons:

```bash
sudo apt update
sudo apt install gnome-shell-extension-appindicator
```

## Linux Dependencies

Install the desktop and monitor-control dependencies:

```bash
sudo apt install build-essential pkg-config libxkbcommon-dev libwayland-dev libx11-dev \
  libxkbcommon-x11-dev libasound2-dev libfontconfig1-dev ddcutil i2c-tools
```

External monitors must have DDC/CI enabled in the monitor OSD. Some monitors ship with DDC/CI disabled.

For DDC/CI access without root, Ubuntu commonly needs `i2c-dev` and group permissions:

```bash
sudo modprobe i2c-dev
sudo usermod -aG i2c "$USER"
```

Log out and back in after changing groups. If `ddcutil detect` reports permission errors, check `/dev/i2c-*` permissions and group membership.

## Windows Support

- Main window: egui/eframe native window.
- System tray: native Windows notification area icon using `Shell_NotifyIconW`.
- Tray menu: open window, refresh displays, set brightness presets, adjust brightness, and quit.
- Internal laptop panels: Windows WMI/CIM brightness APIs.
- The internal panel is hidden while the laptop lid is closed and restored after it opens, including when the app starts with the lid closed.
- Display changes (plug/unplug, desktop configuration, display power, and resume) automatically refresh the main window and tray list. Notifications are coalesced, with a second refresh after drivers settle; this also works without a tray icon.
- External monitors: Windows Monitor Configuration API over DDC/CI.
- Only monitors that report brightness support through Windows DDC/CI APIs are enabled. Unsupported monitors are listed but their controls are disabled.
- Global shortcuts: **Shift+Win+B** applies the Bright profile; **Shift+Win+D** applies the Dark profile, including while hidden to the tray. A shortcut conflict is reported in the main window; tray and toolbar controls remain available.
- **Shift+Win+mouse wheel** adjusts the supported display under the pointer, including in tray mode. Each notch uses `brightness_step`. A small brightness bar appears at the bottom of that screen after scrolling. Its separate process exits automatically when scrolling stops. The wheel passes through when the app cannot identify a single controllable display at the pointer.
- The tray/hardware controller and UI use separate processes of the same executable. Hiding or closing the window exits the UI process and releases its graphics driver resources. Opening it again restores its size, position, and current state from the controller.

External monitors must have DDC/CI enabled in the monitor OSD. Some HDMI/DP adapters, docks, KVMs, GPU drivers, HDR modes, and vendor utilities can block or interfere with DDC/CI brightness commands.

## Memory Use

- With `start_hidden = true` and an available tray, no main window or graphics context is created until you open the window.
- On Windows, the tray process never initializes egui's renderer. The UI child receives snapshots and sends commands over inherited anonymous pipes; it does not enumerate displays, register shortcuts, or write profiles itself. Only the tray process remains after hiding the window, and it retains brightness state and window geometry.
- Hiding or closing the main window with a tray available destroys the UI resources. Tray controls, profiles, display notifications, and global shortcuts continue to work in the background. Quitting terminates both processes; the UI also exits if its owner disconnects.
- State and Linux tray update notifications are coalesced into single pending wakeups, so notifications cannot accumulate while the window is closed. The Windows tray builds its menu on demand and needs no update forwarding thread.
- The total memory while a Windows window is open includes both processes. The main saving is in tray mode, when the UI process and all of its driver allocations are gone. Resident and private memory vary by driver and platform.

The Windows process lifecycle tests require an interactive desktop with OpenGL:

```powershell
cargo test --locked --test ui_process -- --ignored --test-threads=1
```

## Brightness Profiles

1. Adjust each display to the brightness you want and wait for updates to finish.
2. Open **Profiles** in the top toolbar and choose **Save current** under **Bright** or **Dark**.
3. Use **Apply**, the tray's **Bright Profile / Dark Profile** entries, or the Windows global shortcuts to restore that profile.

Each profile stores a separate brightness for each display in `profiles.toml`, alongside `config.toml`. Saves survive app restarts. Saving updates the currently available displays and preserves saved values for closed or disconnected screens; applying skips screens that are currently hidden, unavailable, unsupported, or have no saved value. Opening/reconnecting a screen restores its list entry; apply a profile again to set its saved brightness.

Windows profiles match the WMI instance or monitor device interface rather than the display's enumeration number. Displays without a stable Windows device identity remain manually controllable and are excluded from profiles. Connecting a display through a different port or dock can change its Windows identity and require saving it again.

## Run

```bash
cargo run
```

Measure display discovery and brightness worker initialization without changing brightness:

```bash
cargo run --locked --example brightness_timing
```

Useful debugging commands:

```bash
ddcutil detect
ddcutil getvcp 10
ls -la /sys/class/backlight
RUST_BACKTRACE=1 cargo run
```

On Windows, run the app directly:

```powershell
cargo run
```

## Configuration

On Windows and Linux, enable **Settings → Start at sign-in** to register KemoLight for the current user. The option is off by default and requires no administrator privileges. At sign-in, KemoLight starts in the tray when available and opens its window if the tray is unavailable.

The startup entry uses the current executable's location. If you move KemoLight, toggle the option off and on from the new location.

Config is read from:

```text
~/.config/kemolight/config.toml
```

Example:

```toml
start_hidden = false
brightness_step = 5
preferred_backend = "auto" # auto, ddcutil, sysfs, mock
```

## Features

- `tray`: enables Linux StatusNotifierItem tray integration.
- `linux-ddcutil`: enables external monitor control through `ddcutil`.
- `mock-backend`: includes a mock backend for development and tests.

Default features enable tray, linux-ddcutil, and mock-backend. Linux tray and ddcutil dependencies are only built on Linux. Windows tray support is built through `Shell_NotifyIconW`; Windows internal panel support is built through WMI/CIM; Windows DDC/CI support is built through the Windows Monitor Configuration API. eframe provides Wayland and X11 support through its default native features; the app chooses X11/XWayland when tray hiding is available.

## Current TODOs

- macOS backend is a skeleton.
- GNOME native shell integration still depends on the desktop having an SNI/AppIndicator host.
- Sysfs writes may require distro-specific policy or helper integration for non-root users.
- DDC/CI behavior varies by monitor, cable, dock, GPU driver, and permissions.
