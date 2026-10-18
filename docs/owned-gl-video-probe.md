# Owned EGL/shared-texture proof

The experimental Linux `owned_gl_video_probe` draws a bounded sequence of GStreamer GL
textures into an application-owned framebuffer. It creates no desktop window or wallpaper. This tests
the context-sharing boundary needed before an OpenWall-owned video presenter can control
its own Wayland surface, input region and presentation feedback.

```sh
cargo build -p openwall --example owned_gl_video_probe --features video-gl-probe --locked
GST_GL_WINDOW=surfaceless GST_GL_PLATFORM=egl GST_GL_API=gles2 EGL_PLATFORM=surfaceless \
  target/debug/examples/owned_gl_video_probe /path/to/original.webm --first-frame /tmp/new-frame.png
```

By default it draws one frame. `--frames 24 --atlas /tmp/new-atlas.png` captures the first
24 frames as a vertical strip, with the earliest frame at the top. The limit is 120;
multi-frame runs require an atlas path. The video must contain the requested number of
frames. `--paced` delivers frames on GStreamer's media clock; it does not add presentation
feedback or qualify playback timing.

It needs system EGL with GLES 3, GStreamer GL plugins and GStreamer 1.26 or newer. The
feature is opt-in; normal workers do not load the new GL dependencies. The older runtime
is rejected because its [full-range conversion is not qualified](video-color.md).
The output PNG must not exist; existing files and symlinks are never overwritten.
Use an external process timeout when testing: the 15-second sample deadline cannot bound
a blocked native driver call or teardown.

## Ownership and synchronization

Rust owns a GLES context and a 1×1 pbuffer on a surfaceless EGL display. A wrapped
`GstGLContext` and the same `GstGLDisplay` are supplied to the pipeline before playback.
GStreamer creates its producer context in that share group. The supplied sink bin is
`glupload → glcolorconvert → RGBA GLMemory/2D caps → bounded appsink`; playbin's implicit
conversion is disabled. Media paths go through the URI API, never pipeline text.

The consumer checks the actual GL allocator, texture target/dimensions, context sharing,
texture visibility and producer synchronization metadata. It calls
[`GstGLSyncMeta.wait`](https://gstreamer.freedesktop.org/documentation/gl/gstglsyncmeta.html#gst_gl_sync_meta_wait)
before drawing. It never maps the source buffer into CPU memory. An independent sampler
avoids changing the producer's texture parameters. Only its own GL objects are deleted.
The EGL context stays on its creating thread and outlives the pipeline and GL objects.

After each draw the consumer inserts an OpenGL fence and flushes its command stream.
At most three consumer samples remain leased; a sample is released only after its fence
signals. Nonblocking retirement checks completed fences; at capacity, a blocking native
wait sleeps for up to one millisecond at a time, with a five-second deadline and interrupt
checks. An incomplete lease finishes its pending GPU work during error cleanup. Successful
retirement performs no consumer `glFinish`. The appsink queue is separately limited to
one sample; this is not a bound on every internal decoder/driver allocation.

One explicit `glReadPixels` captures the first frame; a second reads the complete atlas
after the sequence finishes. There are no intermediate readbacks. Each draw goes to its
own region, so a final pixel check can detect incorrect reuse in earlier frames. Atlas
dimensions are limited to 16,384 per side and 256 MiB of RGBA storage; the first-frame CPU
snapshot and driver allocations are additional. Unsupported crop/affine metadata,
interlaced or multiview frames, non-RGBA data, non-2D textures and changing frame dimensions
fail explicitly. Interruption or early EOS during frame collection publishes no
successful report or capture.

## Evidence and remaining gates

```sh
cargo build -p openwall --example video_transfer_probe --locked
python3 scripts/test_video_color.py --software-gl \
  --owned-gl-binary target/debug/examples/owned_gl_video_probe
python3 scripts/test_owned_gl_sequence.py
```

The original asymmetric BT.601/BT.709 limited/full-range fixtures check first-frame PTS,
orientation, dimensions, opaque alpha and independent color equations. Local GStreamer
1.26.11/Mesa 25.3.6 software GL matched all sampled channels exactly. The suite also
checks quoted paths, invalid media, the surfaceless guard and preservation of existing
files/symlinks. Ubuntu 26.04 CI exercises the same path; Ubuntu 24.04 checks the version
rejection. Default/macOS builds retain an unavailable-platform fallback.

The separate 24-frame original sequence changes both its patch positions and a unique
gray marker every frame. Its encoded YUV bytes are independently verified before testing.
Both paced and unpaced runs compare every frame's atlas region against the reference and
check PTS order, actual producer texture reuse, submitted/retired fence counts, the
three-sample bound and zero successful-path consumer finishes. Local Mesa 25.3.6/GStreamer
1.26.11 runs reused three textures across 24 frames, peaked at two in-flight leases and
matched every sampled channel exactly. Early EOS and SIGINT must fail without captures.

A separate windowless 1080p draw succeeded on NVIDIA 580.178.04/GLES 3.2. Its selected
`nvvp9dec` delivered system-memory NV12 before upload, so it does not establish resident
hardware decoding. `GstGLMemoryPBOAllocator` at the consumer still represents a GL texture;
this path does not map the PBO. The separate [CPU-consumption measurement](video-baseline-2026-10-18.md)
shows why mapping deferred readback must be accounted for when a CPU consumer is required.

This is a bounded interoperability and buffer-reuse check. It does not qualify sustained
playback, looping, hardware color accuracy, HDR, display color interpretation,
compositor import, input behavior, resize/hotplug or energy use. It changes no production
renderer. A separate [owned EGL Wayland surface proof](owned-egl-wayland-probe.md) checks
private compositor pixels, click-through and teardown. Combining those two boundaries,
clock/presentation scheduling and sustained hardware measurements remain open. The
verification atlas is a test output, not the production presenter's allocation strategy.
