# Windowless video transfer probe

The Rust `video_transfer_probe` example isolates decoding and conversion from desktop
presentation. A clock-paced sink creates no wallpaper or native output window. CPU-bound
modes explicitly map and copy each frame into one bounded reusable CPU buffer. This M0
diagnostic helps distinguish CPU conversion, GPU conversion with readback, and successful
DMA-BUF delivery. Use the [worker benchmark](video-performance.md)
to measure the full layer-shell path separately.

Build on Linux with the normal video dependencies:

```sh
cargo build -p openwall --release --example video_transfer_probe --locked
target/release/examples/video_transfer_probe /tmp/openwall-loop.webm --mode cpu --seconds 30 --warmup 5
```

| Mode | Explicit conversion/sink chain | Required environment |
| --- | --- | --- |
| `cpu` | `videoconvert` → RGBA → CPU map/copy → paced sink | None |
| `gl-download` | `glupload` → `glcolorconvert` → `gldownload` → RGBA → CPU map/copy → paced sink | `GST_GL_WINDOW=surfaceless` |
| `dmabuf` | Same GL chain → DMA_DRM with `memory:DMABuf` → unmapped fake sink | `GST_GL_WINDOW=surfaceless` |

The GL modes refuse to start without the surfaceless setting. They require the distribution's
GStreamer OpenGL plugin (`gstreamer1.0-gl` on Ubuntu). A hardware probe exercised locally used:

```sh
GST_GL_WINDOW=surfaceless GST_GL_PLATFORM=egl GST_GL_API=opengl \
  target/release/examples/video_transfer_probe /tmp/openwall-loop.webm --mode gl-download
```

Try `--mode dmabuf` in the same environment to test export negotiation. Failure is useful
evidence; unsupported modifiers must not be forced. Depending on the decoder and GL
capabilities, negotiation can fail before any frame reaches the export element. GStreamer
may describe this as a missing decoder even when the factory exists. Diagnose the requested
memory formats rather than assuming that another codec package will fix it.

The complete conversion chain lives inside the supplied sink bin. `native-video` disables
playsink's implicit conversion chain, which would alter the experiment or reject DMA_DRM
before export. Media paths are passed through the filename-to-URI API, never pipeline text.
Only fixed, repository-owned pipeline descriptions are evaluated.

## Reading a result

A completed run prints JSON with process CPU time as a percentage of one core, actual
sample duration, warm-up, loop count, GStreamer version, decoder output caps, and the first
sink buffer's caps, byte count, allocator types, memory count and all-memory DMA-BUF check.
CPU-bound modes also report total copied bytes/frames across startup, warm-up and sampling.
Copy buffers are limited to 256 MiB; DMA-BUF mode never maps or copies the exported data.
Process CPU time includes native media threads. The probe waits on media events and deadlines, and SIGINT
wakes that wait. Errors and interrupted/incomplete samples exit nonzero without printing a
successful report. The 15-second startup deadline covers missing data events; use an external
process timeout to bound a blocked native-library call or teardown.

Record the build/media hashes, complete driver/plugin environment and GPU/compositor/driver
versions alongside results. Repeat warmed release runs. RGBA caps alone do not imply
system-memory backing: GL download can return `GLMemoryPBO`, deferring final CPU access
until mapping. The explicit CPU consumer includes that map/copy cost, unlike a bare fake
sink. It adds a controlled full-buffer copy to both CPU-bound modes; it does not reproduce
the real Wayland sink's buffer-pool allocation or compositor work.

The probe itself performs no pixel/color comparison; a GL pipeline can negotiate different
color metadata from a CPU pipeline. The [SDR color regression suite](video-color.md)
compares captured samples with independent reference equations and records an unresolved
CPU precision gap. Similar caps or successful playback do not establish matching colors.
The GL-download mode consumes CPU-visible bytes even when the decoder outputs
`GLMemory` and the final allocator is PBO-backed. DMA-BUF delivery to a fake sink does not
establish compositor import, fence correctness, end-to-end zero-copy or lower energy use.

## Capturing the first frame

CPU-bound modes accept `--first-frame /path/to/new-frame.png`. Capture maps the first
frame using GStreamer's video-frame API, honoring buffer offsets and row strides, and
copies only active RGBA pixels. The report includes capture dimensions, caps and PTS.
The bounded extra copy happens during startup; PNG encoding happens after playback stops.
The PNG contains the converted RGBA bytes without a color-profile transformation. It
does not establish how a compositor or display will interpret them.

The path must not exist; existing files and symlinks are never overwritten. Capture
failure fails the run. Do not enable capture for performance comparisons because it adds
startup work and up to 256 MiB of retained pixel data. DMA-BUF mode rejects this option
before initializing media libraries, preserving its unmapped scope.

## Regression checks

Using the original two-second VP9 fixture from [validation](validation.md):

```sh
cargo build -p openwall --example video_transfer_probe --locked
python3 scripts/test_video_transfer_probe.py --software-gl --video-fixture /tmp/openwall-loop.webm
```

The optional GL check uses Mesa's EGL vendor configuration and software rendering, with
desktop display variables removed and a private GStreamer registry. Tests cover real RGBA
buffers, decoder caps, looping, CPU accounting, quoted filenames, invalid media, the
surfaceless guard and SIGINT cleanup. Linux CI runs these checks with GStreamer 1.24/Mesa;
local tests also exercised GStreamer 1.26. The example compiles to an unavailable-platform
error outside Linux/video builds. These tests do not qualify hardware rendering or desktop
presentation, and are not a content sandbox.
