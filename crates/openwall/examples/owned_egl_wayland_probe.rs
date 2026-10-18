// SPDX-License-Identifier: AGPL-3.0-or-later
//! Private-compositor EGL window ownership and input regression probe.
#[cfg(all(target_os = "linux", feature = "video-gl-probe"))]
#[path = "owned_egl_wayland_probe/probe.rs"]
mod probe;

#[cfg(all(target_os = "linux", feature = "video-gl-probe"))]
fn main() -> anyhow::Result<()> {
    probe::run()
}

#[cfg(not(all(target_os = "linux", feature = "video-gl-probe")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("owned_egl_wayland_probe requires Linux with the video-gl-probe feature")
}
