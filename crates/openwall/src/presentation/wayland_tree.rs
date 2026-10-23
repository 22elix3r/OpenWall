// SPDX-License-Identifier: AGPL-3.0-or-later
//! Two synchronized native content branches under one persistent desktop root.
use anyhow::{Context, Result, ensure};
use std::{io::Write, os::fd::AsFd};
use wayland_client::{
    Dispatch, QueueHandle,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_region::WlRegion,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_subcompositor::WlSubcompositor,
        wl_subsurface::WlSubsurface,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

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

/// The caller retains the configured root and its desktop role. It supplies all
/// globals from the root's connection and grants this tree exclusive ownership
/// of root buffer commits/viewport state. Root input and configure policy remain
/// the adapter's responsibility. Every native window must be dropped before its
/// branch is removed or this tree is dropped. Geometry is fixed for this owner.
pub struct Tree {
    branches: [Option<Branch>; 2],
    root: WlSurface,
    viewport: WpViewport,
    transparent: WlBuffer,
    size: (i32, i32),
    active: Option<usize>,
    compositor: WlCompositor,
    subs: WlSubcompositor,
    views: WpViewporter,
}

impl Tree {
    pub fn new<D>(
        qh: &QueueHandle<D>,
        root: &WlSurface,
        compositor: &WlCompositor,
        subs: &WlSubcompositor,
        shm: &WlShm,
        views: &WpViewporter,
        size: (u32, u32),
    ) -> Result<Self>
    where
        D: Dispatch<WlSurface, ()>
            + Dispatch<WlSubsurface, ()>
            + Dispatch<WlRegion, ()>
            + Dispatch<WlShmPool, ()>
            + Dispatch<WlBuffer, ()>
            + Dispatch<WpViewport, ()>
            + 'static,
    {
        let (width, height) = size;
        ensure!(
            width > 0
                && height > 0
                && width <= 16_384
                && height <= 16_384
                && u64::from(width) * u64::from(height) * 4 <= 256 * 1024 * 1024,
            "presentation tree geometry exceeds limits"
        );
        let fd = rustix::fs::memfd_create(
            c"openwall-presentation-tree",
            rustix::fs::MemfdFlags::CLOEXEC,
        )?;
        let mut file = std::fs::File::from(fd);
        file.write_all(&0u32.to_ne_bytes())?;
        let pool = shm.create_pool(file.as_fd(), 4, qh, ());
        let transparent = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, qh, ());
        pool.destroy();
        let branches =
            std::array::from_fn(|_| Some(Self::make_branch(qh, root, compositor, subs, views)));
        let viewport = views.get_viewport(root, qh, ());
        viewport.set_destination(width as i32, height as i32);
        Ok(Self {
            branches,
            root: root.clone(),
            viewport,
            transparent,
            size: (width as i32, height as i32),
            active: None,
            compositor: compositor.clone(),
            subs: subs.clone(),
            views: views.clone(),
        })
    }

    fn make_branch<D>(
        qh: &QueueHandle<D>,
        root: &WlSurface,
        compositor: &WlCompositor,
        subs: &WlSubcompositor,
        views: &WpViewporter,
    ) -> Branch
    where
        D: Dispatch<WlSurface, ()>
            + Dispatch<WlSubsurface, ()>
            + Dispatch<WlRegion, ()>
            + Dispatch<WpViewport, ()>
            + 'static,
    {
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
        Branch {
            viewport: views.get_viewport(&container, qh, ()),
            surface,
            role,
            container,
            container_role,
        }
    }

    /// Recreate a retired slot, initially hidden, without changing the root or
    /// active mapping. A native window may then exclusively borrow the new leaf.
    pub fn recreate<D>(&mut self, index: usize, qh: &QueueHandle<D>) -> Result<()>
    where
        D: Dispatch<WlSurface, ()>
            + Dispatch<WlSubsurface, ()>
            + Dispatch<WlRegion, ()>
            + Dispatch<WpViewport, ()>
            + 'static,
    {
        ensure!(
            self.branches.get(index).is_some_and(Option::is_none),
            "presentation branch slot is occupied or invalid"
        );
        self.branches[index] = Some(Self::make_branch(
            qh,
            &self.root,
            &self.compositor,
            &self.subs,
            &self.views,
        ));
        Ok(())
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

    pub fn container(&self, index: usize) -> Result<&WlSurface> {
        Ok(&self.branch(index)?.container)
    }

    /// Last submitted root mapping, not an acknowledgment of presentation.
    pub fn active(&self) -> Option<usize> {
        self.active
    }

    /// Drain a hidden child's synchronized cache without mapping its container.
    pub fn flush(&self, index: usize) -> Result<()> {
        self.branch(index)?.container.commit();
        self.root.commit();
        Ok(())
    }

    /// Atomically request one mapped branch in the next root commit. The caller
    /// must separately await actual presentation before accepting readiness.
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

    /// Retire an inactive protocol branch after dropping its native window.
    /// Retaining an old source until assignment finalization is caller policy.
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
        // The adapter retains and destroys the configured desktop root after this tree.
    }
}
