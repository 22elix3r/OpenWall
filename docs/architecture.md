# Architecture

OpenWall separates user interface, content interpretation, and desktop presentation.

```mermaid
flowchart LR
    Q[Qt Quick / Rust GUI] -->|process arguments, bounded diagnostics| W[Wallpaper worker]
    C[CLI] --> W
    W --> D[Bounded asset + scene reader]
    D --> G[wgpu sprite renderer]
    W --> V[Native video player]
    G --> P[Wayland layer-shell / AppKit]
    V --> P
```

`openwall-core` contains immutable asset access, the declared compatibility subset,
original sample loading and reports. It has no Qt, GPU or desktop dependency. Package
data is read without extraction. Resource paths stay inside the selected wallpaper root.
PNG/JPEG/WebP and static raw TEX decoding is bounded; embedded-media TEX is rejected.

`openwall` is the actual GPU/media worker and diagnostic CLI. A scene's textures are
uploaded to the GPU and rendered only on presentation/configuration changes. Linux
Wayland desktop surfaces are background-layer, non-focusable and click-through. Hot-plug
removes GPU presentation before native surfaces. The C pointer boundary documents connection and
surface lifetime requirements. Linux video uses GStreamer-owned Wayland subsurfaces;
the parent surface has a static shared-memory black buffer backed by an anonymous memfd,
so video workers never initialize a second scene GPU/device or require a writable temporary
filesystem. macOS uses AppKit and AVFoundation.

Wayland video previews and experimental shell-hosted workers use an OpenWall-owned XDG
toplevel. GStreamer receives a separate container subsurface, preserving OpenWall's
exclusive ownership of root commits and XDG configure acknowledgments. This matters
because GStreamer's embedding constructor commits the parent it receives. The XDG root
stays unmapped until decoded video reaches the sink. A sink-buffer arrival is data
readiness, not proof of compositor presentation; M1 still needs transactional first-frame
acknowledgments. A startup deadline covers missing configure/data events; the launcher
must also bound blocked native-library calls. Playback waits on Wayland and media events
rather than periodic polling. Resize briefly synchronizes the container with its parent commit,
then resumes independently paced playback without restarting the decoder. Preview on X11
still uses GStreamer's automatic window; this does not provide X11 desktop integration.

GStreamer 1.28's output-registration callback has an upstream crash on repeated output
notifications. The [app-local helper](../integrations/gstreamer/README.md) keeps its
LGPL source and a minimal patch alongside the worker, loaded before plugin initialization
and retained for the process lifetime. Without that helper, known affected native
runtimes fail before desktop surface creation. Older 1.24/1.26 paths use the system
helper. This scoped dependency fix does not change codecs, driver selection, environment
variables or the scene renderer.

Wayland surface-local dimensions and physical buffer dimensions are separate. The
`wp_fractional_scale_v1` preference uses units of 1/120; buffers round to physical
pixels while `wp_viewport` keeps the logical destination and `wl_surface` buffer scale
at one. Compositors without those extensions use the preferred integer surface/output
scale. Dimension and allocation limits are checked before buffer creation or GPU
configuration. Video parent surfaces use a single black pixel when viewporter is
available, and rectangle changes resize the existing native video overlay instead of
restarting its decoder. Temporary frame-acquisition failures retry with a capped delay;
successful static presentation returns to an event-only wait. Compositor disconnection
terminates the worker with an error. Output removal releases the decoder/GPU surface,
viewport, fractional-scale object, layer surface, native surface and output binding.
Repeated identical scale preferences/configure notifications do not schedule a redraw;
configure serials are still acknowledged. This prevents a commit/notification feedback
loop on compositors that resend unchanged preferences.
New scene surfaces request presentation feedback and retry an unconfirmed initial
frame at 200 ms intervals, with at most three submissions. `wp_presentation` confirmation
ends recovery; compositors without it use the weaker frame-callback hint. Missing
feedback never creates a permanent redraw loop. This handles initial frames lost during
output recovery; a normal frame callback alone can arrive before a successful display
commit on older compositors.

