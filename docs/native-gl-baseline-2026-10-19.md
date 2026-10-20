# Exploratory native GL video process measurements: 2026-10-19

Three release-build trials exercise the same native presentation path as the private
animated/color diagnostics. These are process observations in a headless compositor,
not a desktop power budget or a comparison with another wallpaper application.

## Conditions and provenance

- Fedora 43, Linux 7.2.5-100.fc43.x86_64, Ryzen 5 7600X.
- Private Sway 1.11/wlroots 0.19.3, one 1920×1080 output selected for video and an
  unchanged second headless output. No desktop display/socket/bus was used.
- RTX 4070 Ti SUPER, NVIDIA 580.178.04, EGL Wayland 1.1.21, desktop OpenGL 3.3,
  GStreamer 1.26.11, Rust 1.99.0. The existing render node/driver was selected only
  for the child processes. Host driver configuration was unchanged.
- An original fully tagged BT.709 limited-range 1920×1080 VP9/WebM, 30 fps, two seconds,
  looped without decoder replacement. Encoder input and output both carried the tags.
- Five seconds of warm-up after actual presentation/input readiness, followed by a
  30-second process sample, repeated three times. No screenshots during timed intervals.
- Renderer/accounting source: `6735293`. The diagnostic uses the application-owned
  Wayland EGL session and GStreamer shared-texture source; production playback is unchanged.

Release binary SHA-256: `ad1a06bcfb53b990a673abde6687da746cb1a80846fa6b953f00de048bf47fd0`.
Media SHA-256: `056cee3614557e8610089fdfd9f3e707d5f0a973ebc5b44ab700c410bc197394`.
Binary size: 2,216,432 bytes; media size: 746,922 bytes.

The private environment selects `GST_PLUGIN_FEATURE_RANK=nvvp9dec:MAX`,
`GST_GL_WINDOW=surfaceless`, `GST_GL_PLATFORM=egl`, `GST_GL_API=opengl3`,
`EGL_PLATFORM=wayland`, `LIBGL_ALWAYS_SOFTWARE=0` and the existing NVIDIA GLVND vendor.
Each benchmark session uses a private GStreamer registry. Decoder diagnostics use
`GST_DEBUG=nvdecoder:5`; this logging and the diagnostic's validation/accounting costs
are included in process CPU. Desktop/other system activity and GPU power state were
not controlled; software correctness tests ran separately on the same CPU during part
of the overall experiment.

## Observations

CPU is a percentage of one core across all threads in the video process. RSS is sampled
every 250 ms; it excludes other processes and GPU/device allocations.

| Trial | CPU | Mean sampled RSS | Peak sampled RSS | Presentation feedbacks / 30 s |
| --- | ---: | ---: | ---: | ---: |
| 1 | 8.597% | 365.60 MiB | 365.64 MiB | 898 |
| 2 | 8.515% | 305.07 MiB | 305.20 MiB | 898 |
| 3 | 8.449% | 305.02 MiB | 305.16 MiB | 898 |

Median CPU: **8.515%** of one core. Each approximately 30.00006-second interval received
about **29.933 presentation feedbacks/s**, 898 source deliveries and 898 submissions,
with no delivery-slot replacements or discarded feedbacks. Each interval completed
15 loops. The sampling-window counts can differ from ideal 30 fps because of looping,
in-flight boundary work and feedback delivery; they are not physical dropped-frame counts.

Each whole run submitted 1,050 consumer fences, retired all 1,050, measured peak one
in flight and performed zero cleanup `glFinish` calls. Total playback completed 17 loops.
All native surfaces were removed, one UI context/display owner was destroyed per trial,
and the other output stayed unchanged. Frame records remained capped at 32 and retained
presentation identities at 256 while counters continued beyond those limits.

The actual `nvvp9dec` output was GL-memory NV12; the consumer received GL-memory RGBA
with shared-context and producer-sync metadata. Logs selected GL transfer without a
system-memory fallback. The application did not CPU-map/read back source pixels or
create an off-screen consumer target. This does not prove zero copies, identify every
driver transfer or provide a CUDA trace.

The first trial's roughly 60 MiB higher RSS was stable through its sample but has not
been explained. These three trials do not establish a memory ceiling or long-duration
leak behavior. Compositor CPU, GPU/driver work and energy are outside the metrics. The
earlier [desktop/sink baseline](video-baseline-2026-10-18.md) has different presentation
and accounting conditions and is not an equivalent comparison. Physical displays,
higher resolution/refresh, multiple sources/outputs and native playback baselines
remain required before an efficiency/support claim.
