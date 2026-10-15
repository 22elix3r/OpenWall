# Validation and evidence

## Automated checks

- Asset traversal, symlink escape, package entry allocation limits and archive paths.
- Raw TEX fixture colors; equivalent loose/packed scene pixels.
- Unsupported code/effects and non-finite/malformed transforms.
- CLI process success/failure contracts and JSON reports.
- Opt-in Vulkan/Metal output comparison with original reference artwork.
- Qt QML startup via `openwall-ui --check` with nonzero exit on root creation failure.
- Linux/macOS CI build, tests and Clippy. Hosted software Vulkan is correctness evidence only.

Regenerate original samples with `python3 scripts/generate_samples.py`. Generation must
be deterministic. Do not commit Workshop downloads or original engine shared assets as
test fixtures without redistribution permission.

## Local session: 2026-10-15

Fedora 43, Hyprland, Qt 6.10.3, GStreamer 1.26.11, 2560×1440 display at integer scale 1.
The Vulkan adapter reports RADV RAPHAEL_MENDOCINO. Actual GUI and rendered artwork were
visually inspected. The Qt GUI was also started with the offscreen platform.

The original Aurora scene rendered to PNG and was presented as a layer-shell background.
A generated two-second VP9/WebM test video ran for seven seconds and selected
`nvvp9dec`. Decoder choice does not establish that end-to-end presentation is zero-copy.
On this particular decoder/sink path the first sink buffer was RGBx system memory
(`DMA-BUF=false`). Hardware decoding therefore does not eliminate the frame-transfer
cost. Efficient GPU-resident video presentation remains an open M0 gate. Worker
diagnostics expose first-buffer sink caps and DMA-BUF status for further checks.

A three-second idle sample of the static scene recorded 0.0% of one CPU core and
127.3 MiB RSS in the development build. SIGINT cleanup passed, including Hyprland's
closing-surface animation. This short sample does not establish a sustained CPU/energy
budget or video performance.

Run `python3 scripts/smoke_wayland.py` in a Hyprland session to test owned surface
creation, SIGINT cleanup, and an idle CPU sample. It refuses to interfere with an already
running OpenWall surface. The script changes no desktop settings.

For the native video regression, generate an original short fixture and opt in:

```sh
ffmpeg -f lavfi -i testsrc2=size=640x360:rate=30 -t 2 -an -c:v libvpx-vp9 -b:v 400k /tmp/openwall-loop.webm
python3 scripts/smoke_wayland.py --video-fixture /tmp/openwall-loop.webm
```

This requires at least two loops, a buffer at the native sink, clean surface shutdown,
and no scene-GPU initialization in the native video worker.

## Required before support claims

- Real macOS video/scene desktop placement, Spaces, Mission Control, Stage Manager,
  Finder icons, fullscreen behavior and native app activation.
- Multiple monitors, fractional/mixed scaling, output rotations, mixed refresh,
  hot-plug and display sleep/wake.
- GNOME and Plasma integration proofs and version-specific lifecycle testing.
- Driver/device combinations, external-memory synchronization and failure recovery.
- Playback power/visibility policy, GPU device loss and bounded worker restarts.
- Screen readers, keyboard navigation and reduced motion.
- Sustained CPU/GPU/energy/memory measurements against native playback baselines.
- Black-box compatibility comparisons against Wallpaper Engine on an owned reference system.

These gates remain open. Process separation is implemented; OS sandboxing is not yet
implemented. Do not treat this development snapshot as a hardened untrusted-content runtime.
