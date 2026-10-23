// SPDX-License-Identifier: AGPL-3.0-or-later
#[path = "nested.rs"]
mod nested;
#[allow(dead_code)] // The two-source diagnostic uses a persistent root with two native children.
pub(super) mod video_tree;
use anyhow::{Context, Result, ensure};
use openwall::presentation::feedback::{Completion, Receipt, Tracker};
use std::{collections::BTreeMap, io::Write, os::fd::AsFd};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer, wl_compositor, wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_subcompositor, wl_subsurface, wl_surface,
    },
};
use wayland_protocols::{
    wp::{
        presentation_time::client::{wp_presentation, wp_presentation_feedback},
        viewporter::client::{wp_viewport, wp_viewporter},
    },
    xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};
use wayland_protocols_wlr::{
    layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1},
    virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1},
};

#[derive(Default)]
pub(super) struct State {
    pub secondary: Option<SecondarySurface>,
    pub surface: Option<wl_surface::WlSurface>,
    child: Option<wl_surface::WlSurface>,
    child_role: Option<wl_subsurface::WlSubsurface>,
    staging_viewport: Option<wp_viewport::WpViewport>,
    staging_buffer: Option<wl_buffer::WlBuffer>,
    staging_published: bool,
    nested: Option<nested::Tree>,
    layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    witness: Option<wl_surface::WlSurface>,
    witness_role: Option<xdg_surface::XdgSurface>,
    witness_top: Option<xdg_toplevel::XdgToplevel>,
    witness_viewport: Option<wp_viewport::WpViewport>,
    witness_buffer: Option<wl_buffer::WlBuffer>,
    witness_size: (i32, i32),
    witness_mapped: bool,
    compositor: Option<wl_compositor::WlCompositor>,
    subcompositor: Option<wl_subcompositor::WlSubcompositor>,
    shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    xdg: Option<xdg_wm_base::XdgWmBase>,
    shm: Option<wl_shm::WlShm>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    presentation: Option<wp_presentation::WpPresentation>,
    outputs: BTreeMap<u32, (wl_output::WlOutput, String)>,
    output: Option<wl_output::WlOutput>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    virtual_manager: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    virtual_pointer: Option<zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1>,
    pointer_focus: Option<wl_surface::WlSurface>,
    pub configured: Option<(u32, u32)>,
    pub dirty: bool,
    pub pending_feedback: bool,
    pub feedback_tracker: Tracker,
    pub presented: u32,
    pub presented_frame_ids: Vec<u64>,
    pub discarded: u32,
    pub discarded_frame_ids: Vec<u64>,
    pub injected: bool,
    pub witness_buttons: u32,
    pub intercepted: bool,
    pub closed: bool,
}

pub(super) struct SecondarySurface {
    pub surface: wl_surface::WlSurface,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    pub configured: Option<(u32, u32)>,
    pub dirty: bool,
    pub pending_feedback: bool,
    pub presented: u32,
    pub presented_frame_ids: Vec<u64>,
    pub discarded: u32,
    pub closed: bool,
}

pub(super) struct Secondary(pub u64);

impl Drop for SecondarySurface {
    fn drop(&mut self) {
        self.layer.destroy();
        self.surface.destroy();
    }
}

