# Native EGL buffers behind a parent commit

The optional `staged_egl_subsurface_probe` tests first publication independently of
media decoding or the production worker. A layer-shell root owns an immutable,
transparent 1×1 SHM buffer and a viewport. Its native EGL child is a synchronized
subsurface. EGL controls child commits; the application controls the parent commit.
The parent remains unmapped while EGL paints and swaps hidden buffers.

[Wayland synchronized subsurfaces](https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_subsurface)
cache child commits until the parent's state is applied. Priming swaps are drained with
empty parent commits while no parent buffer is attached, so the child remains hidden.
Each native surface explicitly
requests [swap interval zero](https://registry.khronos.org/EGL/sdk/docs/man/html/eglSwapInterval.xhtml)
and requires an EGL configuration with minimum interval zero. Waiting for a frame callback
on an unpublished child would stall staging. Interval zero does not guarantee that a driver
will never block on other resources; the private process harness also bounds execution.

During an unpublished resize, the existing EGL wrapper may need one extra swap to obtain
the new drawable extent. Both passes remain behind the parent gate. After a private
control command, the probe paints one fresh, correctly sized buffer and requests
presentation feedback for that final child commit only. It then attaches the transparent
parent buffer and commits the parent. Readiness requires actual `wp_presentation` feedback
and a virtual pointer click reaching the underlying fullscreen witness. A repeated
same-size configure on initial mapping requires acknowledgement, without another repaint.

The control socket is diagnostic-only. The harness creates it inside a private temporary
directory. Binding must fail if the path exists; the probe never deletes that path.
Only one exact `publish\n` datagram is accepted. Invalid, oversized and duplicate commands
fail. This is not the future worker IPC protocol, content isolation or a security boundary.
The environment launch guard prevents accidental use outside the private harness; it is
not an OS sandbox.

wlroots 0.17.1 keeps presentation feedback in separate pending/current slots in
[`wlr_presentation_time.c`](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/0.17.1/types/wlr_presentation_time.c),
outside the cached surface state. When several hidden child commits are first applied
together, an earlier cached commit consumes the final feedback and a later one discards
it. The private Sway 1.9 test reproduced this. Empty parent commits drain each priming
update without attaching a parent buffer; final feedback is requested only afterwards.
This preserves actual presentation validation and leaves the witness visible during
staging. No compositor version check or acceptance of discarded feedback is used.

```sh
cargo build -p openwall --example staged_egl_subsurface_probe --features video-gl-probe --locked
python3 scripts/test_staged_egl.py
python3 scripts/test_staged_egl.py --gl-api gles
# Explicit use of an existing GPU/driver in a private headless compositor:
python3 scripts/test_staged_egl.py --nvidia
python3 scripts/test_staged_egl.py --nvidia --gl-api gles
```

The harness checks that the witness stays visible before publication, including after
640×360 → 800×500 native resizing. Sampled compositor frames during publication must
contain either the witness or the complete quadrant image; quadrant interiors and their
outer edges are checked. A 24×32 area at the injected center click is excluded because
Sway 1.9 can briefly include its software cursor in screencopy. The other output must remain unchanged. It
also checks idle sleep, cancellation before publication, input interception as a negative
control, existing-path preservation, invalid/duplicate commands, SIGKILL before/after
publication, and compositor loss while staging. Native cleanup must destroy one surface,
one context and one display owner. A cancelled result explicitly reports no publication
or presented frame.

## Qualification and remaining work

Private software tests cover desktop GL and GLES on Ubuntu 24.04/Sway 1.9/Mesa 25.2.8
and Ubuntu 26.04/Sway 1.11/Mesa 26.0.8. Explicit hardware tests use Fedora 43/Sway 1.11,
wlroots 0.19.3, RTX 4070 Ti SUPER, NVIDIA 580.178.04 and EGL Wayland 1.1.21. Mesa requires
four swaps in the resize/publication case, including one geometry refresh; NVIDIA requires
three. Both retain one native surface/context/display and sleep while the parent is held.

This establishes the static parent/child publication boundary on those private sessions.
Screencopy samples do not record every compositor frame or physical scanout. It does not
establish physical-display flicker behavior, video-source lifetime during staging, later
resizing of published content, replacing a real previous wallpaper, cross-output visual
atomicity, other compositor roles or transactional worker apply/rollback. Production
playback is unchanged. The next gate combines this parent boundary with bounded video
sample retention and GPU leases before applying it to the worker.
