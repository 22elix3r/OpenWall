// SPDX-License-Identifier: AGPL-3.0-or-later
//! Two native video branches under one persistent desktop role.
use super::State;
use anyhow::{Context, Result, ensure};
use std::{io::Write, os::fd::AsFd};
use wayland_client::{
    QueueHandle,
    protocol::{wl_buffer::WlBuffer, wl_shm, wl_subsurface::WlSubsurface, wl_surface::WlSurface},
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

struct Branch {
    surface: WlSurface,
    role: WlSubsurface,
    container: WlSurface,
    container_role: WlSubsurface,
    viewport: WpViewport,
}

impl Drop for Branch {
    fn drop(&mut self) {
        // Native EGL window wrappers must already be gone.
        self.role.destroy();
        self.surface.destroy();
        self.viewport.destroy();
        self.container_role.destroy();
        self.container.destroy();
    }
}

pub struct Tree {
    branches: [Option<Branch>; 2],
    root: WlSurface,
    viewport: WpViewport,
    transparent: WlBuffer,
    size: (i32, i32),
    pub active: Option<usize>,
}

impl Tree {
    pub fn new(state: &State, qh: &QueueHandle<State>) -> Result<Self> {
        let (width, height) = state.configured.context("layer configure missing")?;
        ensure!(
            width > 0
                && height > 0
                && width <= 16_384
                && height <= 16_384
                && u64::from(width) * u64::from(height) * 4 <= 256 * 1024 * 1024,
            "video tree geometry exceeds limits"
        );
        let root = state.surface.as_ref().context("root missing")?;
        let compositor = state.compositor.as_ref().context("compositor missing")?;
        let subs = state
            .subcompositor
            .as_ref()
            .context("subcompositor missing")?;
        let views = state.viewporter.as_ref().context("viewporter missing")?;
        let fd = rustix::fs::memfd_create(c"openwall-video-tree", rustix::fs::MemfdFlags::CLOEXEC)?;
        let mut file = std::fs::File::from(fd);
        file.write_all(&0u32.to_ne_bytes())?;
        let pool = state
            .shm
            .as_ref()
            .context("SHM missing")?
            .create_pool(file.as_fd(), 4, qh, ());
        let transparent = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, qh, ());
        pool.destroy();
        let branches = std::array::from_fn(|_| {
            let container = compositor.create_surface(qh, ());
            let container_role = subs.get_subsurface(&container, root, qh, ());
            container_role.set_sync();
            let surface = compositor.create_surface(qh, ());
            let role = subs.get_subsurface(&surface, &container, qh, ());
            role.set_sync();
            let empty = compositor.create_region(qh, ());
            container.set_input_region(Some(&empty));
            surface.set_input_region(Some(&empty));
            empty.destroy();
            Some(Branch {
                viewport: views.get_viewport(&container, qh, ()),
                surface,
                role,
                container,
                container_role,
            })
        });
        let viewport = views.get_viewport(root, qh, ());
        viewport.set_destination(width as i32, height as i32);
        Ok(Self {
            branches,
            root: root.clone(),
            viewport,
            transparent,
            size: (width as i32, height as i32),
            active: None,
        })
    }

    fn branch(&self, index: usize) -> Result<&Branch> {
        self.branches
            .get(index)
            .and_then(Option::as_ref)
            .context("video branch missing")
    }

    pub fn surface(&self, index: usize) -> Result<&WlSurface> {
        Ok(&self.branch(index)?.surface)
    }

    pub fn flush(&self, index: usize) -> Result<()> {
        self.branch(index)?.container.commit();
        self.root.commit();
        Ok(())
    }

    pub fn show(&mut self, index: usize) -> Result<()> {
        self.branch(index)?;
        for (slot, branch) in self.branches.iter().enumerate() {
            if let Some(branch) = branch {
                branch.viewport.set_destination(self.size.0, self.size.1);
                branch.container.attach(
                    if slot == index {
                        Some(&self.transparent)
                    } else {
                        None
                    },
                    0,
                    0,
                );
                branch.container.damage_buffer(0, 0, 1, 1);
                branch.container.commit();
            }
        }
        self.root.attach(Some(&self.transparent), 0, 0);
        self.root.damage_buffer(0, 0, 1, 1);
        self.root.commit();
        self.active = Some(index);
        Ok(())
    }

    pub fn remove(&mut self, index: usize) -> Result<()> {
        ensure!(
            self.active != Some(index),
            "cannot remove the visible video branch"
        );
        self.branch(index)?;
        self.branches[index].take();
        Ok(())
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        self.branches.iter_mut().for_each(|branch| {
            branch.take();
        });
        self.viewport.destroy();
        self.transparent.destroy();
        // The State owner retains and destroys the desktop root after this tree.
    }
}
