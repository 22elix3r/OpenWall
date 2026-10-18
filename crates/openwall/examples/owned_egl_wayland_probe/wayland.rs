// SPDX-License-Identifier: AGPL-3.0-or-later
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, io::Write, os::fd::AsFd};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer, wl_compositor, wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
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
    pub surface: Option<wl_surface::WlSurface>,
    layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    witness: Option<wl_surface::WlSurface>,
    witness_role: Option<xdg_surface::XdgSurface>,
    witness_top: Option<xdg_toplevel::XdgToplevel>,
    witness_viewport: Option<wp_viewport::WpViewport>,
    witness_buffer: Option<wl_buffer::WlBuffer>,
    witness_size: (i32, i32),
    witness_mapped: bool,
    compositor: Option<wl_compositor::WlCompositor>,
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
    pub presented: u32,
    pub discarded: u32,
    pub injected: bool,
    pub witness_buttons: u32,
    pub intercepted: bool,
    pub closed: bool,
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
    pub fn request_feedback(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        self.presentation
            .as_ref()
            .context("presentation feedback missing")?
            .feedback(
                self.surface.as_ref().context("EGL surface missing")?,
                qh,
                (),
            );
        self.pending_feedback = true;
        Ok(())
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
                if state.pointer_focus == state.surface {
                    state.intercepted = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented { .. } => {
                state.presented += 1;
                state.pending_feedback = false;
            }
            wp_presentation_feedback::Event::Discarded => {
                state.discarded += 1;
                state.pending_feedback = false;
                state.dirty = true;
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
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wp_viewport::WpViewport);
delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_presentation::WpPresentation);
delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(State: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
