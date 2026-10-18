# Owned EGL/shared-texture proof

The experimental Linux `owned_gl_video_probe` draws one GStreamer GL texture into an
application-owned framebuffer. It creates no desktop window or wallpaper. This tests
the context-sharing boundary needed before an OpenWall-owned video presenter can control
its own Wayland surface, input region and presentation feedback.

```sh
cargo build -p openwall --example owned_gl_video_probe --features video-gl-probe --locked
GST_GL_WINDOW=surfaceless GST_GL_PLATFORM=egl GST_GL_API=gles2 EGL_PLATFORM=surfaceless \
  target/debug/examples/owned_gl_video_probe /path/to/original.webm --first-frame /tmp/new-frame.png
```

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

One explicit `glReadPixels` produces the diagnostic PNG. The consumer finishes its work
before releasing the source sample back to GStreamer's pool, including error cleanup.
This single-frame proof deliberately uses `glFinish`; a continuous presenter needs
bounded asynchronous fence retirement and must not finish the entire GL stream per frame.
Frame/capture allocation is bounded to 256 MiB. Unsupported crop/affine metadata,
interlaced or multiview frames, non-RGBA data and non-2D textures fail explicitly.

## Evidence and remaining gates

```sh
cargo build -p openwall --example video_transfer_probe --locked
python3 scripts/test_video_color.py --software-gl \
  --owned-gl-binary target/debug/examples/owned_gl_video_probe
```

The original asymmetric BT.601/BT.709 limited/full-range fixtures check first-frame PTS,
orientation, dimensions, opaque alpha and independent color equations. Local GStreamer
1.26.11/Mesa 25.3.6 software GL matched all sampled channels exactly. The suite also
checks quoted paths, invalid media, the surfaceless guard and preservation of existing
files/symlinks. Ubuntu 26.04 CI exercises the same path; Ubuntu 24.04 checks the version
rejection. Default/macOS builds retain an unavailable-platform fallback.

A separate windowless 1080p draw succeeded on NVIDIA 580.178.04/GLES 3.2. Its selected
`nvvp9dec` delivered system-memory NV12 before upload, so it does not establish resident
hardware decoding. `GstGLMemoryPBOAllocator` at the consumer still represents a GL texture;
this path does not map the PBO. The separate [CPU-consumption measurement](video-baseline-2026-10-18.md)
shows why mapping deferred readback must be accounted for when a CPU consumer is required.

This is a single-frame interoperability check. It does not qualify continuous buffer
reuse, timing/looping, hardware color accuracy, HDR, display color interpretation,
compositor import, input behavior, resize/hotplug or energy use. It changes no production
renderer. The next gate is a bounded continuous texture consumer with asynchronous
producer/consumer synchronization, followed by an owned EGL Wayland surface and private
compositor pixel, click-through and teardown checks.
