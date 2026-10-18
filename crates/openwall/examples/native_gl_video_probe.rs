// SPDX-License-Identifier: AGPL-3.0-or-later
//! Shared-texture video presentation in an isolated private Wayland compositor.
#[cfg(all(target_os = "linux", feature = "video-gl-probe"))]
#[path = "native_gl_video_probe/probe.rs"]
mod probe;

#[cfg(all(target_os = "linux", feature = "video-gl-probe"))]
fn main() -> anyhow::Result<()> {
    probe::run()
}

#[cfg(not(all(target_os = "linux", feature = "video-gl-probe")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("native_gl_video_probe requires Linux with the video-gl-probe feature")
}
