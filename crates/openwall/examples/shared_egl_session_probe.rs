// SPDX-License-Identifier: AGPL-3.0-or-later
#[cfg(all(target_os = "linux", feature = "video-gl-probe"))]
#[path = "shared_egl_session_probe/probe.rs"]
mod probe;

#[cfg(all(target_os = "linux", feature = "video-gl-probe"))]
fn main() -> anyhow::Result<()> {
    probe::run()
}

#[cfg(not(all(target_os = "linux", feature = "video-gl-probe")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("shared_egl_session_probe requires Linux with the video-gl-probe feature")
}
