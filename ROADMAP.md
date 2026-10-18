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

Immediate follow-up: qualify the macOS implementation on real hardware, prove GNOME and
Plasma presentation without running content inside the shell, then introduce the manager
and control protocol. Do not build discovery or a browser runtime ahead of those gates.

M0 progress (2026-10-18): per-surface fractional-scale/viewporter presentation, bounded
physical buffer sizing, video resize without decoder replacement, and a private headless
Sway regression suite now cover mixed scales, rotation, newly connected outputs, complete
output disable/enable, selected-output placement, static idle behavior, worker crashes,
and compositor loss. An experimental Plasma wallpaper item can present an external
Rust/GStreamer worker in a private nested Plasma 6 session. Connection discovery,
GPU-resident buffer sharing and real desktop lifecycle qualification remain open.
These automated protocol/pixel checks do not qualify physical monitor hot-plug,
suspend/resume, energy use, macOS, GNOME or general Plasma support.
