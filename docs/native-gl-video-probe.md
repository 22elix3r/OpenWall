# Native shared-texture video presentation diagnostic

`native_gl_video_probe` combines the owned EGL Wayland surface with the shared GStreamer
texture consumer. It is an optional Linux diagnostic, guarded by
`OPENWALL_PRIVATE_COMPOSITOR=1`. This is an accidental-launch guard, not an OS sandbox.
Use its private Sway harness; it must never be launched on the user's desktop session.
Production playback continues to use the existing native sink.

```sh
cargo build -p openwall --examples --features video-gl-probe --locked
python3 scripts/test_native_gl_video.py
python3 scripts/test_native_gl_video.py --gl-api gles
```

GStreamer 1.26 or newer is required. Core OpenGL 3.3 is the default, with GLES 3 as an
explicit comparison. The application owns one Wayland EGLDisplay/context/surface and
passes a foreign display wrapper and the current wrapped context to GStreamer. The
native owner outlives all producer contexts, pipeline objects, buffers and GL resources.
The foreign wrapper does not terminate the borrowed EGLDisplay.

The appsink follows the media clock. Its streaming callback only pulls a sample into
a replaceable one-sample slot and wakes the native event loop through a nonblocking
socket. It performs no application GL calls, waits for no UI acknowledgement, and releases
replaced samples outside the slot lock. The native thread validates and waits for the
producer's GL sync metadata, then paints the shared RGBA texture directly into the
Wayland default framebuffer. The fixed painter and source/fence guards are also used by
the off-screen diagnostic; no second application GPU device, intermediate render target,
application source CPU map or application readback is introduced by this presenter.
Software decoding still uses CPU memory and uploads; these counters describe the consumer.

At most three GPU leases retain samples until consumer fences signal. One additional
sample is retained for resize/discard redraws, one may wait in the callback slot, and the
appsink queue is bounded to one. These are application delivery bounds, not a whole-process
memory limit: decoder pools, pipeline internals and compositor buffers are separate.
Each lease has its own submission deadline. An incomplete/error path may use `glFinish`
before releasing a source; normal retirement does not.

Each commit has an ID passed as presentation-feedback user data. A successful swap is
insufficient for readiness: the diagnostic waits for actual `wp_presentation.Presented`
and a real injected pointer press/release reaching the SHM witness underneath the empty
input region. Resize, fractional/integer scale and rotation reuse the native context.
Only a new frame, configure or discarded commit requests a draw; there is no continuous
render timer. Pending fences use bounded polling while other waits use the socket loop.

EOS looping drains every consumer lease and releases retained samples before the flush
seek. Shutdown disables delivery, clears its slot, removes callbacks and sets the pipeline
to NULL while the native context is still alive. The diagnostic allows 1–3 loops, at most
240 draw records, a 30-second maximum duration, and bounded frame/buffer dimensions.

## Validation and remaining gates

The private Ubuntu 26.04/Sway 1.11/Mesa 26.0.8/GStreamer 1.28.2 run on 2026-10-18 passed
both APIs. Original lossless 130×68 BT.709-limited fixtures visibly displayed all twelve
distinct frame markers and completed two loops. Every sampled compositor channel matched
the independent reference exactly. Each run presented 29 labelled commits, including
resize redraws, with real click-through after mode, 1.25×/2× scale and rotation changes.
Every consumer fence retired without normal-path `glFinish`. The harness also checks
selected-output isolation, malformed media, EOS cleanup, SIGINT, SIGKILL, compositor loss
and an opaque-input negative control. Screenshots come from the external compositor
capture tool; the application itself performs no readback. Software CI runs both APIs;
Ubuntu 24.04 checks rejection of its older GStreamer before any display connection.

This is a finite single-output proof with stretched SDR frames and logical-size buffers.
It does not establish physical GPU/compositor import, sustained playback, power savings,
aspect/fit policy, full-resolution HiDPI rendering, display color management, HDR, dynamic
formats, other desktop environments, multi-output presentation or macOS behavior.
The native window owner must not be duplicated per output on a common Wayland connection:
those surfaces share the EGLDisplay, so production needs a shared display/session owner.
Decoder sharing, device affinity, pause/resume, visibility policy and transactional apply
remain required before replacing the normal video backend. The separate
[desktop GL qualification](desktop-gl-video-interop.md) describes NVIDIA's transfer
boundary and why GL-looking memory does not establish a GPU-only path.