pub(super) fn connect(
    output: &str,
    intercept_input: bool,
) -> Result<(Connection, EventQueue<State>, State)> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    let output = state
        .outputs
        .values()
        .find(|(_, name)| name == output)
        .map(|(proxy, _)| proxy.clone())
        .context("selected private output was not advertised")?;
    state.output = Some(output.clone());
    let compositor = state.compositor.as_ref().context("wl_compositor missing")?;
    let shm = state.shm.as_ref().context("wl_shm missing")?;
    let xdg = state.xdg.as_ref().context("XDG shell missing")?;
    let layer_shell = state.shell.as_ref().context("layer shell missing")?;
    let viewporter = state.viewporter.as_ref().context("viewporter missing")?;
    ensure!(
        state.presentation.is_some(),
        "presentation feedback missing"
    );
    let manager = state
        .virtual_manager
        .as_ref()
        .context("private compositor has no virtual pointer v2")?;
    state.virtual_pointer = Some(manager.create_virtual_pointer_with_output(
        state.seat.as_ref(),
        Some(&output),
        &qh,
        (),
    ));

    // A fullscreen SHM witness receives the test click underneath the opaque EGL
    // overlay. Its single immutable pixel requires no second GPU context/device.
    let witness = compositor.create_surface(&qh, ());
    let role = xdg.get_xdg_surface(&witness, &qh, ());
    let top = role.get_toplevel(&qh, ());
    top.set_app_id("org.openwall.PrivateInputWitness".into());
    top.set_title("OpenWall private input witness".into());
    top.set_fullscreen(Some(&output));
    state.witness_viewport = Some(viewporter.get_viewport(&witness, &qh, ()));
    let fd = rustix::fs::memfd_create(c"openwall-egl-witness", rustix::fs::MemfdFlags::CLOEXEC)?;
    let mut pixel = std::fs::File::from(fd);
    pixel.write_all(&0xff222222u32.to_ne_bytes())?;
    let pool = shm.create_pool(pixel.as_fd(), 4, &qh, ());
    state.witness_buffer = Some(pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Xrgb8888, &qh, ()));
    pool.destroy();
    witness.commit();
    state.witness = Some(witness);
    state.witness_role = Some(role);
    state.witness_top = Some(top);

    let surface = compositor.create_surface(&qh, ());
    if !intercept_input {
        let empty = compositor.create_region(&qh, ());
        surface.set_input_region(Some(&empty));
        empty.destroy();
    }
    let layer = layer_shell.get_layer_surface(
        &surface,
        Some(&output),
        zwlr_layer_shell_v1::Layer::Overlay,
        "openwall-private-egl-probe".into(),
        &qh,
        (),
    );
    layer.set_size(0, 0);
    layer.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
    surface.commit();
    state.surface = Some(surface);
    state.layer = Some(layer);
    Ok((conn, queue, state))
}

