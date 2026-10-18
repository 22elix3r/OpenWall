# Experimental Plasma presentation bridge

This M0 feasibility adapter presents an external worker's Wayland surface inside a
trusted Plasma `WallpaperItem`. It leaves media decoding, scene parsing, shaders and
content code in the Rust/GStreamer process. Plasma's desktop widgets and panel keep
their normal position above the wallpaper. The package loads only OpenWall's own QML.

The adapter is **not connected to the normal Apply command or GUI yet**. Those still
report that Plasma needs its dedicated integration. There is no manager, authenticated
discovery, persistent assignment, automatic worker launch or restart policy. Do not
install it expecting a complete wallpaper application.

## Test without changing your desktop

Build the worker, install Sway, grim, Pillow, Mesa's software Vulkan/OpenGL drivers,
Qt Wayland, Qt's `qml` executable and its `QtWayland.Compositor` QML module. Run:

```sh
cargo build -p openwall --locked
python3 scripts/test_plasma_bridge.py
```

This loads the actual `ExternalSurface.qml` in a Qt window inside headless Sway.
Ubuntu packages include `qml-qt6` and `qml6-module-qtwayland-compositor`.
Pass `--qml /path/to/qt6/bin/qml` if executable discovery needs an explicit path.
Linux CI runs this bridge test; it does not run a full Plasma desktop.

On a machine with Plasma 6, KWin Wayland, `qdbus` and ActivityManager:

```sh
cargo build -p openwall --example headless_pointer --locked
python3 scripts/test_plasma_bridge.py --plasma --artifacts /tmp/openwall-plasma-results
```

The opt-in test installs a copy of the package in temporary XDG directories, starts
a private D-Bus session allowing only ActivityManager activation, and nests KWin/Plasma
inside the private Sway session. It never sends scripts to your session bus. It closes
its workers and desktop and removes the temporary configuration. Only the explicit
artifact directory survives. Original quadrant artwork and a test widget are generated
or loaded from this repository. Add `--video-fixture /tmp/openwall-loop.webm` to exercise
native video as described in [validation](../../docs/validation.md).

## Presentation contract and limits

Each real desktop item creates a new `openwall-plasma-<output hash>-<instance>` Unix
socket in `XDG_RUNTIME_DIR`. The hash bounds connector-name length; it provides no
authentication. Only one XDG toplevel is displayed. A second toplevel is disconnected
after Qt finishes handling its creation request. Popups and input forwarding are
disabled. Stopping or crashing the worker reveals a static gradient, and another
worker may reconnect to the same bridge. A new plugin instance gets a fresh socket.
Losing the presentation host terminates a connected worker with an error.

For the proof, the test starts `openwall preview` with `WAYLAND_DISPLAY` pointing to
that exact private socket. Scene workers use their existing winit/wgpu presentation.
Video uses a native GStreamer Wayland sink inside an OpenWall-owned XDG root and separate
container subsurface. The root maps only after decoded data reaches the sink. Resize
keeps the decoder alive, and desktop widgets stay above both scenes and video. Socket
globbing is test-only discovery; normal application integration needs a versioned
handshake identifying output,
activity, capabilities, scale and trusted worker lifetime before automatic launch.

Desktop detection currently uses Plasma `DesktopView` window properties. An ordinary
preview window shows the fallback and creates no compositor. These shell details must
be checked for each supported Plasma release. The adapter has been exercised on
Plasma/KWin 6.6.4 and Qt 6.10.3 with software rendering; other versions are unqualified.

The nested test path uses shared-memory buffers. Efficient DMA-BUF import, modifiers,
explicit synchronization, mixed GPUs, HDR/color management, multi-output/activity
transitions, shell restarts, physical hot-plug and sleep/wake remain M0/M4 gates.
Do not infer zero-copy playback or acceptable video energy consumption from these
correctness tests. Qt Wayland Compositor runs in the shell, so protocol/buffer handling
still belongs to the trusted attack surface; process separation is not a sandbox.

## Upstream API and license references

- [Plasma DesktopView source](https://invent.kde.org/plasma/plasma-workspace/-/blob/Plasma/6.6/shell/desktopview.h).
- [Qt Wayland Compositor](https://doc.qt.io/qt-6.10/qtwaylandcompositor-index.html).
- [WaylandQuickItem lifecycle](https://doc.qt.io/qt-6.10/qml-qtwayland-compositor-waylandquickitem.html).

Original adapter/test code is AGPL-3.0-or-later. Qt Wayland Compositor uses its GPLv3
open-source option, distinct from Qt's LGPL modules. The runtime is supplied by the
host distribution. See [third-party provenance](../../THIRD_PARTY.md).
