# Validation and evidence

## Automated checks

- Asset traversal, symlink escape, package entry allocation limits and archive paths.
- Raw TEX fixture colors; equivalent loose/packed scene pixels.
- Unsupported code/effects and non-finite/malformed transforms.
- CLI process success/failure contracts and JSON reports.
- Opt-in Vulkan/Metal output comparison with original reference artwork.
- GPU checks for layer position, rotation, and alpha blending in linear light.
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

## Isolated compositor regression: 2026-10-18

The private headless Sway 1.11 session, with its pixman renderer and Mesa lavapipe
Vulkan client, passed these checks without touching the running Hyprland session:

- Simultaneous 1× and 1.25× output scaling, followed by 1.5× and 2× scale changes.
- Correct physical buffer dimensions, upright quadrant pixels, and 90-degree output rotation.
- Output disable/enable, a newly created output, and disappearance/recovery of all outputs.
- Selected-output placement and an idle static scene with no additional presentations.
- Native video pixels and looping before/after logical rectangle resizing, without
  constructing a new decoder or initializing the scene GPU.
- Surface removal after SIGINT/SIGKILL, and prompt worker failure on compositor death.

The script creates a temporary runtime directory, Wayland socket, IPC socket and
compositor configuration. Every display command and screenshot targets that private
session. It generates original quadrant artwork and cleans up its own processes.
These tests are also run in Linux CI with software rendering. They exercise real
protocol objects and composited pixels, but do not measure physical GPU performance,
power savings or physical display suspend/reconnection.

Install `sway`, `grim`, Pillow and Mesa's Vulkan software driver to run:

```sh
cargo build -p openwall --locked
python3 scripts/test_wayland_headless.py --software-vulkan
```

The script discovers the distribution's generic or architecture-specific lavapipe ICD.
To include native video resizing, add
`--video-fixture /tmp/openwall-loop.webm` using the original animated fixture generated
above. GStreamer's VP9 decoder and Wayland sink plugins must be installed.

The live Fedora/Hyprland regression also passed after fixing an unchanged-scale
notification feedback loop observed there. The static scene made no additional
presentations during its three-second idle sample, measured 0.0% of one CPU core and
128.0 MiB development-build RSS. Video selected `nvvp9dec`, looped twice, used no scene
GPU and cleaned up its surfaces. Its sink still received system-memory RGBx buffers;
this change does not establish zero-copy presentation or a sustained energy budget.

Ubuntu's Sway 1.9/wlroots 0.17.1 packages were also extracted into a temporary directory
and tested on the Fedora host. They exposed an output-recovery case in which a queued
frame and frame callback preceded successful presentation. Bounded `wp_presentation`
recovery now passes the complete lifecycle/video suite there as well. Two deterministic
timer-policy tests check the retry budget and cancellation after confirmation.

GStreamer 1.24.2's Wayland sink and helper library were extracted separately to reproduce
Ubuntu's embedding behavior. Its display-context name differs from the primary name in
1.26; the shared compatibility name now works with both, and setup errors are collected
before the sink is parented into playbin. The complete suite passes with the older sink
and checks that desktop video never opens a separate application window. The one-pixel
parent optimization remains enabled.

## Experimental Plasma bridge: 2026-10-18

Fedora 43, Plasma/KWin 6.6.4 and Qt 6.10.3 passed an isolated nested-desktop test using
Mesa software Vulkan/OpenGL and shared-memory client buffers. The actual wallpaper
package was copied into temporary XDG data; a private D-Bus session activated only
ActivityManager. Neither the running Hyprland session nor its configuration was used.

- Original quadrant pixels from a Rust worker appeared inside the Plasma wallpaper.
- A repository-owned desktop widget appeared above both scenes and video and received a
  virtual pointer click through both nested desktops; its visible state changed.
- Duplicate toplevel rejection preserved the original wallpaper and host process.
- Graceful worker exit, SIGKILL, reconnection and dark fallback passed.
- Terminating the presentation host caused its connected worker to fail promptly.
- Fractional output changes and logical host resizing preserved visible scene pixels.
- Native GStreamer Wayland video appeared and looped, including after host resizing,
  without replacing the decoder or initializing a separate scene GPU.
- Video SIGKILL restored the fallback; OpenWall's native XDG geometry followed host resize.
- A wallpaper loaded in a plain preview window did not create a compositor socket.

Run the generic Qt host test with `python3 scripts/test_plasma_bridge.py`; it exercises
the actual presentation QML without requiring Plasma and is included in Linux CI.
The full desktop test is opt-in with `--plasma`; see the adapter's
[test instructions and remaining gates](../integrations/plasma/README.md).
No hardware DMA-BUF import, sustained performance, general Plasma-version compatibility,
real configuration-dialog lifecycle, screen lock or activity switching is qualified.
Normal GUI/CLI Apply on Plasma remains unavailable until its connection/lifecycle
contract is implemented and validated.

## Isolated GNOME background probe: 2026-10-18

