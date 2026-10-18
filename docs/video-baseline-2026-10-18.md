# Exploratory 1080p30 video measurements: 2026-10-18

These are worker CPU/memory observations on one development desktop. They do not qualify
energy use, dropped frames, end-to-end GPU-resident presentation or general hardware support.

## Conditions

- Fedora 43, Linux 7.2.5-100.fc43.x86_64, Hyprland 0.54.3.
- Ryzen 5 7600X with Raphael integrated graphics and NVIDIA RTX 4070 Ti SUPER.
- Mesa 25.3.6, NVIDIA 580.178.04, GStreamer 1.26.11, Rust 1.99.0.
- One DP-4 output at 2560×1440, scale 1, approximately 179.877 Hz.
- Original two-second 1920×1080 VP9/WebM at 30 fps, generated with `testsrc2` and 4 Mbit/s
  target bitrate using the command in the [measurement guide](video-performance.md).
- Release worker production sources at `0853b98`, measurement harness at `68a97eb`.
  These runs preceded the new XDG video parent/memfd changes; future comparisons must
  rebuild the selected revision rather than assume these results describe current main.
- Five-second warm-up followed by three separate 30-second samples per decoder policy.
  Every run looped 17 times across startup/warm-up/sampling and stopped cleanly.

Binary SHA-256: `963e8588c854cdd71c256e75a65cbf1972a48c14589858f898a026ff7e3495b9`.
Media SHA-256: `dd12b2f635c5a4e58aa0c1c4feef99175813710d270c701eca7ec47c3a065d8a`.
All nine reports used those same hashes. The desktop was otherwise used normally;
foreground activity, GPU power state and energy sensors were not controlled.

## Observations

CPU is a percentage of one core. Memory is the range of per-run peak sampled RSS in MiB;
it excludes other processes and does not account for GPU/device memory.

| Decoder policy | Selected decoder | Median CPU | CPU range | Peak sampled RSS range |
| --- | --- | ---: | ---: | ---: |
| Inherited automatic policy | `nvvp9dec` | 13.827% | 13.693–14.059% | 311.01–312.06 |
| `GST_PLUGIN_FEATURE_RANK=vp9dec:MAX` | `vp9dec` | 19.585% | 18.521–20.080% | 71.61–72.93 |
| Private Mesa VA test environment | `vavp9dec` | 13.457% | 13.455–13.854% | 148.44–151.37 |

The Mesa test removed `LIBVA_DRIVER_NAME` only for its child process, used a fresh
temporary GStreamer registry and set `GST_PLUGIN_FEATURE_RANK=vavp9dec:MAX`. The inherited
driver override otherwise prevented GStreamer's VA plugin from registering factories.
No host driver configuration was changed. All paths delivered system-memory RGBx to
the Wayland sink. Decoder choice alone did not close the video transfer gate.

## Follow-up probes

A surfaceless GStreamer test negotiated `DMA_DRM` with a NVIDIA tiled RGB modifier, but
GStreamer 1.26.11's EGL export helper rejected the non-linear buffer and `gldownload`
fell back to system memory. Forcing a DMA-BUF-only downstream filter then failed
negotiation. A caps agreement is therefore insufficient evidence of successful export.
The relevant upstream paths are [GL download](https://github.com/GStreamer/gstreamer/blob/1.26.11/subprojects/gst-plugins-base/ext/gl/gstgldownloadelement.c)
and [EGL image export](https://github.com/GStreamer/gstreamer/blob/1.26.11/subprojects/gst-plugins-base/gst-libs/gst/gl/egl/gsteglimage.c).

A separate, single-run surfaceless comparison used NVIDIA decode, RGBA output and a
clock-paced fake sink. After five seconds of warm-up, CPU conversion used 12.639% of
one core over 30 seconds; GL conversion followed by system-memory download used 8.713%.
The GL path kept decoder output in `GLMemory` but still downloaded the final RGBA frame.
These were temporary diagnostic pipelines, without Wayland presentation, pixel/color
conformance or repeated measurements. They suggest a direction for investigation;
they do not justify changing the default renderer or claiming GPU-resident playback.

Before selecting a GL presenter, verify pixel/color fidelity, default input regions of
its embedded subsurfaces, resize/teardown and hardware buffer synchronization. Retain
the compositor pixel/lifecycle tests and compare sustained energy against equivalent
native playback. Other resolutions, refresh rates, codecs and GPUs remain unmeasured.