Protocol references: [fractional scale](https://wayland.app/protocols/fractional-scale-v1),
[viewporter](https://wayland.app/protocols/viewporter), and the
[core Wayland specification](https://wayland.freedesktop.org/docs/html/apa.html).

The GUI accepts `--open PATH` to inspect/select one local wallpaper at launch; it does
not apply it automatically. Qt's own arguments remain available. The existing `--check`
mode waits for a pending selection/thumbnail to settle, with a finite upper bound,
reports a diagnostic snapshot through Qt logging, and exits. Offscreen integration
checks use it to cover actual scene/image import, generated PNG loading, missing and
malformed previews, selection retention and temporary-file cleanup.

The pinned [qt-build-utils 0.8.1 source](https://github.com/KDAB/cxx-qt/blob/666fd733bc82523c2b5b19d6c2938662791b18ad/crates/qt-build-utils/src/lib.rs#L271)
runs QML compilation before regenerating its type-information file. Locally, changing
QObject invokable order with an existing build reproduced an AOT method-lookup crash.
OpenWall's build script removes only its previous `plugin.qmltypes` in its own Cargo
OUT_DIR before invoking the builder, matching fresh-build behavior. The builder still
regenerates/exports current metadata for tooling afterward. This avoids stale method
indices without deleting Cargo caches or disabling QML compilation globally; revisit
this workaround when the upstream generation order changes.

`openwall-ui` exposes Rust objects to QML through CXX-Qt. Content inspection runs in a
separate process on a background thread; the GUI never evaluates wallpaper scripts or
loads downloaded QML. Worker logs are continuously drained and capped at 16 KiB to
prevent pipe backpressure. Selecting another file or returning to the sample cancels
the obsolete inspection,
terminating and reaping its owned process group within the query loop's polling interval
when native process operations are responsive. Cancellation belongs to that inspection;
display discovery and newer inspections remain usable. Generation checks prevent stale
inspection results replacing
a newer selection. The current GUI owns its workers and stops them on exit.

GUI inspection and display discovery use finite process queries on Linux/macOS:
10-second and 5-second deadlines respectively, at most 64 KiB of complete stdout and
a 16 KiB stderr tail. Nonblocking reads/polling drain both pipes without detached
reader threads; a continuously noisy pipe cannot starve the deadline or the other pipe.
An application-owned query scope allows at most eight jobs, reclaims completed thread
handles, and cancels/joins active jobs before the Qt engine is destroyed.

Inspection success responses carry schema version 1 and are decoded through a bounded
shared data contract before reaching Qt. Missing/malformed fields, unknown versions or
content kinds, and invalid image/scene dimensions fail without changing the selection.
Video dimensions can remain zero until native decode. The GUI's busy state clears on
these errors. This is the current inspector response contract, not the future manager's
versioned request/response protocol. CLI inspection retains full compatibility details;
the GUI rejects responses exceeding its 64 KiB query/report budget.
After successful inspection, a second cancellable worker query generates a static PNG
thumbnail with a five-second deadline. Raster input is checked against the 8192-pixel
axis limit and a 64 MiB RGBA output budget before decoding/conversion; decoding retains
its separate allocation limits. The worker downsizes without enlargement, preserving
aspect ratio, to at most 512 pixels per side and caps encoded output at 2 MiB. Images
and TEX use their selected file; project/scene/video entries use only a bounded loose
`preview.png` under that directory, without loading a neighboring package or running
wallpaper/video code. Missing or rejected previews leave a valid selection available.
The GUI receives only the generated PNG from a temporary directory created with owner-only
permissions on Linux/macOS. It checks output size, PNG header and dimensions, retains
that directory while displayed, and removes it on release/normal shutdown. It does not
pass original images or untrusted sidecar paths to Qt. This reduces GUI decoder exposure;
it does not add an operating-system sandbox, hostile same-user isolation or crash cleanup.

Titles, details and worker diagnostics are rendered as plain text; imported strings
cannot opt those labels into Qt's rich-text/inline-image behavior.

Each query starts a new process group. The sole waiter observes exit with `waitid`
and `WNOWAIT`, preserving the leader's PID until group termination and final reaping.
This prevents signaling a reused numeric process-group ID after early parent exit,
and bounds queries when ordinary descendants retain inherited pipes. GUI shutdown
also terminates active query groups. This is lifecycle management, not an OS content
sandbox or the future persistent manager's worker supervisor. Normal wallpaper
playback remains managed separately from these short-lived queries. Playback/preview
workers now share the same sole-waiter process-group owner. Their diagnostic reader
uses nonblocking reads plus a wake socket, retains a 16 KiB tail, and is joined on
observed process exit or worker destruction. An exited leader's ordinary descendants
are terminated before it is reaped, even when they retain stderr. After reaping,
repeated status checks/cleanup never signal its former numeric process-group ID.
Final diagnostics are drained before the GUI reports exit; post-stop draining is
bounded so an escaped writer cannot keep the reader alive indefinitely. Closed stderr
alone does not end a running worker. Observation errors clear the GUI's running state.
These paths cover normal user-space process cleanup, not escaped process sessions,
blocked kernel/driver waits or a persistent restart supervisor.

macOS display discovery runs a separate windowless winit/AppKit main-thread event loop.
Enumeration reports native display identifiers and physical geometry; Apply resolves
the selected identifier against the current monitor list. Ambiguous human-readable
names fail rather than selecting the first matching monitor. The GUI keeps labels and
selection values separate, preserving Wayland output names as Linux selectors. Native
macOS IDs are configuration-local hints and must not become durable assignment keys;
the manager still needs a qualified persistent hardware identity policy. Enumeration,
multi-display placement and Spaces behavior remain unqualified on real macOS hardware.

## Next boundary: manager

M1 will replace GUI-owned playback with a background Rust manager. It will own SQLite
state and output assignments, expose bounded/versioned same-user Unix-socket messages,
and pass read-only resources to workers. The native video experiments establish a more
specific presentation boundary: a persistent worker owns each desktop root and prepares
candidate sources below unmapped synchronized containers. Independent background roots
cannot supply a deterministic replacement order. The existing source remains active
until publication and retained for rollback until the manager finalizes the assignment. The private video diagnostic also
accepts a bounded inherited Unix stream from a same-user controller: loss or invalid
commands retire a hidden candidate or restore the retained source before finalization;
a finalized assignment remains active. This validates responsive-event-loop ownership
loss, not production IPC, driver-hang recovery or durable manager/worker reconciliation.

The manager must publish a prepared candidate, await actual presentation on its target
outputs, persist the assignment, then finalize resource retirement. Persistence failure,
lost control or a transaction deadline must roll back while the previous source is
retained. These are integration requirements, not implemented IPC/manager behavior.
The [two-source diagnostic](video-replacement-probe.md) currently establishes explicit
publication/rollback and pre-publication media failure handling in one process, with
injected post-publication media-error rollback and explicit finalization retirement.
Per-registration presentation receipts retain terminal bookkeeping independently of
bounded diagnostic histories. The video replacement path uses exact receipts to gate
clocks and identify an active candidate discard; a full history cannot suppress a later
successful transition or misidentify a failure. Receipts are local to the owning event
thread and retain no native/GStreamer resources. Keeping application receipts bounded,
generating unambiguous source/transaction identities and reconciling durable manager
decisions remain responsibilities of the persistent worker/manager design.

The reusable presentation-feedback tracker also separates locally cancelled readiness
from a compositor's eventual terminal event, so a failed candidate's late feedback
cannot complete a restoration wait. The candidate clock stays paused when its error
is observed before readiness is accepted.
Real decoder/presentation/control failure recovery and repeated assignments remain open. A process crash still
loses that worker's sources; the manager needs bounded restart/backoff and a trusted
static/previous fallback after its restart budget is exhausted. Combine
user/power/session/adapter-visibility pause reasons independently of transaction state.

The [native EGL runtime](native-egl-runtime.md) now supplies the reusable Linux display/
context/window ownership boundary used by the conformance programs. It remains optional;
normal production playback and non-Linux presenters do not silently switch backend.

`presentation::wayland_tree::Tree` supplies the reusable two-branch synchronized
publication tree independently of EGL/GStreamer. The adapter supplies a configured
root and its globals, retains its desktop role, and delegates root buffer/viewport
commits to the tree. Native windows must disappear before branch retirement. Root
input/configure policy stays with the adapter; both content branches have empty input
regions. The last submitted branch is private bookkeeping exposed read-only, and does
not establish actual presentation. The caller still gates readiness and persistence/
finalization on feedback. Retired slots can be recreated hidden while the submitted
branch and root remain alive. Geometry stays fixed; complete repeated publication/
rollback/commit transactions remain open.
The video conformance program now exercises this library implementation; its State
binding, input witness and forced-discard controls remain outside the runtime.

The current launcher does not implement that transaction or a restart policy. There is
no stable external plugin ABI. GNOME/Plasma adapters must remain minimal trusted
presentation shims; media decoding, scripts and shader compilation stay out of the shell.

The [experimental Plasma shim](../integrations/plasma/README.md) implements that
presentation boundary using a Qt Wayland compositor inside a `WallpaperItem`. An
external Rust/GStreamer worker connects to a per-instance socket and supplies one XDG
toplevel; the shell displays its buffers below desktop widgets and forwards no input.
Disconnecting a duplicate client is deferred until Qt's creation callback has returned,
avoiding use-after-free in Qt's remaining role initialization. Worker exit/crash reveals
a static fallback and permits reconnection. This prototype uses test-only discovery and
is not connected to Apply. Shared-memory correctness has been tested; efficient hardware
buffer import and a versioned/authorized connection handshake remain open.

The [GNOME presentation probe](../integrations/gnome/README.md) uses a dedicated
`Meta.WaylandClient` connection and a native `Clutter.Clone` inside the desktop's
background actor. Ownership is determined by the connection; unrelated windows with
matching titles are preserved. Worker logs, startup and stop waits are bounded, and
source/clone teardown is tied to the specific worker instance. It does not install
shell-method overrides or provide finished app-list/workspace roles. Its native XDG video
path uses the same isolated container boundary as other Wayland previews. A minimized
source window can still feed a visible background clone, so future visibility policy must
use the adapter's actual background state rather than the source window's minimized state.
Normal GNOME Apply remains guarded.

## Compatibility boundaries

The parser dependency is pinned in Cargo.toml/Cargo.lock. OpenWall currently maps only
static orthographic image layers, transform/opacity data and the generic image material
to its own sprite shader. Unknown layer/model/material fields are reported; complex
scenes are rejected by default. This is an initial import subset, not full engine parity.

Future features need deterministic original fixtures, public API/behavior observations,
and comparisons with an owned reference installation. Replacement built-ins must be
independently written and openly licensed. Avoid dependence on official shared assets,
Windows executables, the proprietary Steam SDK or incompatible GPL-2.0-only renderers.