impl State {
    #[allow(dead_code)] // Caller first destroys its primary native EGL window.
    pub fn retire_primary(&mut self) {
        self.nested.take();
        if let Some(layer) = self.layer.take() {
            layer.destroy();
        }
        if let Some(surface) = self.surface.take() {
            surface.destroy();
        }
        self.configured = None;
        self.dirty = false;
        self.pending_feedback = false;
        self.feedback_tracker.cancel_all();
    }
    #[allow(dead_code)] // Explicit staged-child mode; normal probes keep their root.
    pub fn enable_staging(&mut self, qh: &QueueHandle<Self>, intercept: bool) -> Result<()> {
        ensure!(
            self.child.is_none() && self.nested.is_none(),
            "staged native child already exists"
        );
        let compositor = self.compositor.as_ref().context("compositor missing")?;
        let root = self.surface.as_ref().context("root surface missing")?;
        let child = compositor.create_surface(qh, ());
        if !intercept {
            let region = compositor.create_region(qh, ());
            child.set_input_region(Some(&region));
            region.destroy();
        }
        let role = self
            .subcompositor
            .as_ref()
            .context("subcompositor missing")?
            .get_subsurface(&child, root, qh, ());
        role.set_sync();
        role.set_position(0, 0);
        self.staging_viewport = Some(
            self.viewporter
                .as_ref()
                .context("viewporter missing")?
                .get_viewport(root, qh, ()),
        );
        let fd =
            rustix::fs::memfd_create(c"openwall-staging-root", rustix::fs::MemfdFlags::CLOEXEC)?;
        let mut file = std::fs::File::from(fd);
        file.write_all(&0u32.to_ne_bytes())?;
        let pool = self
            .shm
            .as_ref()
            .context("SHM missing")?
            .create_pool(file.as_fd(), 4, qh, ());
        self.staging_buffer =
            Some(pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, qh, ()));
        pool.destroy();
        self.child = Some(child);
        self.child_role = Some(role);
        Ok(())
    }

    pub fn render_surface(&self) -> Result<&wl_surface::WlSurface> {
        if let Some(tree) = &self.nested {
            return tree.surface().context("nested candidate missing");
        }
        self.child
            .as_ref()
            .or(self.surface.as_ref())
            .context("native surface missing")
    }

    #[allow(dead_code)] // Advance hidden child state without mapping its parent.
    pub fn flush_staged(&self) -> Result<()> {
        ensure!(
            (self.child.is_some() || self.nested.is_some()) && !self.staging_published,
            "candidate parent must remain unmapped"
        );
        if let Some(tree) = &self.nested {
            return tree.flush(self.configured.context("layer configure missing")?);
        }
        // Older wlroots applies cached commits individually and presentation
        // feedback is not part of that cache. Drain priming commits without any
        // feedback while the parent still has no attached buffer.
        self.surface.as_ref().context("root missing")?.commit();
        Ok(())
    }

    #[allow(dead_code)] // Only staged mode controls the transparent parent commit.
    pub fn publish_staged(&mut self) -> Result<()> {
        if let Some(tree) = &self.nested {
            tree.publish(self.configured.context("layer configure missing")?)?;
            self.staging_published = true;
            return Ok(());
        }
        ensure!(self.child.is_some(), "staged native child missing");
        let (width, height) = self.configured.context("layer configure missing")?;
        ensure!(
            width > 0
                && height > 0
                && width <= 16_384
                && height <= 16_384
                && u64::from(width) * u64::from(height) * 4 <= 256 * 1024 * 1024,
            "staged geometry exceeds limits"
        );
        let root = self.surface.as_ref().context("root missing")?;
        self.staging_viewport
            .as_ref()
            .context("root viewport missing")?
            .set_destination(width as i32, height as i32);
        root.attach(self.staging_buffer.as_ref(), 0, 0);
        root.damage_buffer(0, 0, 1, 1);
        root.commit();
        self.staging_published = true;
        Ok(())
    }
    #[allow(dead_code)] // Only the shared-session diagnostic creates a second root.
    pub fn add_secondary(&mut self, output: &str, qh: &QueueHandle<Self>) -> Result<()> {
        ensure!(
            self.secondary.is_none(),
            "secondary private surface already exists"
        );
        let output = self
            .outputs
            .values()
            .find(|(_, name)| name == output)
            .map(|(proxy, _)| proxy.clone())
            .context("secondary private output was not advertised")?;
        let surface = self
            .compositor
            .as_ref()
            .context("compositor missing")?
            .create_surface(qh, ());
        let empty = self
            .compositor
            .as_ref()
            .context("compositor missing")?
            .create_region(qh, ());
        surface.set_input_region(Some(&empty));
        empty.destroy();
        let layer = self
            .shell
            .as_ref()
            .context("layer shell missing")?
            .get_layer_surface(
                &surface,
                Some(&output),
                zwlr_layer_shell_v1::Layer::Overlay,
                "openwall-private-egl-secondary".into(),
                qh,
                Secondary(0),
            );
        layer.set_size(0, 0);
        layer.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
        surface.commit();
        self.secondary = Some(SecondarySurface {
            surface,
            layer,
            configured: None,
            dirty: true,
            pending_feedback: false,
            presented: 0,
            presented_frame_ids: Vec::new(),
            discarded: 0,
            closed: false,
        });
        Ok(())
    }

    #[allow(dead_code)] // Only the shared-session diagnostic creates a second root.
    pub fn secondary_feedback(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        self.secondary_frame_feedback(qh, 0)
    }

    #[allow(dead_code)] // Shared video labels each output's buffer independently.
    pub fn secondary_frame_feedback(&mut self, qh: &QueueHandle<Self>, id: u64) -> Result<()> {
        let secondary = self
            .secondary
            .as_mut()
            .context("secondary surface missing")?;
        self.presentation
            .as_ref()
            .context("presentation missing")?
            .feedback(&secondary.surface, qh, Secondary(id));
        secondary.pending_feedback = true;
        Ok(())
    }
    #[allow(dead_code)] // Video and staged diagnostics label their final buffers.
    pub fn request_feedback(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        self.request_frame_feedback(qh, 0)
    }

    #[allow(dead_code)] // The replacement proof keeps a previous root image mapped.
    pub fn enable_nested_staging(
        &mut self,
        qh: &QueueHandle<Self>,
        previous: [u8; 3],
        intercept: bool,
    ) -> Result<()> {
        ensure!(
            self.child.is_none() && self.nested.is_none(),
            "staging already initialized"
        );
        let size = self.configured.context("layer configure missing")?;
        let tree = nested::Tree::new(self, qh, previous, intercept)?;
        self.request_root_feedback(qh, 0)?;
        tree.show_previous(size)?;
        self.nested = Some(tree);
        Ok(())
    }

    fn request_root_feedback(&mut self, qh: &QueueHandle<Self>, id: u64) -> Result<()> {
        let surface = self.surface.as_ref().context("root missing")?.clone();
        self.request_surface_feedback(qh, &surface, id).map(|_| ())
    }

    #[allow(dead_code)] // Caller waits for candidate presentation before rollback.
    pub fn rollback_nested(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        ensure!(
            self.staging_published && !self.pending_feedback,
            "rollback requires confirmed publication"
        );
        self.request_root_feedback(qh, 2)?;
        self.nested
            .as_ref()
            .context("nested tree missing")?
            .rollback(self.configured.context("layer configure missing")?)?;
        self.staging_published = false;
        self.injected = false;
        self.dirty = false;
        Ok(())
    }

    #[allow(dead_code)] // Caller first drops the candidate's native EGL window.
    pub fn abort_nested(&mut self) -> Result<()> {
        ensure!(
            !self.staging_published,
            "cannot abort a published candidate"
        );
        self.nested.as_mut().context("nested tree missing")?.abort();
        self.dirty = false;
        Ok(())
    }

    pub fn request_frame_feedback(&mut self, qh: &QueueHandle<Self>, frame_id: u64) -> Result<()> {
        let surface = self.render_surface()?.clone();
        self.request_surface_feedback(qh, &surface, frame_id)
            .map(|_| ())
    }

    pub fn request_surface_feedback(
        &mut self,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        frame_id: u64,
    ) -> Result<Receipt> {
        let presentation = self
            .presentation
            .as_ref()
            .context("presentation feedback missing")?;
        let receipt = self.feedback_tracker.register(frame_id)?;
        presentation.feedback(surface, qh, frame_id);
        self.pending_feedback = true;
        Ok(receipt)
    }

    #[allow(dead_code)] // A failed candidate must not retain readiness authority.
    pub fn cancel_frame_feedback(&mut self, id: u64) -> bool {
        let cancelled = self.feedback_tracker.cancel(id);
        self.pending_feedback = self.feedback_tracker.has_active();
        cancelled
    }

    pub fn inject_if_ready(&mut self) {
        if self.injected
            || self.pending_feedback
            || self.dirty
            || self.presented == 0
            || !self.witness_mapped
            || self.pointer.is_none()
        {
            return;
        }
        if let Some(pointer) = &self.virtual_pointer {
            let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
            let millis = (now.tv_sec as u64 * 1000 + now.tv_nsec as u64 / 1_000_000) as u32;
            pointer.motion_absolute(millis, 500, 500, 1000, 1000);
            pointer.frame();
            pointer.button(millis, 0x110, wl_pointer::ButtonState::Pressed);
            pointer.frame();
            pointer.button(millis, 0x110, wl_pointer::ButtonState::Released);
            pointer.frame();
            self.injected = true;
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        // Caller drops EGLSurface and wl_egl_window before destroying these roots.
        self.secondary.take();
        self.nested.take();
        if let Some(role) = self.child_role.take() {
            role.destroy();
        }
        if let Some(surface) = self.child.take() {
            surface.destroy();
        }
        if let Some(viewport) = self.staging_viewport.take() {
            viewport.destroy();
        }
        if let Some(buffer) = self.staging_buffer.take() {
            buffer.destroy();
        }
        if let Some(pointer) = self.virtual_pointer.take() {
            pointer.destroy();
        }
        if let Some(pointer) = self.pointer.take() {
            pointer.release();
        }
        if let Some(layer) = self.layer.take() {
            layer.destroy();
        }
        if let Some(surface) = self.surface.take() {
            surface.destroy();
        }
        if let Some(viewport) = self.witness_viewport.take() {
            viewport.destroy();
        }
        if let Some(top) = self.witness_top.take() {
            top.destroy();
        }
        if let Some(role) = self.witness_role.take() {
            role.destroy();
        }
        if let Some(surface) = self.witness.take() {
            surface.destroy();
        }
        if let Some(buffer) = self.witness_buffer.take() {
            buffer.destroy();
        }
        if let Some(seat) = self.seat.take() {
            seat.release();
        }
        for (_, (output, _)) in std::mem::take(&mut self.outputs) {
            if output.version() >= 3 {
                output.release();
            }
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, Secondary> for State {
    fn event(
        state: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &Secondary,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(secondary) = state.secondary.as_mut() {
            match event {
                zwlr_layer_surface_v1::Event::Configure {
                    serial,
                    width,
                    height,
                } => {
                    layer.ack_configure(serial);
                    secondary.configured = Some((width, height));
                    secondary.dirty = true;
                }
                zwlr_layer_surface_v1::Event::Closed => secondary.closed = true,
                _ => {}
            }
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, Secondary> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        id: &Secondary,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(secondary) = state.secondary.as_mut() {
            match event {
                wp_presentation_feedback::Event::Presented { .. } => {
                    secondary.presented += 1;
                    if secondary.presented_frame_ids.len() < 256 {
                        secondary.presented_frame_ids.push(id.0);
                    }
                    secondary.pending_feedback = false;
                }
                wp_presentation_feedback::Event::Discarded => {
                    secondary.discarded += 1;
                    secondary.pending_feedback = false;
                    secondary.dirty = true;
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_subcompositor" => state.subcompositor = Some(registry.bind(name, 1, qh, ())),
                "wl_seat" if state.seat.is_none() && version >= 5 => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()))
                }
                "wl_output" => {
                    state.outputs.insert(
                        name,
                        (registry.bind(name, version.min(4), qh, name), String::new()),
                    );
                }
                "xdg_wm_base" => state.xdg = Some(registry.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => {
                    state.shell = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wp_viewporter" => state.viewporter = Some(registry.bind(name, 1, qh, ())),
                "wp_presentation" => state.presentation = Some(registry.bind(name, 1, qh, ())),
                "zwlr_virtual_pointer_manager_v1" if version >= 2 => {
                    state.virtual_manager = Some(registry.bind(name, 2, qh, ()))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some((_, current)) = state.outputs.get_mut(id)
        {
            *current = name;
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                if state.configured != Some((width, height)) {
                    state.injected = false;
                }
                state.configured = Some((width, height));
                state.dirty = true;
            }
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        shell: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            shell.pong(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                state.witness_size = (width.max(1), height.max(1))
            }
            xdg_toplevel::Event::Close => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        role: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            role.ack_configure(serial);
            if let (Some(viewport), Some(surface), Some(buffer)) = (
                &state.witness_viewport,
                &state.witness,
                &state.witness_buffer,
            ) {
                viewport.set_destination(state.witness_size.0.max(1), state.witness_size.1.max(1));
                surface.attach(Some(buffer), 0, 0);
                surface.damage_buffer(0, 0, 1, 1);
                surface.commit();
                state.witness_mapped = true;
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
            && capabilities.contains(wl_seat::Capability::Pointer)
            && state.pointer.is_none()
        {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { surface, .. } => state.pointer_focus = Some(surface),
            wl_pointer::Event::Leave { .. } => state.pointer_focus = None,
            wl_pointer::Event::Button { button: 0x110, .. } if state.injected => {
                if state.pointer_focus == state.witness {
                    state.witness_buttons += 1;
                }
                if state.pointer_focus == state.surface
                    || state
                        .render_surface()
                        .ok()
                        .is_some_and(|surface| Some(surface) == state.pointer_focus.as_ref())
                {
                    state.intercepted = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, u64> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        frame_id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented { .. } => {
                if state.feedback_tracker.complete(*frame_id, true).is_none() {
                    state.closed = true;
                    return;
                }
                state.presented += 1;
                // Diagnostic histories stay bounded even for a high-rate source.
                if state.presented_frame_ids.len() < 256 {
                    state.presented_frame_ids.push(*frame_id);
                }
                state.pending_feedback = state.feedback_tracker.has_active();
            }
            wp_presentation_feedback::Event::Discarded => {
                match state.feedback_tracker.complete(*frame_id, false) {
                    Some(Completion::Discarded) => {
                        state.discarded += 1;
                        if state.discarded_frame_ids.len() < 256 {
                            state.discarded_frame_ids.push(*frame_id);
                        }
                        state.dirty = true;
                    }
                    Some(Completion::CancelledDiscarded) => {}
                    _ => state.closed = true,
                }
                state.pending_feedback = state.feedback_tracker.has_active();
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_region::WlRegion);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(State: ignore wl_subsurface::WlSubsurface);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wp_viewport::WpViewport);
delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_presentation::WpPresentation);
delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(State: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
