# Measuring native Linux video

`scripts/benchmark_video.py` measures one Linux layer-shell worker with a warm-up interval
and a bounded sample interval. It records startup to the first decoded sink buffer,
CPU time as a percentage of one core, mean/peak sampled RSS, decoder factories, negotiated
sink caps, first-buffer DMA-BUF diagnostics, loops and shutdown outcome. The JSON report
also contains binary/media hashes and selected driver/plugin environment variables.
Only the last 16 KiB of worker output is retained; continuous noise cannot prevent the
startup deadline or shutdown escalation. The tool changes no desktop settings.

Use a release build and original, redistributable test media. Stop other OpenWall workers
first to avoid overlapping backgrounds. Choose one connector from `openwall outputs`.
Run on a layer-shell desktop; normal GNOME/Plasma Apply remains guarded.

```sh
cargo build -p openwall --release --locked
ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 2 -an -c:v libvpx-vp9 -b:v 4M /tmp/openwall-1080p30.webm
python3 scripts/benchmark_video.py /tmp/openwall-1080p30.webm --output DP-1 --seconds 60 > /tmp/openwall-video-auto.json
```

The report is local stdout; nothing is uploaded. Repeat the same build/content/output
three times, keeping monitor scale, refresh, desktop activity and power state constant.
Use `--binary target/debug/openwall` only for correctness checks; development-build
numbers are not release performance baselines. `--warmup` defaults to five seconds and
`--startup-timeout` to fifteen seconds. A premature exit, missing sink buffer or forced
shutdown fails the run and returns a nonzero exit code. It cannot validate displayed
pixels, dropped frames, compositor/GPU energy or allocations in other processes.

## Decoder and transfer evidence

Use the [windowless Rust transfer probe](video-transfer-probe.md) to isolate decoding,
conversion and memory negotiation from the desktop presenter. Its fake-sink measurements
have a different scope from this full-worker benchmark.

Record the host's GStreamer, kernel, compositor and GPU/driver versions alongside each
report. Change decoder ranks or driver variables only for the test process. Use a fresh
temporary `GST_REGISTRY_1_0` when testing a different driver environment, since available
hardware factories are discovered during plugin registration. The tool preserves the
caller environment and never overrides driver policy automatically.

On the development Fedora 43 mixed AMD/NVIDIA system, the inherited `LIBVA_DRIVER_NAME`
forced the NVIDIA VA driver even on the AMD render node, and GStreamer's VA plugin
registered no factories. Removing that variable only for a private test process and
using a fresh registry exposed Mesa's AMD `vavp9dec`. Both that decoder and the default
`nvvp9dec` still delivered system-memory RGBx to the native sink. The AMD decoder
advertised NV12/P010 DMA-BUFs with an AMD modifier; this compositor advertised NVIDIA RGB
modifiers and no NV12. Hardware decoding alone therefore did not close the transfer gate.
No host driver configuration was changed.

A first-buffer `dmabuf: true` records the existing worker diagnostic at the sink; it does
not prove that every frame/plane uses DMA-BUF, that the sink avoids copies, or that the
compositor imports without another transfer. A sink buffer is also weaker than confirmed
presentation. Inspect both decoder/sink negotiation and actual buffer import before
claiming an end-to-end GPU-resident path. Never force an unsupported DRM modifier.

Relevant upstream references:

- [GStreamer DMA-BUF negotiation and modifiers](https://gstreamer.freedesktop.org/documentation/additional/design/dmabuf.html).
- [Wayland sink capabilities](https://gstreamer.freedesktop.org/documentation/waylandsink/index.html).
- [VA VP9 decoder](https://gstreamer.freedesktop.org/documentation/va/vavp9dec.html).

CI checks the measurement harness with disposable busy/noisy/unresponsive processes.
The private Sway suite also measures a real video worker and checks clean surface removal.
Those tests establish accounting and timeout behavior, not desktop performance. Hardware
qualification still needs sustained runs, visible-pixel checks and an equivalent native
playback baseline at representative 1080p/4K rates.

See the [2026-10-18 exploratory release measurements](video-baseline-2026-10-18.md)
for the tested revisions, hashes, conditions and remaining limits.

## Native shared-GL process measurements

`scripts/benchmark_native_gl_video.py` runs the optional native video diagnostic in its
own headless Sway session. It does not connect to a desktop display or bus. The selected
private output defaults to 1920×1080; the other output must stay unchanged. Native
surface removal is checked after every trial. Mesa software GL is the default, while
`--nvidia` explicitly selects an already installed NVIDIA render node/driver and requires
desktop GL plus decoder GL-memory/caps/log evidence. It changes no driver configuration.

```sh
cargo build -p openwall --release --example native_gl_video_probe --features video-gl-probe --locked
python3 scripts/benchmark_native_gl_video.py /tmp/original-tagged-1080p30.webm --nvidia > /tmp/native-gl.json
```

The probe's `--measure-seconds` mode allows 1–120 measured seconds after 0–30 warm-up
seconds, starting only after actual presentation feedback and click-through readiness.
The harness defaults to three 30-second trials with five-second warm-ups. It records
binary/media hashes, platform/compositor/GL/GStreamer identity, negotiated source memory,
process CPU time across all native threads, RSS sampled every 250 ms, submissions,
presentation/discard feedback, sample delivery/replacement and loops. CPU uses the
process CPU clock and a percentage of one core, which can exceed 100 on multiple threads.
Reported memory excludes GPU/device memory and other processes; its peak is sampled RSS,
not the process's all-time high-water mark.

Frame IDs keep increasing while retained frame records are capped at 32 and presentation
identities at 256. Counters and fences continue beyond those limits. This avoids the
normal diagnostic's 240-record stop and does not serialize per-frame JSON indefinitely.
The normal finite/color diagnostic retains its original histories and behavior. An
incomplete measurement fails instead of reporting a successful partial interval.
`--check-interruption` additionally checks argument conflicts, the private launch guard,
intentional early SIGINT rejection and native cleanup; CI uses this with short trials on
both APIs. CI numbers are correctness checks, not release performance baselines.

No screenshots are taken during the timed interval. Independent animated/color tests
qualify the same drawing path separately. Presentation feedback counts are events received
within the sampling window, not a physical-display dropped-frame metric; counts near a
boundary can differ by an in-flight frame. The measurements exclude compositor CPU, GPU
work/energy, desktop roles and physical scanout. Driver/GStreamer pools and diagnostic
validation/logging are included in the video process. Headless runs cannot establish
wallpaper power use or be directly compared with the earlier desktop/sink benchmarks.

The [2026-10-19 native GL release observations](native-gl-baseline-2026-10-19.md) record
three private 1080p30 NVIDIA trials, exact source/binary/media provenance and scope limits.
