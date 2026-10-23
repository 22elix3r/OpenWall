// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native XDG video parent. GStreamer owns only its embedded video subsurfaces.
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_subcompositor,
        wl_subsurface, wl_surface,
    },
};
use wayland_protocols::{
    wp::viewporter::client::{wp_viewport, wp_viewporter},
    xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};

use super::{black_buffer, layout::Layout};

#[derive(Default)]
struct State {
    // Player must stop before the parent/connection disappears.
    player: Option<crate::video::Player>,
    buffer: Option<wl_buffer::WlBuffer>,
    viewport: Option<wp_viewport::WpViewport>,
    container_viewport: Option<wp_viewport::WpViewport>,
    container_role: Option<wl_subsurface::WlSubsurface>,
    container: Option<wl_surface::WlSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    role: Option<xdg_surface::XdgSurface>,
    surface: Option<wl_surface::WlSurface>,
    compositor: Option<wl_compositor::WlCompositor>,
    subcompositor: Option<wl_subcompositor::WlSubcompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<xdg_wm_base::XdgWmBase>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    pending_size: Option<(u32, u32)>,
    size: Option<(u32, u32)>,
    bounds: Option<(u32, u32)>,
    dirty: bool,
    commit: bool,
    mapped: bool,
    closed: bool,
    error: Option<&'static str>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.player.take();
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        if let Some(viewport) = self.container_viewport.take() {
            viewport.destroy();
        }
        if let Some(role) = self.container_role.take() {
            role.destroy();
        }
        if let Some(surface) = self.container.take() {
            surface.destroy();
        }
        if let Some(toplevel) = self.toplevel.take() {
            toplevel.destroy();
        }
        if let Some(role) = self.role.take() {
            role.destroy();
        }
        if let Some(surface) = self.surface.take() {
            surface.destroy();
        }
        if let Some(shell) = self.shell.take() {
            shell.destroy();
        }
        if let Some(viewporter) = self.viewporter.take() {
            viewporter.destroy();
        }
        if let Some(subcompositor) = self.subcompositor.take() {
            subcompositor.destroy();
        }
    }
}

