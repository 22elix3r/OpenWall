# Owned EGL surface/input proof

The opt-in Linux `owned_egl_wayland_probe` checks an application-owned EGL window on a
native Wayland surface. This is an M0 diagnostic, separate from production wallpaper
playback and the [windowless video texture/fence proof](owned-gl-video-probe.md). It
does not initialize GStreamer or the scene renderer.

```sh
cargo build -p openwall --example owned_egl_wayland_probe --features video-gl-probe --locked
python3 scripts/test_owned_egl_wayland.py
```

The harness starts its own headless Sway session and sets its private socket/runtime
directory explicitly. It removes inherited desktop/session sockets, uses Mesa software
EGL and provides no physical input devices. The binary refuses to connect without
`OPENWALL_PRIVATE_COMPOSITOR=1`. That is an accidental-launch guard, not an OS sandbox;
do not set it to point this diagnostic at a desktop session.

## Surface and context ownership

Rust owns the `wl_surface`, its overlay layer-shell role, `wl_egl_window`, EGL window
surface and GL context (GLES 3 by default, desktop core OpenGL 3.3 with `--gl-api desktop`). It acknowledges the role's configure before the first EGL
attachment and validates dimensions before allocation. The native window is created
through the public [wayland-egl API](https://docs.rs/wayland-egl/0.32.11/wayland_egl/struct.WlEglSurface.html).
The EGL display comes from the same live Wayland connection. No opaque GStreamer window
handle is cast to a protocol object and no private upstream struct is accessed.

The EGL window/context stays on its creating thread. Resize changes the existing native
window size, then redraws original opaque quadrants into the default framebuffer. It
creates no video child/subsurface and reads no pixels back into the client. The renderer
sleeps in a socket/deadline poll between changes; SIGINT wakes that poll through a private
socket. Dimensions are limited to 16,384 per side and 256 MiB of RGBA pixel storage;
driver/compositor buffers and copies are additional.

Before destroying the protocol root, cleanup unbinds the context, destroys EGL resources
and the native window, then terminates its owned EGL display. The connection remains live
through that sequence. A completed run prints JSON only after cleanup.

## Real input and presentation checks

An opaque overlay makes click-through a stronger test than a background-only role.
Its input region is explicitly empty and keyboard interactivity is disabled. A separate
fullscreen XDG witness underneath uses one immutable shared-memory pixel and viewporter;
it needs no additional GPU device/context. The private compositor's virtual-pointer
protocol injects a left-button press/release at the output center. The test requires the
witness to receive both events after compositor `wp_presentation` feedback. It repeats
the hit test after output geometry changes. The opaque-input negative control must fail
because the EGL surface intercepts those same events.

The harness captures compositor pixels with `grim`, verifies upright color quadrants,
changes mode, scale (1.25 and 2) and rotation, and checks that one EGL context is retained.
It bounds CPU used during a one-second static interval to catch a busy event loop and
checks that no draw occurs during that interval. This is a regression bound, not an
efficiency/energy benchmark. SIGINT, SIGKILL and compositor loss must remove both surfaces;
failure and killed processes must not publish successful reports.

Private Ubuntu 24.04/Sway 1.9/Mesa 25.2.8 and Ubuntu 26.04/Sway 1.11/Mesa 26 software
rendering runs passed these checks on 2026-10-18. CI runs the same diagnostic on both
Ubuntu releases; default/macOS builds
compile an unavailable-platform fallback. This does not qualify physical hotplug,
hardware drivers, native-resolution fractional rendering, GNOME/Plasma roles or macOS.
The test overlay and input witness are not product UI or a production wallpaper role.
This single-output diagnostic owns one EGL display/context. A multi-output presenter
must share display ownership and destroy it only after all native surfaces and GL
consumers have stopped; independent guards must not terminate a shared display.

Both API profiles run through the native input/lifecycle suite. The separate
[desktop GL interop checks](desktop-gl-video-interop.md) explain why decoder compatibility
must also be qualified before selecting a production video context API.

The next gate combines this owned surface with the shared video texture consumer and
its bounded producer/consumer synchronization. Frame readiness must then be tied to the
specific compositor presentation feedback, with decoder/format changes, looping,
resize and failure teardown tested before replacing production playback.
