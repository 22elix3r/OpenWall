// SPDX-License-Identifier: AGPL-3.0-or-later
//! Private State binding and fault injection; publication is runtime code.
use super::State;
use anyhow::{Context, Result, ensure};
use openwall::presentation::wayland_tree::Tree;
use std::{io::Write, os::fd::AsFd};
use wayland_client::{QueueHandle, protocol::wl_shm};

pub fn create(state: &State, qh: &QueueHandle<State>) -> Result<Tree> {
    Tree::new(
        qh,
        state.surface.as_ref().context("root missing")?,
        state.compositor.as_ref().context("compositor missing")?,
        state
            .subcompositor
            .as_ref()
            .context("subcompositor missing")?,
        state.shm.as_ref().context("SHM missing")?,
        state.viewporter.as_ref().context("viewporter missing")?,
        state.configured.context("layer configure missing")?,
    )
}

/// Supersede an application-owned container commit without touching EGL attachments.
pub fn discard_cached_publication(
    tree: &Tree,
    state: &State,
    qh: &QueueHandle<State>,
    index: usize,
) -> Result<()> {
    ensure!(
        tree.active() != Some(index),
        "discard control requires a hidden branch"
    );
    let container = tree.container(index)?;
    let fd =
        rustix::fs::memfd_create(c"openwall-discard-control", rustix::fs::MemfdFlags::CLOEXEC)?;
    let mut file = std::fs::File::from(fd);
    file.write_all(&0u32.to_ne_bytes())?;
    let pool = state
        .shm
        .as_ref()
        .context("SHM missing")?
        .create_pool(file.as_fd(), 4, qh, ());
    let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    container.attach(Some(&buffer), 0, 0);
    container.damage_buffer(0, 0, 1, 1);
    container.commit();
    container.attach(None, 0, 0);
    container.commit();
    tree.flush(index)?;
    buffer.destroy();
    Ok(())
}