pub fn run(path: PathBuf, duration: Option<Duration>) -> Result<()> {
    let conn = Connection::connect_to_env().context("could not connect to Wayland")?;
    let mut queue = conn.new_event_queue();
    conn.display().get_registry(&queue.handle(), ());
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    let compositor = state
        .compositor
        .as_ref()
        .context("compositor is missing wl_compositor")?;
    let shell = state
        .shell
        .as_ref()
        .context("compositor is missing XDG shell")?;
    let subcompositor = state
        .subcompositor
        .as_ref()
        .context("compositor has no video subsurfaces")?;
    let surface = compositor.create_surface(&queue.handle(), ());
    let role = shell.get_xdg_surface(&surface, &queue.handle(), ());
    let toplevel = role.get_toplevel(&queue.handle(), ());
    toplevel.set_title(format!(
        "{} — OpenWall",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    toplevel.set_app_id("org.openwall.Worker".into());
    // GStreamer may commit its supplied parent during setup. Give it a separate
    // subsurface, so only this application can commit/ack the XDG root. This also
    // keeps that root unmapped until decoded video reaches the native sink.
    let container = compositor.create_surface(&queue.handle(), ());
    let container_role = subcompositor.get_subsurface(&container, &surface, &queue.handle(), ());
    container_role.set_desync();
    if let Some(manager) = &state.viewporter {
        state.viewport = Some(manager.get_viewport(&surface, &queue.handle(), ()));
        state.container_viewport = Some(manager.get_viewport(&container, &queue.handle(), ()));
    }
    // The initial commit has no buffer. The first attachment is allowed only after
    // xdg_surface.configure has been acknowledged on this same connection.
    surface.commit();
    state.surface = Some(surface);
    state.role = Some(role);
    state.toplevel = Some(toplevel);
    state.container = Some(container);
    state.container_role = Some(container_role);

    let (mut wake_read, wake_write) = UnixStream::pair()?;
    wake_read.set_nonblocking(true)?;
    wake_write.set_nonblocking(true)?;
    let wake_write = Arc::new(Mutex::new(wake_write));
    let stopped = Arc::new(AtomicBool::new(false));
    let signal = stopped.clone();
    let signal_writer = wake_write.clone();
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::Relaxed);
        if let Ok(mut writer) = signal_writer.lock() {
            let _ = writer.write_all(&[1]);
        }
    })?;
    let startup_deadline = Instant::now() + Duration::from_secs(15);
    let deadline = duration.map(|value| Instant::now() + value);
    loop {
        queue.dispatch_pending(&mut state)?;
        let mut bytes = [0u8; 64];
        while wake_read.read(&mut bytes).is_ok_and(|count| count > 0) {}
        if let Some(error) = state.error.take() {
            bail!(error);
        }
        if state.closed
            || stopped.load(Ordering::Relaxed)
            || deadline.is_some_and(|at| Instant::now() >= at)
        {
            break;
        }
        if !state.mapped && Instant::now() >= startup_deadline {
            bail!(
                "XDG video window did not configure and receive a video buffer within 15 seconds"
            );
        }
        if let Some(player) = &state.player {
            player.poll()?;
        }
        let synchronize_geometry = state.dirty && state.mapped;
        if synchronize_geometry {
            state
                .container_role
                .as_ref()
                .context("video window has no container role")?
                .set_sync();
        }
        if state.dirty {
            let (width, height) = state.size.context("video window has no configured size")?;
            // Uniform black does not need a high-DPI physical buffer. The video
            // child manages its own scaling; our parent uses logical coordinates.
            let layout = Layout::new((width, height), 1, None, state.viewport.is_some())?;
            let native = state
                .surface
                .as_ref()
                .context("video window has no native surface")?;
            let shm = state
                .shm
                .as_ref()
                .context("compositor has no shared-memory buffers")?;
            if let Some(viewport) = &state.viewport {
                viewport.set_destination(width as i32, height as i32);
            }
            if let Some(viewport) = &state.container_viewport {
                viewport.set_destination(width as i32, height as i32);
            }
            let black_size = if layout.viewport {
                (1, 1)
            } else {
                layout.buffer
            };
            let buffer = black_buffer(shm, &queue.handle(), black_size.0, black_size.1)?;
            if state.mapped {
                native.attach(Some(&buffer), 0, 0);
                native.damage(0, 0, width as i32, height as i32);
            }
            let container = state
                .container
                .as_ref()
                .context("video window has no container")?;
            container.attach(Some(&buffer), 0, 0);
            container.damage(0, 0, width as i32, height as i32);
            container.commit();
            if let Some(previous) = state.buffer.replace(buffer) {
                previous.destroy();
            }
            if let Some(player) = &state.player {
                player.resize(width, height)?;
            } else {
                // SAFETY: State::drop stops the pipeline before destroying the
                // surface, and this function owns the connection until then.
                state.player = Some(crate::video::Player::new(
                    &path,
                    Some((
                        conn.backend().display_ptr() as usize,
                        container.id().as_ptr() as usize,
                        width,
                        height,
                    )),
                    Some(wake_write.clone()),
                )?);
            }
            state
                .role
                .as_ref()
                .context("video window has no XDG surface")?
                .set_window_geometry(0, 0, width as i32, height as i32);
            state.dirty = false;
            state.commit = true;
            eprintln!(
                "Native video window configured (logical {width}×{height}, parent {}×{}, no scene GPU initialized)",
                black_size.0, black_size.1
            );
        }
        if !state.mapped
            && state
                .player
                .as_ref()
                .is_some_and(crate::video::Player::has_first_buffer)
        {
            // Keep the parent unmapped while decoding starts. A stalled decoder
            // must not turn a shell adapter's startup check into a black wallpaper.
            let native = state
                .surface
                .as_ref()
                .context("video window has no native surface")?;
            let buffer = state
                .buffer
                .as_ref()
                .context("video window has no parent buffer")?;
            let (width, height) = state.size.context("video window has no configured size")?;
            native.attach(Some(buffer), 0, 0);
            native.damage(0, 0, width as i32, height as i32);
            state.mapped = true;
            state.commit = true;
        }
        if state.commit && state.mapped {
            state
                .surface
                .as_ref()
                .context("video window has no native surface")?
                .commit();
            state.commit = false;
        }
        if synchronize_geometry {
            // Release the video subtree for independently paced playback after
            // applying its resize together with the XDG parent commit.
            state
                .container_role
                .as_ref()
                .context("video window has no container role")?
                .set_desync();
        }
        conn.flush()?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut fds = [
            PollFd::new(&conn, PollFlags::IN),
            PollFd::new(&wake_read, PollFlags::IN),
        ];
        let next_wake = deadline
            .into_iter()
            .chain((!state.mapped).then_some(startup_deadline))
            .min();
        let timeout = next_wake.map(|at| {
            let remaining = at.saturating_duration_since(Instant::now());
            Timespec {
                tv_sec: remaining.as_secs() as i64,
                tv_nsec: remaining.subsec_nanos() as i64,
            }
        });
        match poll(&mut fds, timeout.as_ref()) {
            Ok(_)
                if fds[0]
                    .revents()
                    .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) =>
            {
                bail!("Wayland compositor disconnected")
            }
            Ok(_) if fds[0].revents().contains(PollFlags::IN) => {
                openwall::presentation::wayland_io::read_events(guard)?;
            }
            Ok(_) | Err(rustix::io::Errno::INTR) => drop(guard),
            Err(error) => return Err(error.into()),
        }
    }
    drop(state);
    conn.flush()?;
    Ok(())
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
                    state.compositor = Some(registry.bind(name, version.min(6), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_subcompositor" => state.subcompositor = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => state.shell = Some(registry.bind(name, version.min(6), qh, ())),
                "wp_viewporter" => state.viewporter = Some(registry.bind(name, 1, qh, ())),
                _ => {}
            }
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
            let size = state
                .pending_size
                .take()
                .unwrap_or(state.size.unwrap_or((960, 540)));
            if state.size != Some(size) {
                state.size = Some(size);
                state.dirty = true;
            }
            state.commit = true;
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
                if width < 0 || height < 0 {
                    state.error = Some("compositor supplied negative window dimensions");
                    return;
                }
                let mut fallback = state.size.unwrap_or((960, 540));
                if let Some(bounds) = state.bounds {
                    fallback = (fallback.0.min(bounds.0), fallback.1.min(bounds.1));
                }
                state.pending_size = Some((
                    if width == 0 { fallback.0 } else { width as u32 },
                    if height == 0 {
                        fallback.1
                    } else {
                        height as u32
                    },
                ));
            }
            xdg_toplevel::Event::ConfigureBounds { width, height } if width > 0 && height > 0 => {
                state.bounds = Some((width as u32, height as u32))
            }
            xdg_toplevel::Event::Close => state.closed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(State: ignore wl_subsurface::WlSubsurface);
delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_viewport::WpViewport);