GNOME Shell 49.10 / Mutter 49.8 / GJS 1.86.0, extracted from Fedora packages, passed
the actual repository presentation module in a private headless shell. Scene rendering
used lavapipe; bubblewrap hid GPU nodes so Mutter used its surfaceless renderer. A
read-only host filesystem, private XDG state, memory-only GSettings and private D-Bus
session kept the running desktop and real OS-service buses out of the test.

- Exact quadrant pixels under GNOME's native panel from a minimized, connection-owned
  Rust source window; an identically titled unrelated preview remained unmanaged.
- Graceful stop/fallback, mapped-worker crash, reconnect, presentation actor loss,
  superseded startup and stop during startup.
- A 256 KiB noisy worker was drained into bounded diagnostics. A worker ignoring SIGINT
  and never mapping was forcibly reaped after its startup deadline.
- Moving GStreamer VP9 video through a background clone, at least two loops, and no
  scene-GPU initialization. A software plugin set excluded host GPU-driver probing.
- Native XDG video initialization emitted no initial configure or invalid-geometry
  warnings. Invalid video exited without ever mapping a black window.
- A worker outside the test PID namespace failed promptly when its compositor died.

The launcher must explicitly close its retained copy of the Wayland socket peer after
spawning; waiting for garbage collection delays EOF and window cleanup. The test also
exposed GJS's unsupported streaming TextDecoder option and background-manager replacement
during shell startup. The probe now uses bounded non-streaming diagnostics and resolves
the background actor at map time, with worker cleanup if that actor disappears.

Run `python3 scripts/test_gnome_headless.py` on a GNOME 49 test host; see the
[runtime requirements and boundaries](../integrations/gnome/README.md). This test is
opt-in; hosted CI syntax checks do not qualify GNOME behavior. OpenWall now owns the XDG
root and isolates GStreamer's parent commits in a separate container subsurface. Both
parents use a single black pixel with viewporter, backed by anonymous memory. The generic
Qt bridge also passed with the extracted GStreamer 1.24.2 sink/helper and Sway 1.9, including
resize without decoder replacement and crash fallback. These tests do not confirm
first-frame transactional presentation. Normal app-list/overview roles, monitor/session
transitions and hardware rendering remain unqualified. Normal
GNOME Apply is still unavailable. These results establish presentation feasibility.

## Ubuntu 26.04 runtime regression: 2026-10-18

A rootless Ubuntu 26.04 container reproduced a stock GStreamer 1.28.2 SIGSEGV during
native video output resizing. GDB identified `output_done` using a display pointer that
had already been transferred on an earlier notification. The current 1.28.8 source has
the same registration logic. The [source-included app-local helper](../integrations/gstreamer/README.md)
patches only that registration boundary and leaves the system installation unchanged.

The complete private Sway 1.11 scene/video suite passes with the patched helper and the
distribution's 1.28.2 core/sink. It includes repeated mode/scale changes, looping without
decoder replacement, native pixels, cleanup, hot-plug scene handling and compositor loss.
Separate process tests verify fail-closed startup without a helper, oversized/incompatible
metadata rejection, helper selection and included-source/hash provenance. These results
do not qualify physical hardware or replace the remaining support gates.

## Required before support claims

The optional [staged EGL diagnostic](staged-egl-probe.md) now holds static native child
buffers, including resize priming swaps, behind an uncommitted parent. Private tests
require presentation feedback and real click-through after publication, unchanged sampled
pixels before publication, cancellation/crash cleanup and bounded control messages. Both
GL APIs are exercised on Mesa and explicitly selected NVIDIA hardware. This does not yet
provide transactional video or worker apply; physical scanout remains unqualified.

The combined [video diagnostic](native-gl-video-probe.md#optional-paused-video-publication)
can now preroll in PAUSED, retain PTS zero through hidden resizing, publish the corrected
buffer and start the same pipeline. Private Mesa/NVIDIA tests verify animated pixels,
presentation, GPU lease retirement and paused cancellation; general worker apply and
resizing published content still require qualification.

The [shared video output diagnostic](shared-video-outputs-probe.md) checks one decoder,
one current source sample and per-output presentation/fences on two native windows. It
requires moving video and a new playback loop on the survivor after the first native
surface is destroyed, while the shared display/context remain alive. This does not yet
replace production's per-output decoders or qualify general display management.

- Real macOS video/scene desktop placement, Spaces, Mission Control, Stage Manager,
  Finder icons, fullscreen behavior and native app activation.
- Multiple monitors, fractional/mixed scaling, output rotations, mixed refresh,
  hot-plug and display sleep/wake.
- GNOME/Plasma native app/window roles, qualified video paths and version-specific
  monitor/session lifecycle testing beyond the isolated presentation probes.
- Driver/device combinations, external-memory synchronization and failure recovery.
- Playback power/visibility policy, GPU device loss and bounded worker restarts.
- Screen readers, keyboard navigation and reduced motion.
- Sustained CPU/GPU/energy/memory measurements against native playback baselines.
- Black-box compatibility comparisons against Wallpaper Engine on an owned reference system.

These gates remain open. Process separation is implemented; OS sandboxing is not yet
implemented. Do not treat this development snapshot as a hardened untrusted-content runtime.
