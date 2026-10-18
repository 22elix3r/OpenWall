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

## Optional paused video publication

```sh
python3 scripts/test_native_gl_video.py --staged
python3 scripts/test_native_gl_video.py --staged --gl-api gles
python3 scripts/test_native_gl_video.py --staged --nvidia
# Hold past initial startup timeout, then publish without spinning:
python3 scripts/test_native_gl_video.py --staged --hold-seconds 11
```

The private `--staged-control PATH` mode combines the
[unmapped parent publication boundary](staged-egl-probe.md) with shared video textures.
The pipeline starts in PAUSED. An appsink `new_preroll` callback transfers the initial
sample into the same bounded delivery slot used for playing samples. It does not perform
GL work or wait for the consumer. GStreamer's
[preroll contract](https://gstreamer.freedesktop.org/documentation/app/appsink.html#pull-preroll)
also returns that buffer as the first playing sample; repeated PTS zero after resuming is
expected. Consumer draws still retain samples until their GPU fences signal.

The first frame stays retained while native drawable resizing is primed behind the
unmapped parent. There is no continuous playback or repaint timer during this hold.
One exact private `publish\n` command draws a fresh, correctly sized frame, requests
child presentation feedback and maps the parent. Actual presentation of that first frame
then transitions the same pipeline to PLAYING, so the media clock does not advance while
the initial commit waits for the compositor. Readiness also requires real click-through. Published
frames carry `published: true`; hidden records must never appear in the presented-ID
history. Cancellation before publication releases samples/fences before stopping the
pipeline and reports `cancelled_before_publish`, with zero presented frames.

The staged harness resizes before publication, checks held witness pixels and idle CPU,
then checks original animated pixels, two loops, fence retirement, selected output,
SIGINT, cancellation while paused, malformed control/media, input interception,
SIGKILL before/after publication and compositor loss. Both APIs run in software CI on
Ubuntu 26.04; hardware mode requires the same NVIDIA source/caps/log evidence as normal
qualification. The static parent proof separately covers Ubuntu 24.04's older compositor;
the combined video path still requires GStreamer 1.26 or newer.

Private Mesa 26.0.8/GStreamer 1.28.2 tests passed desktop GL and GLES; the explicit
NVIDIA 580.178.04/GStreamer 1.26.11 run passed desktop GL. Each displayed all twelve
original markers with zero sampled channel error, completed two loops and confirmed
26 published commits. Mesa recorded three hidden preroll draws, including one geometry
refresh; NVIDIA recorded two. All consumer fences retired without normal-path `glFinish`.
An eleven-second software hold passed with no new draws and 0.0 process CPU seconds at
procfs tick resolution. This is a bounded idle observation, not a sustained power budget.

This mode intentionally rejects geometry changes after publication. Keeping old content
visible while staging a replacement or resizing an already mapped output remains a
separate gate, along with general pause/visibility policy, decode sharing and worker
transactions. The normal diagnostic and production video backend retain their existing
behavior.

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

## Explicit NVIDIA hardware qualification

```sh
python3 scripts/test_native_gl_video.py --nvidia
# For multiple NVIDIA GPUs, select the existing render node explicitly:
python3 scripts/test_native_gl_video.py --nvidia --render-node /dev/dri/renderD129
```

This opt-in mode requires an existing NVIDIA driver, EGL configuration, `nvvp9dec`,
Sway and capture tools. It installs no drivers and bundles no vendor runtime. It selects
an NVIDIA DRM render node for the private headless compositor, checks the node's sysfs
vendor/driver, and forces that EGL vendor for the compositor and client. The compositor
still uses `WLR_BACKENDS=headless`, a private runtime directory/socket, no desktop display
or session bus, and virtual input only. Its debug log must identify NVIDIA and the selected
render node. Hardware qualification is outside software CI and must be requested with
`--nvidia`; the normal test retains the software compositor and decoder.

The original fixtures are doubled to 260×136 without changing their YUV values to meet
NVDEC's minimum dimensions. The native and windowless diagnostics share one-shot bounded
decoder-buffer tracing. NVIDIA qualification requires `nvvp9dec`, actual GL memory at its
source pad, an explicit GL-memory contract, a positive native GL-selection log and no
native system-fallback warning. This remains source/log-based evidence rather than a
CUDA API trace or proof of zero copies. The consumer's GL output caps alone do not establish
the decoder's transfer path.

On 2026-10-18, Fedora 43, Sway 1.11/wlroots 0.19.3, GStreamer 1.26.11 and an RTX 4070 Ti
SUPER with NVIDIA 580.178.04 passed the full native suite using core GL 3.3. All twelve
visible markers matched the independent reference with zero sampled channel error. The
run completed two loops and 29 labelled presentations, retired all 29 consumer fences
without normal-path `glFinish`, and passed the same resize/input/error/crash cleanup checks.
This demonstrates this driver/compositor combination in a headless session. Physical
monitor presentation, desktop shell behavior, sustained performance and power remain open.

This is a finite single-output proof with stretched SDR frames and logical-size buffers.
It does not establish general GPU/compositor compatibility, sustained playback, power savings,
aspect/fit policy, full-resolution HiDPI rendering, display color management, HDR, dynamic
formats, other desktop environments, multi-output presentation or macOS behavior.
The diagnostics now use a [shared EGL session owner](shared-egl-session-probe.md) for
display/context lifetime, with separately retained native windows. Production still needs
general output management on top of that bounded ownership proof. The optional
[two-output video diagnostic](shared-video-outputs-probe.md) exercises one decoder and
shared source texture through removal of its first native window.
Decoder sharing, device affinity, pause/resume, visibility policy and transactional apply
remain required before replacing the normal video backend. The separate
[desktop GL qualification](desktop-gl-video-interop.md) describes NVIDIA's transfer
boundary and why GL-looking memory does not establish a GPU-only path.
