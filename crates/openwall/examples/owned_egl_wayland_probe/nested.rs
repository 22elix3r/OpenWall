// SPDX-License-Identifier: AGPL-3.0-or-later
//! Private protocol proof: keep an old root visible around a hidden candidate.
use super::State;
use anyhow::{Context, Result, ensure};
use std::{io::Write, os::fd::AsFd};
use wayland_client::{
    QueueHandle,
    protocol::{wl_buffer::WlBuffer, wl_shm, wl_subsurface::WlSubsurface, wl_surface::WlSurface},
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

struct Candidate {
    child: WlSurface,
    child_role: WlSubsurface,
    parent: WlSurface,
    parent_role: WlSubsurface,
    viewport: WpViewport,
}

impl Drop for Candidate {
    fn drop(&mut self) {
        // The caller has already destroyed its native EGL wrappers.
        self.child_role.destroy();
        self.child.destroy();
        self.viewport.destroy();
        self.parent_role.destroy();
        self.parent.destroy();
    }
}

pub(super) struct Tree {
    candidate: Option<Candidate>,
    root: WlSurface,
    viewport: WpViewport,
    previous: WlBuffer,
    transparent: WlBuffer,
}

fn pixel(state: &State, qh: &QueueHandle<State>, argb: u32) -> Result<WlBuffer> {
    let fd = rustix::fs::memfd_create(c"openwall-nested-pixel", rustix::fs::MemfdFlags::CLOEXEC)?;
    let mut file = std::fs::File::from(fd);
    file.write_all(&argb.to_ne_bytes())?;
    let pool = state
        .shm
        .as_ref()
        .context("SHM missing")?
        .create_pool(file.as_fd(), 4, qh, ());
    let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    Ok(buffer)
}

impl Tree {
    pub fn new(
        state: &State,
        qh: &QueueHandle<State>,
        previous: [u8; 3],
        intercept: bool,
    ) -> Result<Self> {
        let root = state.surface.as_ref().context("root missing")?;
        let compositor = state.compositor.as_ref().context("compositor missing")?;
        let subs = state
            .subcompositor
            .as_ref()
            .context("subcompositor missing")?;
        let views = state.viewporter.as_ref().context("viewporter missing")?;
        let [red, green, blue] = previous.map(u32::from);
        let previous = pixel(state, qh, 0xff000000 | red << 16 | green << 8 | blue)?;
        let transparent = match pixel(state, qh, 0) {
            Ok(buffer) => buffer,
            Err(error) => {
                previous.destroy();
                return Err(error);
            }
        };
        let parent = compositor.create_surface(qh, ());
        let parent_role = subs.get_subsurface(&parent, root, qh, ());
        parent_role.set_sync();
        let child = compositor.create_surface(qh, ());
        let child_role = subs.get_subsurface(&child, &parent, qh, ());
        child_role.set_sync();
        let region = compositor.create_region(qh, ());
        parent.set_input_region(Some(&region));
        if !intercept {
            child.set_input_region(Some(&region));
        }
        region.destroy();
        Ok(Self {
            candidate: Some(Candidate {
                viewport: views.get_viewport(&parent, qh, ()),
                parent,
                parent_role,
                child,
                child_role,
            }),
            root: root.clone(),
            viewport: views.get_viewport(root, qh, ()),
            previous,
            transparent,
        })
    }

    pub fn surface(&self) -> Option<&WlSurface> {
        self.candidate.as_ref().map(|candidate| &candidate.child)
    }

    fn size(&self, size: (u32, u32)) -> Result<(i32, i32)> {
        let (width, height) = size;
        ensure!(
            width > 0
                && height > 0
                && width <= 16_384
                && height <= 16_384
                && u64::from(width) * u64::from(height) * 4 <= 256 * 1024 * 1024,
            "nested geometry exceeds limits"
        );
        self.viewport.set_destination(width as i32, height as i32);
        Ok((width as i32, height as i32))
    }

    pub fn show_previous(&self, size: (u32, u32)) -> Result<()> {
        self.size(size)?;
        self.root.attach(Some(&self.previous), 0, 0);
        self.root.damage_buffer(0, 0, 1, 1);
        self.root.commit();
        Ok(())
    }

    pub fn flush(&self, size: (u32, u32)) -> Result<()> {
        self.size(size)?;
        // Applying cached priming states cannot reveal the native child: this
        // intermediate parent still has no buffer, even while the root is mapped.
        self.candidate
            .as_ref()
            .context("candidate missing")?
            .parent
            .commit();
        self.root.commit();
        Ok(())
    }

    pub fn publish(&self, size: (u32, u32)) -> Result<()> {
        let (width, height) = self.size(size)?;
        let candidate = self.candidate.as_ref().context("candidate missing")?;
        candidate.viewport.set_destination(width, height);
        candidate.parent.attach(Some(&self.transparent), 0, 0);
        candidate.parent.damage_buffer(0, 0, 1, 1);
        candidate.parent.commit();
        self.root.attach(Some(&self.transparent), 0, 0);
        self.root.damage_buffer(0, 0, 1, 1);
        // The root's old content and the synchronized candidate mapping change
        // together. Neither native child nor desktop role is recreated.
        self.root.commit();
        Ok(())
    }

    pub fn rollback(&self, size: (u32, u32)) -> Result<()> {
        let candidate = self.candidate.as_ref().context("candidate missing")?;
        candidate.parent.attach(None, 0, 0);
        candidate.parent.commit();
        self.show_previous(size)
    }

    pub fn abort(&mut self) {
        self.candidate.take();
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        self.candidate.take();
        self.viewport.destroy();
        self.previous.destroy();
        self.transparent.destroy();
    }
}
