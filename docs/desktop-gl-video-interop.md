# Desktop OpenGL video interop qualification

The owned EGL diagnostics accept `--gl-api desktop` for core OpenGL 3.3, alongside the
original `--gl-api gles` default. These are diagnostic switches; production playback is
unchanged. One shared API configuration specifies the EGL binding, renderable bit and
context attributes. Shader headers match the actual profile, and the off-screen target
uses allocation available in core 3.3 rather than assuming OpenGL 4.2 texture storage.

```sh
cargo build -p openwall --examples --features video-gl-probe --locked
python3 scripts/test_video_color.py --software-gl \
  --owned-gl-binary target/debug/examples/owned_gl_video_probe --owned-gl-api desktop
python3 scripts/test_owned_gl_sequence.py --gl-api desktop
python3 scripts/test_owned_egl_wayland.py --gl-api desktop
```

The same four SDR matrix/range fixtures, paced/unpaced 24-frame fence/reuse checks and
private native-surface pixel/input/lifecycle suite exercise both profiles. The source
sample remains retained until its consumer fence signals, with no normal-path consumer
`glFinish` and no intermediate sequence readbacks. First decoder-buffer probes record
actual allocator types and caps without mapping memory, remove themselves after capture,
and bound the number of recorded decoders/memory entries.

## NVIDIA's decoder transfer boundary

GStreamer 1.26.11's [NVDEC implementation](https://github.com/GStreamer/gstreamer/blob/1.26.11/subprojects/gst-plugins-bad/sys/nvcodec/gstnvdecoder.cpp)
accepts desktop OpenGL/OpenGL3 for its GL output path and requires a CUDA-compatible
context. The GL copy transfers decoded device data into a registered GL PBO using a
device-to-device copy, followed by a PBO-to-texture transfer. Its system path maps the
video frame for CPU writing and uses a device-to-host copy instead. A failed GL copy can
also fall back to that system path after caps have already been established.

The hardware check is explicit and windowless; it requires an existing NVIDIA driver and
the installed `nvvp9dec` plugin. It installs or bundles no vendor SDK/driver, and is not
part of software-only CI or a requirement for the application:

```sh
python3 scripts/test_nvidia_gl_interop.py --nvidia
python3 scripts/test_owned_gl_sequence.py --gl-api desktop --nvidia
```

Original lossless 260×136 VP9 fixtures satisfy the decoder's minimum dimensions. Their
YUV bytes are independently verified after encoding. The test forces NVIDIA decode,
checks actual source memory and the negotiated contract, and enables native decoder
debug logs. Desktop GL must positively report GL output and produce no system-fallback
warning; a changed/missing logging contract fails qualification. For exactly 1.26.11,
the GLES case must expose its known system fallback. This is source/log-based path
evidence, not a driver-level memory-transfer trace or proof of zero copies.

Local NVIDIA RTX 4070 Ti SUPER, driver 580.178.04, GStreamer 1.26.11, 2026-10-18:

| Input | GLES 3.2: max channel error | Core GL 3.3: max channel error |
| --- | ---: | ---: |
| BT.709 limited | 0 | 0 |
| BT.709 full | 1 | 0 |
| BT.601 limited | 1 | 0 |
| BT.601 full | 1 | 0 |

Both profiles exposed two `GLMemoryPBO` blocks at the decoder output. GLES nevertheless
negotiated the system-memory NV12 contract and logged the CPU fallback. Desktop GL
negotiated `memory:GLMemory` and logged GL output without an observed fallback warning.
That demonstrates why neither a GL-looking allocator nor caps alone establish the
transfer path. Each successful single-frame consumer retired its fence, performed zero
source CPU maps and made one explicit diagnostic snapshot readback. Errors above are
8-bit channel levels at patch interiors; they do not qualify chroma edges or display color.

The same hardware also passed a 24-frame 260×136 sequence with a unique marker per
frame. Unpaced delivery reused five producer textures and reached the three-sample
in-flight limit; paced delivery reused three textures and peaked at two samples. Both
retired all 24 fences, performed no consumer finishes, reported GL output without an
observed native fallback, and matched every sampled channel exactly. First/final readbacks
also verified that the earliest frame remained intact while later buffers were reused.
Early EOS and SIGINT failed cleanly. This is a bounded reuse test, not sustained playback.

These results make desktop GL the preferred API for the next Linux video presenter
experiment. GLES remains a useful portability comparison and fallback investigation;
the normal native sink is still the production implementation. Sustained hardware
playback, other codecs/decoders, cross-GPU copies, driver/compositor import, presentation
timing, power and sustained CPU/energy measurements remain open. The GPU path contains
device copies and synchronization, so it must never be described as zero-copy or assumed
to be cheaper without measurement. macOS continues to use its native AVFoundation path.
