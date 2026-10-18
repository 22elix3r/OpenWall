// SPDX-License-Identifier: AGPL-3.0-or-later
#[cfg(all(target_os = "linux", feature = "video-gl-probe"))]
#[path = "video_replacement_probe/probe.rs"]
mod probe;

#[cfg(all(target_os = "linux", feature = "video-gl-probe"))]
fn main() -> anyhow::Result<()> {
    probe::run()
}

#[cfg(not(all(target_os = "linux", feature = "video-gl-probe")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("video_replacement_probe requires Linux with the video-gl-probe feature")
}
