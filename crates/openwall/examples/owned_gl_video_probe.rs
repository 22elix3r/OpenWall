// SPDX-License-Identifier: AGPL-3.0-or-later
//! Single-frame shared-texture proof. Never presents a desktop surface.
#[cfg(all(target_os = "linux", feature = "video-gl-probe"))]
#[path = "owned_gl_video_probe/probe.rs"]
mod probe;

#[cfg(all(target_os = "linux", feature = "video-gl-probe"))]
fn main() -> anyhow::Result<()> {
    probe::run()
}

#[cfg(not(all(target_os = "linux", feature = "video-gl-probe")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("owned_gl_video_probe requires Linux with the video-gl-probe feature")
}
