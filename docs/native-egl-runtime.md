# Native EGL ownership runtime

The optional Linux `openwall::native_egl` module is shared runtime code used by the
static, staging, shared-video, replacement and measurement conformance programs. It is
available with `native-egl`, independently of GStreamer. `video-gl-probe` includes that
feature plus its media dependencies. This extraction does not change production Apply
or create a stable external plugin ABI.

`Session` owns a retained Wayland connection, one EGLDisplay, one creating-thread context
and its loaded GL dispatch. Each `Window` retains an `Rc<Session>` and destroys its
EGLSurface/native wrapper before releasing that reference. `Rc`/`Cell` prevent sending
native ownership to another thread. The static original quadrant painter stays in
example support code, separate from resource ownership.

## Enforced boundaries

- Repeating `Session::new` on the same connection/thread/API reuses the existing `Rc`.
  It does not reinitialize EGL or change the current window binding.
- A conflicting API on the same connection is rejected before any EGL calls.
- Live sessions on one thread use one GL API, including across independent connections.
  A different API on that thread is rejected before claiming or initializing a display.
- A process-wide connection reservation rejects a duplicate owner from another thread.
  Only connection handles cross threads; the registry contains no GL context or `Rc`.
- Reservations precede fallible EGL initialization and remain until native destruction.
  Partial construction and final owner teardown release them. Thread-local references
  are weak and expired entries are pruned when constructing another session.
- Independent connections can own independent EGL displays on the same thread. Dropping
  an inactive owner/window preserves another owner's current context. Destruction unbinds
  only the context/drawable being released.
- Native windows validate bounded geometry and the surface's connection before wrapping
  it. `Window::on_session` is explicitly unsafe: callers must retain the exclusive
  protocol surface/role and prevent other producers until the native window is gone.
- Drawing binds the selected window before native resizing. Presentation checks that its
  context/drawable remained current. Context reports also reject a non-current owner.
  Raw GL use and borrowed GStreamer wrappers retain their documented caller obligations.

This registry covers application-owned sessions created through this module. It does not
make arbitrary foreign EGL termination/context manipulation safe. The separate GStreamer
sharing guard still wraps a foreign display without owning its termination and retains
the native session until producer objects and consumer samples/fences have been released.

The strengthened shared-session diagnostic checks pointer-identical reuse, API and
cross-thread rejection, an independent connection whose destruction preserves the first
context/texture, and owner recreation after the last owner is destroyed. Its original
two-output pixels, real input, resize/scale/rotation, output removal and zero-window
texture-lifetime checks remain. Mid/final counters distinguish the original owner,
independent connection and recreated owner; the latter two create no native windows.

```sh
cargo clippy -p openwall --lib --no-default-features --features native-egl --locked -- -D warnings
cargo build -p openwall --examples --features video-gl-probe --locked
python3 scripts/test_shared_egl_session.py
python3 scripts/test_shared_egl_session.py --gl-api gles
```

The library supplies resource ownership, not platform wallpaper roles, transaction IPC,
device affinity/recovery or automatic published-source rollback. Those responsibilities
remain separate integration gates. Native GPU calls can still block; private harnesses
bound execution and the eventual launcher must retain process deadlines.
