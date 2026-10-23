# Implementation milestones

Each milestone closes only when its acceptance evidence exists. This snapshot completes
part of M0; it does not mark untested platforms or planned features as supported.

| Milestone | Dependencies | Acceptance gate |
| --- | --- | --- |
| M0: compatibility/platform feasibility | None | Original scene and video on real macOS/Linux desktops; GUI build; package/TEX fixtures; GNOME/Plasma presentation proofs; mixed-scale/hot-plug/suspend/crash tests; CPU/GPU baselines. |
| M1: durable runtime | M0 | Same-user versioned IPC, worker supervisor, atomic apply/rollback, combined pause reasons, persisted output assignments/library, bounded restart and diagnostics. |
| M2: local library alpha | M1 | Import/preview/apply/adjust/remove end-to-end; keyboard/screen-reader checks; virtualized large libraries; playlists, per-monitor controls, opt-in login launch. |
| M3: compatibility alpha | M0–M2 | Original fixtures and reference comparisons per advertised shader/effect/property/SceneScript feature; bounded scripts; package/texture fuzzing; complete unsupported-feature reporting. |
| M4: Linux desktop beta | M2–M3 | GNOME/Plasma and qualified X11 adapters; tested shell versions; GPU import/fence failures; hardware decode, scale, session and sleep/wake regression matrix. |
| M5: interactive content | M3–M4 | More effects, particles/timelines/audio reaction, sandboxed web workers, permissions and resource budgets; capability-aware input and spanning. |
| M6: stable distribution | M4–M5 | Open signed catalog, licensed content, offline cache, signed/notarized macOS builds, native Linux packages, update integrity, corresponding source and SBOM. |

Immediate follow-up: qualify the macOS implementation on real hardware, validate efficient
GPU-resident video and the GNOME/Plasma connection and native window-role contracts, then
introduce the manager and control protocol. Do not build discovery or a browser runtime
ahead of those gates.

M0 progress (2026-10-18): per-surface fractional-scale/viewporter presentation, bounded
physical buffer sizing, video resize without decoder replacement, and a private headless
Sway regression suite now cover mixed scales, rotation, newly connected outputs, complete
output disable/enable, selected-output placement, static idle behavior, worker crashes,
and compositor loss. An experimental Plasma wallpaper item can present an external
Rust/GStreamer worker in a private nested Plasma 6 session. A GNOME 49 background probe
validates connection-owned window cloning, moving video and bounded worker cleanup.
OpenWall-owned XDG video roots and isolated GStreamer container subsurfaces now pass
GNOME/Plasma video, resize and crash tests without initial configure/geometry warnings.
The roots remain unmapped until decoded data reaches the sink; this is not a confirmed
first-presentation transaction. Normal application/window roles remain open.
Connection discovery, GPU-resident buffer sharing and real desktop lifecycle qualification
remain open.
These automated protocol/pixel checks do not qualify physical monitor hot-plug,
suspend/resume, energy use, macOS, GNOME or general Plasma support.

M0/M1 boundary progress (2026-10-23): optional native GL video conformance now has
one reusable EGL owner per Wayland connection, shared decoder/output lifetime checks,
and distinct old/new sources below one persistent desktop root. Private Mesa desktop
GL/GLES and NVIDIA desktop GL tests cover presentation-gated clocks, retained-frame
rollback, failed/cancelled candidate readiness, real candidate-discard recovery and
explicit old-source retirement after finalization. A discarded restoration remains a
failure. These are qualified building blocks for a persistent worker, not production
Apply or a durable manager. Subsequent private conformance covers recreation/retirement
of the next hidden candidate, same-user inherited-controller loss, exact presentation
receipts independent of bounded diagnostic histories and combined user/power pause
reasons. Paused publication/restoration can retain static frames without starting a
media clock; clearing one reason cannot clear another. These remain diagnostic controls.
Repeated transactions, production versioned control/authentication, durable assignments,
real platform policy producers and worker restart/fallback remain M1 work; lost feedback
and actual decoder/driver crashes still need recovery evidence.
