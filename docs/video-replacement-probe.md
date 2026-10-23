# Replace and restore distinct video sources

The optional `video_replacement_probe` combines the [persistent-root publication
boundary](nested-egl-publication-probe.md) with two actual media pipelines. Each source
has its own native EGL child inside a synchronized container. Both containers belong
to one stable root and borrow one EGL session/context. The application controls
container/root commits; EGL owns native child buffers and commits.

The previous source prerolls paused, primes any necessary drawable extent while hidden,
then publishes PTS zero. Its clock starts only after actual presentation feedback. It
continues playing and looping while the candidate decodes its paused preroll into its
own unmapped container. The candidate has a separate, visibly different original fixture;
seeing the old source's pixels cannot satisfy the candidate check.

Publication draws the candidate's retained PTS-zero sample and switches the two container
mappings in one root commit. The previous source is paused with its last rendered sample
retained. Actual candidate presentation starts its clock. Explicit rollback draws that
retained previous sample, switches the container mappings back, waits for actual previous
presentation and resumes the previous clock. The previous decoder is not recreated.
After that presentation, the candidate's samples/fences, media pipeline, native window
and protocol branch are retired while the previous source continues.

Aborting a prepared candidate or failing to open/decode it before publication releases
its media/sample and native window before removing its protocol branch. The previous
video continues playing under the same root. A midpoint ownership snapshot must show two
surfaces created, only the candidate destroyed, and no context destruction or display
termination. The retained native session outlives both producer pipelines and all leases.

Each source retains one current sample and one replaceable delivery slot. A global queue
allows at most three consumer GPU leases; completed fences release source samples without
application `glFinish`. A looping source releases its current sample and drains consumers
before a flushing seek. A publish arriving during that gap waits for a fresh previous
sample so it can retain a valid rollback source. These bounds do not count additional
GStreamer/driver/compositor pools or claim a sustained memory budget.

```sh
cargo build -p openwall --example video_replacement_probe --features video-gl-probe --locked
python3 scripts/test_video_replacement.py
python3 scripts/test_video_replacement.py --gl-api gles
# Existing NVIDIA desktop-GL driver/decoder, private compositor only:
python3 scripts/test_video_replacement.py --nvidia
```

The private control reuses the finite `publish\n`, `rollback\n`, `abort\n` diagnostic
commands plus explicit `finalize\n`, with checked phase ordering. Finalization requires
confirmed candidate presentation/input, drains consumer leases and retires the previous
source/window/branch while the candidate keeps playing. It models the resource-retirement
step after a manager commits an assignment; this diagnostic does not implement persistence.
After finalization the previous source is unavailable for rollback. Input interception is a negative control on the
root; normal root, container and native child input regions are empty. All three visible
states require real virtual pointer clicks to reach the underlying witness. A regular
SIGINT before completing the transaction reports `interrupted`, with explicit preparation,
playback and presentation fields rather than claiming a completed rollback.

`--fail-candidate-after-ms` is explicit fault injection for the private harness. After
confirmed candidate readiness it posts an injected GStreamer stream-error message through
the pipeline bus. The normal media error reader schedules rollback, stops submitting
candidate frames, waits for the pending feedback, redraws the retained previous PTS and
resumes the previous clock after presentation. Failed-candidate metadata collection must
not prevent recovery. Its samples/fences are drained before its pipeline/window/branch
are removed. The harness checks moving candidate frames before this fault and moving
previous frames afterwards. It injects a bus error; it does not corrupt an actual decoder
or reproduce a decoder/driver crash.

`--fail-before-candidate-confirmation` posts the same error immediately after the
candidate publication is queued, before the worker accepts readiness. Error handling
cancels that submission's local readiness authority and queues restoration without
waiting for the failed candidate. The candidate clock must never start. Publication
was already sent to the compositor, so a candidate frame may briefly be displayed;
the report preserves that actual presentation history separately from accepted readiness.

The cross-platform `presentation::feedback::Tracker` bounds outstanding primary
feedback identities at sixteen. Cancellation retains an identity until its terminal
event: `wp_presentation_feedback` has no client cancellation request. A late presented
or discarded event for a cancelled submission cannot clear an active restoration wait.
Cancelled discards are counted separately from unexpected active discards. Pure Rust
tests exercise both terminal event orders and capacity exhaustion; private compositor
tests check early error recovery, the exact retained previous PTS, four witness button
events, candidate retirement and continued previous playback. Which cancelled terminal
event occurs depends on compositor timing; both are valid.

## Evidence and remaining gates

Private Ubuntu 26.04/Sway 1.11/Mesa 26.0.8/GStreamer 1.28.2 passes desktop GL and GLES.
Fedora 43/Sway 1.11/wlroots 0.19.3, RTX 4070 Ti SUPER/NVIDIA 580.178.04/GStreamer
1.26.11 passes desktop GL with both actual `nvvp9dec` sources emitting GL-memory NV12
and logs selecting GL transfer without a system-memory fallback. Consumer source
memory has producer synchronization metadata and a shared context. This does not establish
zero copies or provide a CUDA trace. GStreamer older than 1.26 is rejected before any
display connection/control socket binding; CI checks 1.24 rejection separately.

Tests observe all twelve previous-source markers and a full loop during a seven-second
candidate hold, moving candidate/restored pixels, sampled transitions matching complete
old/new fixture patch sets, exact retained rollback PTS, presentation-gated clock events,
six witness button events and an unchanged second output. Sampled channel errors are
zero for desktop Mesa and at most one for NVIDIA/GLES in the expanded qualification. All 26
submitted consumer fences retire, with measured peak one and zero cleanup finishes.
Abort and malformed candidate cases retain visibly moving previous video. Invalid
commands/order, SIGINT, input interception, SIGKILL and compositor loss exercise cleanup.
Final ownership counts require two surfaces and exactly one UI context/display owner.
The injected post-publication error also restores the exact retained previous PTS and
retires the candidate without terminating the shared context/display. A separate finalized
run retires the previous source, observes all twelve candidate markers and another EOS
loop, and requires every later submitted frame to belong to the candidate. Both midpoint
paths show one of two native windows destroyed, with the context/display still alive.

This is a bounded private diagnostic with fixed published geometry, logical-size SDR
buffers and one transaction. It is not general worker IPC, production Apply or a content
sandbox. Injected media-bus errors after confirmed candidate readiness now have automatic
rollback coverage, including an error before candidate readiness is accepted. Real
post-publication decoder failures, lost/discarded active presentation feedback,
control-connection loss and rollback
failure still need qualification. A decoder/worker process crash loses both in-process
sources; manager restart/static fallback remains necessary. Repeated candidates,
replacing a committed candidate, device loss, dynamic output geometry, multi-output visual
atomicity, GNOME/Plasma/macOS roles, physical scanout and sustained resource/energy budgets
remain open. Screencopy samples check fixture patch interiors, not every compositor frame.
Production playback is unchanged.
