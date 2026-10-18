# App-local GStreamer Wayland output-change fix

GStreamer's 1.28 Wayland helper registers a `GstWlOutput` on its first `wl_output.done`
event by transferring it into the display's output table. That callback steals its
temporary `display` pointer. A second `done` event after a monitor scale/mode change
uses the now-null pointer and crashes in `g_mutex_lock`. OpenWall reproduced this with
Ubuntu 26.04's GStreamer 1.28.2, a private Sway session and a native GDB backtrace.
The same callback remains in upstream 1.28.8 source.

The included LGPL patch returns early on subsequent registration callbacks. Geometry,
mode and scale events still update the existing object in place. Registering it again
would also release the table's existing ownership of the same object. The patch changes
no public ABI and does not depend on private structure access from Rust.

This is a local patch prepared for upstream contribution; it has not been submitted
upstream by this task. Sources:
[1.28.2](https://github.com/GStreamer/gstreamer/blob/1.28.2/subprojects/gst-plugins-bad/gst-libs/gst/wayland/gstwldisplay.c),
[1.28.8](https://github.com/GStreamer/gstreamer/blob/1.28.8/subprojects/gst-plugins-bad/gst-libs/gst/wayland/gstwldisplay.c).

## Build

On a Linux GStreamer 1.28 development host, build the worker first, then prepare its
runtime. On Ubuntu the additional tools are `meson ninja-build patch wayland-protocols
libdrm-dev`, alongside the normal C compiler, Wayland and GStreamer development packages.

```sh
cargo build -p openwall --locked
python3 scripts/build_wayland_runtime.py --binary target/debug/openwall
python3 scripts/test_wayland_runtime.py --expect-bundled
```

Use `--binary target/release/openwall` for a release build. `--source-archive` accepts an
offline copy of the exact pinned archive; it must pass the same SHA-256 check.
`--output` selects a packaging staging directory. Existing runtime directories are not
overwritten; move one aside before rebuilding.

The build downloads the pinned 1.28.8 release archive, verifies its hash, applies the
patch without fuzz, disables automatic features/network dependency downloads, and builds
only the Wayland helper target. It installs nothing system-wide. No codecs, proprietary
components or alternative GStreamer core are built. Build for the target distribution's
native dependencies; this is not a universal Linux binary.

The worker selects the helper only from
`<worker-directory>/openwall-runtime/gstreamer-1.28/libgstwayland-1.0.so.0` when running
with the 1.28 core ABI. It loads it before GStreamer initialization and retains it for
the process lifetime, so the native sink resolves the app-local helper. No process
environment or system plugin path is changed. Wallpaper paths cannot select a library.

Without that helper, known affected waylandsink versions 1.28.0–1.28.8 are rejected
before creating desktop surfaces. `openwall doctor` reports runtime availability and
the reason. GStreamer 1.24/1.26 continue to use the system helper. Availability does not
establish hardware, color or presentation qualification. Later releases need validation;
the guard's upper bound is a known affected range, not a claim that 1.28.9 fixes it.

## Source and licensing

The built Wayland helper and this patch use **LGPL-2.1-or-later**. The app-local directory
includes the unmodified complete source archive, patch, COPYING, build script, instructions
and provenance manifest. Other source files in the archive retain their own licenses.
The manifest records source, patch and library hashes. Keep those files when packaging
this runtime; distribute corresponding source alongside any binary release. The native
library can be replaced with an ABI-compatible modified build. OpenWall's original Rust
application code remains AGPL-3.0-or-later.

To rebuild an included source bundle, move the installed runtime directory aside, then
run its included build script with explicit `--binary` and `--source-archive` paths.
The script can find the patch beside itself outside a repository checkout.
