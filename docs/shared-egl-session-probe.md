# Shared EGL ownership across private outputs

The diagnostic EGL owner separates one thread-local `Session` from individual native
`Window` surfaces. The session initializes/terminates the EGLDisplay and owns the UI
context. Windows retain it through `Rc` and own EGLSurface/wl_egl_window resources.
All windows on one Wayland connection must use the same session. Each window checks its
protocol surface's connection before crossing the native ABI. Production playback is
unchanged by these optional diagnostics.

Window destruction makes the shared context current without a drawable, destroys that
window's EGLSurface, then frees its native Wayland EGL wrapper. Other windows retain the
display/context. The retained session also supports GL resource cleanup with no windows,
using [surfaceless contexts](https://registry.khronos.org/EGL/extensions/KHR/EGL_KHR_surfaceless_context.txt).
The diagnostic rejects a driver that cannot provide that binding. Final session destruction
unbinds/destroys the context and terminates the display once. The video probe retains its
session until all GStreamer objects, producer references, leases and foreign wrappers are
gone. Its successful lifecycle report is captured after native cleanup.

Counters describe the application's successful EGL ownership calls. GStreamer's separate
producer contexts, driver internals and compositor resources are outside those counters.

```sh
cargo build -p openwall --examples --features video-gl-probe --locked
python3 scripts/test_shared_egl_session.py
python3 scripts/test_shared_egl_session.py --gl-api gles
# Explicit use of an existing GPU/driver in the private headless compositor:
python3 scripts/test_shared_egl_session.py --nvidia
python3 scripts/test_shared_egl_session.py --nvidia --gl-api gles
```

The harness verifies quadrant pixels and presentation feedback on two outputs, plus real
click-through on the first. It resizes the second, removes the first, then checks the
surviving output through fractional/integer scale and rotation changes. A marker texture
must remain valid after the first window disappears and after both windows are gone.
Final counters require two surface creations/destructions, one context creation/destruction
and one display initialization/termination. Idle checks, SIGINT, SIGKILL and compositor
loss exercise teardown. `--artifact-directory NEW_PATH` saves private screenshots/logs
on failure.

## Driver requirements found by the pixel tests

Painters explicitly select the window back buffer, or `COLOR_ATTACHMENT0` for an off-screen
target. On NVIDIA 580.178.04, a desktop context initially made current without a drawable
retained `GL_NONE` (`0x0`) when attached to its first window. Swaps and presentation feedback
succeeded while pixels remained blank. Selecting the back buffer restored rendering. The
report records the initial window draw-buffer value. GLES uses `glDrawBuffers` for its
back buffer; desktop GL uses `glDrawBuffer`.

A window becomes current **before** its native resize notification. NVIDIA's
[implementation notes](https://github.com/NVIDIA/egl-wayland2#implementation-notes) explain
the older implementation's deferred resize for a non-current window. The local EGL
Wayland 1.1.21 runtime reproduced an 800×500 draw targeting the previous 640×360 buffer,
with clipped quadrants. Binding before resizing fixed that path. A second binding follows
the notification. Contexts and surfaces remain retained. Some software paths keep the
previous drawable until swap; EGL permits that deferred
[native-window resize](https://registry.khronos.org/EGL/specs/eglspec.1.5.withchanges.pdf).
If the reported extent still differs, the event loop schedules exactly one follow-up
repaint after presentation. That draw corrects the geometry without a continuous timer.
Input/geometry confirmation waits for its presentation, and video records flag the earlier
commit as requiring a geometry refresh. The suite bounds follow-up draws to the four
actual geometry changes. Actual compositor pixels remain the acceptance check; an immediate
`eglQuerySurface` equality assertion would reject valid deferred resizing.

The original native video proof also passed after separating session/window ownership.
Its painter and off-screen counterpart share the same explicit destination-buffer policy.

## Scope

The two-output core GL/GLES matrix is exercised in private Ubuntu 24.04/Sway 1.9/Mesa
25.2.8 and Ubuntu 26.04/Sway 1.11/Mesa 26.0.8 software sessions. Explicit hardware tests
use Fedora 43/Sway 1.11/wlroots 0.19.3, RTX 4070 Ti SUPER, NVIDIA 580.178.04 and EGL Wayland
1.1.21. The hardware compositor/client select the same existing render node and vendor;
they never connect to a desktop session. Software CI runs both APIs.

This is a bounded two-surface ownership proof. The protocol helper has two diagnostic
slots. Production still needs general output management, decode sharing, zero-output
recreation, physical-resolution HiDPI, fit/aspect policy, independent output pacing,
device changes, suspend, other desktop roles and sustained resource measurements.
Window reference counting alone does not establish the lifetimes of external producer
contexts and GPU leases; the worker must retain the session until those are released.
