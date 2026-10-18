# One video source across two native outputs

The optional `shared_video_outputs_probe` combines the native shared-video renderer with
the two-window EGL session owner. It creates one playbin/decoder pipeline, one source
delivery slot and one retained current sample for two private layer-shell outputs. Each
output copies the same shared GL texture into its own native window buffer. It never
CPU-maps the source or creates a consumer off-screen target.

One context is switched between the two EGL surfaces on the creating thread. Each output
has its own pending presentation feedback and last-rendered source serial. A slower
output can consume the latest retained source when its feedback completes. A global
queue bounds consumer leases to three; each draw retains its sample until its GL fence
signals. Cached current samples, the delivery slot and GStreamer/driver buffer pools are
additional resources, rather than part of that queue limit.

The pipeline initially prerolls in PAUSED. Both outputs must present PTS zero before
the shared media clock starts. This is not simultaneous cross-output presentation.
The first output also proves real click-through to an underlying witness. Closing that
output destroys its EGL surface before its protocol role and root. The shared EGL session,
borrowed GStreamer context, decoder and second output stay alive. Actual moving video
and another loop on the second output must continue after that teardown.

The shared `native_media_context::Sharing` guard retains the native session independently
of any window. It makes that context current without a drawable before wrapping it, then
releases the borrowed wrapped context before the foreign display wrapper and native owner.
All source samples, GPU leases and pipeline objects must be released before the guard.
The single-output diagnostic uses the same guard. These counters cover application EGL
ownership calls; GStreamer producer contexts and driver/compositor resources are separate.

```sh
cargo build -p openwall --example shared_video_outputs_probe --features video-gl-probe --locked
python3 scripts/test_shared_video_outputs.py
python3 scripts/test_shared_video_outputs.py --gl-api gles
# Existing hardware/driver only, inside the private headless compositor:
python3 scripts/test_shared_video_outputs.py --nvidia
```

The harness checks original lossless YUV fixture pixels on both outputs, matching texture
IDs and PTS for shared source serials, one actual video decoder element, independent
presentation histories, and complete fence retirement. It resizes the second output,
disables the first, then checks surviving video through fractional/integer scale and
rotation changes and a drained EOS loop. While the first native window has been retired,
the display/context must remain alive with only one of two surfaces destroyed. Final
cleanup must destroy two surfaces, one UI context and one display owner. SIGINT,
malformed media, SIGKILL and compositor loss exercise cleanup with both outputs present.
The test owns a private compositor/runtime/socket and never connects to a desktop session.

## Boundaries

Private Ubuntu 26.04/Sway 1.11/Mesa 26.0.8/GStreamer 1.28.2 tests cover both GL APIs.
An explicit Fedora 43/Sway 1.11/wlroots 0.19.3/RTX 4070 Ti SUPER test uses NVIDIA
580.178.04 and GStreamer 1.26.11 with desktop GL. Both outputs visibly animate before
the first is removed; the survivor displays all twelve original markers across two
loops. Sampled channels match the independent reference exactly. All submitted consumer
fences retire, with a measured peak of two and no application cleanup `glFinish` calls.

This is a bounded two-output diagnostic. It still uses logical-size buffers and stretches
SDR content. It does not provide general output assignment, full-resolution HiDPI,
different content per output, mixed-refresh qualification, zero-output recreation,
suspend/device-loss recovery, staged replacement of mapped content, transactional apply,
physical-display behavior or sustained CPU/GPU/power budgets. Published resizing retains
the existing bounded follow-up repaint; it can expose a priming frame and is not a
flicker-free transaction. The separate [staged publication proof](staged-egl-probe.md)
keeps such priming buffers hidden before the first parent mapping.

The native pipeline requires GStreamer 1.26 or newer. Hardware mode requires desktop GL
and the existing NVIDIA driver/decoder plus source/caps/log evidence of its GL transfer
path. This does not prove zero copies or provide a CUDA API trace. Production playback
continues to create one decoder per output until its backend and lifecycle gates are met.
