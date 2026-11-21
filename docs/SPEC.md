# naelbar: specification

naelbar is an open source (MIT), local, Rust floating island for Windows 11.
Its behaviour is inspired by what can be seen from the outside of commercial
"notch" utilities. It contains no code, asset, icon or text taken from any of
them, and it never decompiles or inspects their binaries. It has its own name
and its own visual identity.

## Non-negotiable requirements

1. A small island at the top of the screen opens when the pointer moves onto it.
2. **Idle means zero.** With nothing playing and the island closed, the process
   performs no periodic work at all.

### Idle rules (every crate)

- No polling, no periodic timer, no `sleep` loop, no busy wait, while idle.
- Work is driven only by OS notifications: window messages, COM/WinRT event
  callbacks, `RegisterPowerSettingNotification`, `AddClipboardFormatListener`,
  `SetWinEventHook`, `RegisterHotKey`, blocking I/O on a parked thread.
- A timer is allowed only while something visible needs it (animation, a
  running focus timer, an open stats widget), and it is torn down immediately
  after. Use a one-shot timer aimed at the next deadline, never a tick.
- Parked threads are acceptable but kept to a minimum. Prefer OS callbacks.
- Expensive resources (WebView2, camera, stats counters) are created lazily and
  released as soon as their tab closes.

Targets, closed island, 30 s, no pointer movement, release build:
0 ms CPU delta, 0 context switches/s on our threads, under 30 MB working set.

Reference measured on the commercial app used as behavioural benchmark:
15.6 ms CPU over 30 s and 95 MB working set. We must beat both.

## Architecture

```
crates/naelbar          binary: windows, compositor, animation, UI, hotkeys, tray
crates/nb-core          Event enum and Sink type, the only shared contract
crates/nb-media         media sessions (GSMTC)
crates/nb-audio         volume, mute, devices, mic-in-use
crates/nb-clipboard     clipboard history
crates/nb-system        battery, power mode, radios, brightness, foreground, stats
crates/nb-integrations  local notify endpoint, agent hooks, toast mirror, weather
```

Service crate contract:

```rust
pub struct Handle { /* ... */ }           // Drop stops the service
pub fn start(sink: nb_core::Sink) -> Result<Handle, Error>;
impl Handle { /* command methods */ }     // callable from the UI thread, never block
```

Each service emits its current state once on start, then only on change. Each
has `examples/watch_<service>.rs`, which prints events until Ctrl+C for manual
checks.

### Shell design

- Pure Win32 through the `windows` crate. No webview, no retained GUI toolkit.
- `GetMessageW` loop. Blocked in the kernel when idle.
- **Visual window**: `WS_POPUP`, `WS_EX_TOPMOST | TOOLWINDOW | NOACTIVATE |
  LAYERED | TRANSPARENT | NOREDIRECTIONBITMAP`, fixed at the largest island
  size, never resized. DirectComposition + a composition swapchain, drawn with
  Direct2D and DirectWrite. It is always click-through.
- **Hit window**: `WS_EX_TOPMOST | TOOLWINDOW | NOACTIVATE | LAYERED`, alpha 1,
  above the visual window, resized to the current island shape. Mouse input
  comes from ordinary `WM_MOUSEMOVE`, with `TrackMouseEvent` for leave. This
  gives hover detection without hook or polling. Everywhere else, clicks reach
  the applications beneath.
- Frames are presented only during animations. Spring animation driven by DComp
  or vsync (`IDXGIOutput::WaitForVBlank` or the frame latency waitable object)
  while active; nothing once settled. GPU resources may be released after a
  few seconds closed, recreated on demand.
- Honours `SPI_GETCLIENTAREAANIMATION` for reduced motion, DPI per monitor v2.

## Features

P0 = first working release. P1 = next. P2 = later.

| Pri | Area | Behaviour |
|---|---|---|
| P0 | Island | Pill at top centre. Hover opens within 150 ms with a spring. Pointer leaving closes it after a short grace delay. Rounded card. |
| P0 | Tab bar | Icon row at the top of the card, dot under the active tab. Reopens on the last tab. |
| P0 | Media | Compact pill while playing, artwork, title, artist, previous/toggle/next, progress bar. |
| P0 | Quick settings | Brightness and volume sliders, Wi-Fi, Bluetooth, mute, battery tile, power mode segmented control. |
| P0 | Widgets | Calendar, focus timer (25 + 5 min), weather, system stats. Stats sample only while visible. |
| P0 | Clipboard | History cards with filters all/text/images/files, click to re-copy, clear. |
| P0 | Shelf | Drop files on the island, drag them out later. |
| P0 | Hotkeys | Ctrl+Alt+N toggle, P play/pause, T timer, H hide, M mic. All rebindable. |
| P0 | Tray and settings | Tray icon, config file `%APPDATA%\naelbar\config.json`, autostart. |
| P1 | Notifications | Mirror Windows toasts in the island, keep the last 24. |
| P1 | Calls | Mic in use timer, mute button, device pickers, mic mute HUD. |
| P1 | Volume HUD | Replaces volume feedback on change. |
| P1 | Fullscreen | Hide in fullscreen apps, driven by an appbar `ABN_FULLSCREENAPP` notification. Never `EVENT_OBJECT_LOCATIONCHANGE`, which wakes us on every animation of the foreground app. Thin strip under maximised windows. |
| P1 | Appearance | Island or notch-attached shape, top distance, position left/centre/right/sides. |
| P1 | Notify endpoint | `127.0.0.1` HTTP endpoint for scripts: `notify?title=&body=`. Refuses requests carrying a browser `Origin`. |
| P1 | AI agents | Claude Code and others report state through hooks, shown in pill and tab. |
| P1 | Launcher | Capture, snip, record, and user-defined shortcut slots. |
| P2 | Web tab | Pinned sites in a lazily created WebView2. |
| P2 | Export watcher | Growing files in configured folders shown with progress. |
| P2 | OBS | obs-websocket status, REC/LIVE in pill. |
| P2 | Game overlay | FPS/CPU/GPU strip. |
| P2 | Phone deck | Local Wi-Fi remote paired by QR code. Off by default. |
| P2 | Custom taskbar | Island-styled replacement taskbar. |
| P2 | Hide from capture | `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`. |
| P2 | Accessibility | Text scale, higher contrast, reduced motion, UI Automation. |
