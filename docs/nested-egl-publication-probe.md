# Replacement under a persistent desktop root

The optional `nested_egl_publication_probe` keeps one layer-shell root mapped throughout
candidate preparation, publication and rollback. The previous image is an immutable
opaque SHM pixel stretched through a viewport. The candidate is a native EGL grandchild
below a synchronized intermediate container. That container has no buffer while the
candidate is prepared, so its native child stays unmapped even though the desktop root
and previous image remain visible.

This avoids relying on ordering between independent background layer surfaces:
[layer-shell explicitly leaves same-layer ordering undefined](https://github.com/swaywm/wlr-protocols/blob/master/unstable/wlr-layer-shell-unstable-v1.xml).
[Subsurfaces](https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_subsurface)
are not clipped to their parent, and their mapping depends recursively on their parent
being mapped. Moving a candidate outside the output would therefore be a poor hiding
mechanism; placing it under an opaque previous image would also assume previous content
is opaque. The unmapped intermediate container supplies the actual visibility gate.

During preparation, each EGL priming commit is drained through empty container/root
commits while the container remains unmapped. This includes the existing bounded resize
refresh and avoids accumulating old wlroots presentation states. At publication, the
application attaches a transparent pixel to both container and root and commits the
root. Its previous content and the synchronized candidate mapping change together.
Rollback unmaps the container and restores the previous root buffer in one root commit.
The application never attaches buffers directly to the native EGL child. All surfaces
have empty input regions, except the explicit input-interception negative control.

Actual presentation feedback is required for the previous root, final candidate buffer
and restored root. Each published state also requires a real virtual pointer click to
reach the underlying witness. Aborting an unpublished candidate first destroys its EGL
window, then its child/container protocol objects. The root and previous image stay
visible while the session/context owner remains alive. Final cleanup releases that owner
before destroying the desktop root.

```sh
cargo build -p openwall --example nested_egl_publication_probe --features video-gl-probe --locked
python3 scripts/test_nested_egl.py
python3 scripts/test_nested_egl.py --gl-api gles
# Explicit use of existing hardware in a private headless compositor:
python3 scripts/test_nested_egl.py --nvidia
python3 scripts/test_nested_egl.py --nvidia --gl-api gles
```

The private datagram control accepts exact `publish\n`, `rollback\n` and `abort\n`
commands with bounded reads and checked phase ordering. It never deletes an existing
socket path. It is a diagnostic control, not worker IPC or an OS security boundary.
The launch guard, private runtime and compositor prevent accidental desktop use; tests
do not pass a desktop session socket or bus.

## Qualification and limits

The harness holds the previous image before and after 640×360 → 800×500 candidate
resizing, samples publication/rollback pixels, checks all three presentation identities
and click-through, and verifies an unchanged second output. It also checks sleeping
while held, abort with the previous image retained, invalid commands/order, input
interception, SIGKILL before/after publication and compositor loss. Cleanup reports one
native surface/context/display owner; the abort midpoint destroys only the native
surface. Pixel checks exclude only the small center cursor footprint after input on
Sway 1.9; the initial off-center cursor must hide before continuous checks start.

Private software qualification covers desktop GL and GLES on Ubuntu 24.04/Sway 1.9/
Mesa 25.2.8 and Ubuntu 26.04/Sway 1.11/Mesa 26.0.8. Explicit hardware qualification
uses Fedora 43/Sway 1.11/wlroots 0.19.3, RTX 4070 Ti SUPER, NVIDIA 580.178.04 and
EGL Wayland 1.1.21 with both APIs. Mesa uses one hidden geometry refresh, NVIDIA zero.

The previous content is synthetic SHM and the candidate is a static GL image. This does
not yet qualify replacement between two real video pipelines, source failure rollback,
repeated transactions, resize after publication, physical scanout, cross-output visual
atomicity or other desktop roles. Screencopy samples do not cover every compositor frame.
Production playback is unchanged. The next gate applies this persistent-root mechanism
to distinct bounded video sources with presentation-gated clock/lifetime handling before
introducing worker apply/rollback.
