# Experimental GNOME background probe

This M0 component proves out-of-process presentation on GNOME 49. It is trusted
JavaScript that displays a Rust/GStreamer worker's window texture inside GNOME's
existing background actor. Content parsing, decoding and rendering stay outside the
shell; no wallpaper JavaScript or shaders execute in GNOME Shell.

It is **not an installable extension or the normal Apply backend**. App-list, overview,
workspace, monitor and session integration are incomplete. The probe leaves its source
window minimized; that window still has a normal application role. Do not infer a
finished native desktop experience from successful background pixels.

## Repeat the proof

On Linux with GNOME Shell 49, Mutter, PyGObject, Pillow, Mesa software Vulkan/OpenGL,
D-Bus tools and bubblewrap installed:

```sh
cargo build -p openwall --locked
python3 scripts/test_gnome_headless.py --artifacts /tmp/openwall-gnome-results
```

Use `--shell /path/to/gnome-shell` for an explicit executable. `--schema-dir` adds a
directory of GSettings schema XML files to the private schema compilation. Locally
extracted runtimes may also require their normal `LD_LIBRARY_PATH`, `GI_TYPELIB_PATH`,
`GNOME_SHELL_DATADIR` and `XDG_DATA_DIRS` settings. No packages are installed by the test.

Add `--video-fixture /tmp/openwall-loop.webm` using the original VP9 fixture from
[validation](../../docs/validation.md). The test constructs a small open-source
GStreamer plugin directory for software VP9/Wayland playback; use `--gst-plugin-dir`
if the host's plugin directory needs an explicit path. Driver-specific plugin scanning
is outside this software correctness test.

The test starts its own virtual monitor, private session bus, temporary XDG directories
and memory-only GSettings. OS-service calls also target the empty private bus. A
bubblewrap namespace gives GNOME a read-only host filesystem, writable temporary test
directory and no GPU device nodes, forcing Mutter's surfaceless software renderer.
Child processes are contained in that test PID namespace. The native caller checks its
private bus/runtime paths before invoking Shell Eval; **unsafe mode is enabled only in
this private shell**. No command is sent to an existing desktop or session bus.
This test setup is not an implementation of application/content sandboxing.

The actual repository module is imported and tested for scene pixels, connection
ownership, an unrelated client with an identical title, startup cancellation, fallback,
worker crashes, presentation actor loss, video motion/loops, bounded log draining,
unresponsive-worker termination and compositor loss. The test stores only explicitly
requested artifacts. This full GNOME test is currently opt-in, not part of hosted CI.

## Boundary and remaining decisions

`Meta.WaylandClient.new_subprocess` gives the worker a dedicated Wayland socket.
`owns_window` identifies its window without title/PID heuristics. The launcher is closed
immediately after spawning: it otherwise retains the socket peer until garbage
collection and delays worker/window cleanup. A `Clutter.Clone` displays the owned,
minimized source actor under the desktop. No pixel data travels through control IPC.

The probe reads worker output in bounded chunks, retains at most 16,384 diagnostic
characters and stops a worker that never maps. Stop first requests SIGINT, then forces
exit after 1.5 seconds. Superseded startup requests do not launch additional workers.
The probe has no restart policy or durable assignments; the planned Rust manager must
own those policies and process-group containment. A map event is not a first-frame
transaction acknowledgment; pixel checks provide the current evidence.

GNOME's background manager fields are private, and current public window APIs do not
provide the complete desktop/app-list role needed here. Existing implementations use
version-sensitive shell-method overrides. This probe deliberately leaves those out
while their compatibility and interaction with other extensions are evaluated.

The native GStreamer automatic-window path displays moving video, but Mutter 49 reports
an initial configure acknowledgment warning. A supported backend must qualify or replace
that path with an OpenWall-owned XDG parent and native overlay before claiming reliable
video integration. Hardware DMA-BUF import/synchronization, power policy, screen locking,
overview/desktop icons, mixed-scale monitors, shell restarts and sleep/wake remain open.
Only GNOME Shell 49.10 / Mutter 49.8 has been exercised here; other versions are unqualified.

## References and provenance

- [Mutter Wayland client API](https://gitlab.gnome.org/GNOME/mutter/-/blob/gnome-49/src/meta/meta-wayland-client.h).
- [Mutter subprocess ownership implementation](https://gitlab.gnome.org/GNOME/mutter/-/blob/gnome-49/src/wayland/meta-wayland-client.c).
- [GNOME Shell background implementation](https://gitlab.gnome.org/GNOME/gnome-shell/-/blob/gnome-49/js/ui/background.js).
- [Hanabi](https://github.com/jeffshee/gnome-ext-hanabi) informed the evaluation of
  window-actor cloning and shell-version maintenance costs.

The probe and fixtures are original AGPL-3.0-or-later code. No Hanabi or proprietary
engine source/assets are copied. The GNOME runtime is supplied by the host distribution;
see [third-party provenance](../../THIRD_PARTY.md).
