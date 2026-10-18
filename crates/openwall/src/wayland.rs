// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native layer-shell presentation. No XWayland window placement workaround.
mod layout;
mod recovery;
#[cfg(feature = "video")]
pub mod video_window;

use layout::Layout;
use recovery::Recovery;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    ptr::NonNull,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use openwall_core::Scene;
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use serde::Serialize;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_shm,
        wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::wp::{
    fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1},
    presentation_time::client::{wp_presentation, wp_presentation_feedback},
    viewporter::client::{wp_viewport, wp_viewporter},
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::gpu::{Gpu, Painter};

#[derive(Clone, Debug, Serialize, Default)]
pub struct DisplayInfo {
    pub name: String,
    pub description: String,
    pub width: u32,
    pub height: u32,
    pub scale: i32,
}

struct Output {
    #[cfg(feature = "video")]
    video: Option<crate::video::Player>,
    black: Option<wl_buffer::WlBuffer>,
    // Drop GPU presentation before destroying the native wl_surface.
    surface: Option<wgpu::Surface<'static>>,
    painter: Option<Painter>,
    layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    native: Option<wl_surface::WlSurface>,
    viewport: Option<wp_viewport::WpViewport>,
    fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    fractional_scale: Option<u32>,
    preferred_integer_scale: Option<i32>,
    proxy: wl_output::WlOutput,
    info: DisplayInfo,
    configured: Option<(u32, u32)>,
    dirty: bool,
    closed: bool,
    rotated: bool,
    retry_at: Option<Instant>,
    retry_count: u32,
    recovery: Recovery,
}

impl Output {
    fn release_surface(&mut self) {
        #[cfg(feature = "video")]
        self.video.take();
        if let Some(buffer) = self.black.take() {
            buffer.destroy();
        }
        self.surface.take();
        self.painter.take();
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        if let Some(fractional) = self.fractional.take() {
            fractional.destroy();
        }
        if let Some(layer) = self.layer.take() {
            layer.destroy();
        }
        if let Some(surface) = self.native.take() {
            surface.destroy();
        }
        self.configured = None;
        self.fractional_scale = None;
        self.preferred_integer_scale = None;
        self.retry_at = None;
        self.recovery = Recovery::default();
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.release_surface();
        if self.proxy.version() >= 3 {
            self.proxy.release();
        }
    }
}

struct State {
    outputs: BTreeMap<u32, Output>,
    compositor: Option<wl_compositor::WlCompositor>,
    shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    shm: Option<wl_shm::WlShm>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    fractional_manager: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    presentation: Option<wp_presentation::WpPresentation>,
}

fn connect() -> Result<(Connection, wayland_client::EventQueue<State>, State)> {
    let conn = Connection::connect_to_env().context("could not connect to Wayland")?;
    let mut queue = conn.new_event_queue();
    conn.display().get_registry(&queue.handle(), ());
    let mut state = State {
        outputs: BTreeMap::new(),
        compositor: None,
        shell: None,
        shm: None,
        viewporter: None,
        fractional_manager: None,
        presentation: None,
    };
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    Ok((conn, queue, state))
}

pub fn displays() -> Result<(Vec<DisplayInfo>, bool)> {
    let (_, _, state) = connect()?;
    Ok((
        state.outputs.values().map(|x| x.info.clone()).collect(),
        state.shell.is_some(),
    ))
}

pub fn run(scene: Scene, output: Option<String>, duration: Option<Duration>) -> Result<()> {
    run_content(scene, output, duration, None)
}

#[cfg(feature = "video")]
pub fn run_video(path: PathBuf, output: Option<String>, duration: Option<Duration>) -> Result<()> {
    let scene = Scene {
        report: openwall_core::CompatibilityReport {
            title: "Video wallpaper".into(),
            kind: "video".into(),
            width: 1280,
            height: 720,
            layers: 0,
            packed_entries: 0,
            unsupported: Vec::new(),
        },
        background: [0.0, 0.0, 0.0, 1.0],
        layers: Vec::new(),
    };
    run_content(scene, output, duration, Some(path))
}

fn run_content(
    scene: Scene,
    output: Option<String>,
    duration: Option<Duration>,
    video_path: Option<PathBuf>,
) -> Result<()> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let shell_owned = desktop.split([':', ';']).any(|name| {
        matches!(
            name.trim().to_ascii_lowercase().as_str(),
            "gnome" | "gnome-classic" | "kde" | "plasma"
        )
    });
    anyhow::ensure!(
        !shell_owned,
        "{desktop} requires a dedicated desktop integration; the experimental adapters are not connected to Apply yet"
    );
    #[cfg(not(feature = "video"))]
    let _ = &video_path;
    let (conn, mut queue, mut state) = connect()?;
    let compositor = state
        .compositor
        .clone()
        .context("compositor is missing wl_compositor")?;
    let shell = state.shell.clone().context("this compositor has no layer-shell; GNOME and Plasma require desktop adapters (Apply integration pending)")?;
    if let Some(name) = &output
        && !state.outputs.values().any(|x| &x.info.name == name)
    {
        bail!("display not found: {name}");
    }
    let instance = video_path.is_none().then(Gpu::instance);
    let mut gpu: Option<Gpu> = None;
    let (mut wake_read, wake_write) = UnixStream::pair()?;
    wake_read.set_nonblocking(true)?;
    wake_write.set_nonblocking(true)?;
    let wake_write = Arc::new(Mutex::new(wake_write));
    let stopped = Arc::new(AtomicBool::new(false));
    let signal_stopped = stopped.clone();
    let signal_writer = wake_write.clone();
    ctrlc::set_handler(move || {
        signal_stopped.store(true, Ordering::Relaxed);
        if let Ok(mut writer) = signal_writer.lock() {
            let _ = writer.write_all(&[1]);
        }
    })?;
    let deadline = duration.map(|x| Instant::now() + x);
    loop {
        queue.dispatch_pending(&mut state)?;
        let mut wake_bytes = [0u8; 64];
        while wake_read.read(&mut wake_bytes).is_ok_and(|count| count > 0) {}
        if stopped.load(Ordering::Relaxed) || deadline.is_some_and(|x| Instant::now() >= x) {
            break;
        }
        let shm = state.shm.clone();
        let viewporter = state.viewporter.clone();
        let fractional_manager = state.fractional_manager.clone();
        let presentation = state.presentation.clone();
        #[cfg(not(feature = "video"))]
        let _ = &shm;
        for (id, item) in &mut state.outputs {
            #[cfg(feature = "video")]
            if let Some(video) = &item.video {
                video.poll()?;
            }
            if item.closed || output.as_ref().is_some_and(|name| name != &item.info.name) {
                continue;
            }
            if item.recovery.take_due(Instant::now()) {
                item.dirty = true;
            }
            if item.native.is_none() {
                let native = compositor.create_surface(&queue.handle(), *id);
                let region = compositor.create_region(&queue.handle(), ());
                native.set_input_region(Some(&region));
                region.destroy();
                if let Some(manager) = &viewporter {
                    item.viewport = Some(manager.get_viewport(&native, &queue.handle(), ()));
                    if let Some(manager) = &fractional_manager {
                        item.fractional =
                            Some(manager.get_fractional_scale(&native, &queue.handle(), *id));
                    }
                }
                let layer = shell.get_layer_surface(
                    &native,
                    Some(&item.proxy),
                    zwlr_layer_shell_v1::Layer::Background,
                    "openwall".into(),
                    &queue.handle(),
                    *id,
                );
                layer.set_anchor(
                    zwlr_layer_surface_v1::Anchor::Top
                        | zwlr_layer_surface_v1::Anchor::Bottom
                        | zwlr_layer_surface_v1::Anchor::Left
                        | zwlr_layer_surface_v1::Anchor::Right,
                );
                layer.set_exclusive_zone(-1);
                layer
                    .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
                layer.set_size(0, 0);
                native.commit();
                item.native = Some(native);
                item.layer = Some(layer);
            }
            if item.dirty
                && item.retry_at.is_none_or(|at| Instant::now() >= at)
                && let Some((width, height)) = item.configured
            {
                let native = item.native.as_ref().context("missing Wayland surface")?;
                let integer_scale = if native.version() >= 3 {
                    item.preferred_integer_scale.unwrap_or(item.info.scale)
                } else {
                    1
                };
                let layout = Layout::new(
                    (width, height),
                    integer_scale,
                    item.fractional_scale,
                    item.viewport.is_some(),
                )?;
                if native.version() >= 3 {
                    native.set_buffer_scale(layout.buffer_scale);
                }
                if let Some(viewport) = &item.viewport {
                    viewport.set_destination(width as i32, height as i32);
                }
                #[cfg(feature = "video")]
                if let Some(path) = &video_path {
                    let shm = shm
                        .as_ref()
                        .context("compositor has no shared-memory buffers")?;
                    // A viewport stretches a single black pixel behind the video subsurface.
                    let black_size = if layout.viewport {
                        (1, 1)
                    } else {
                        layout.buffer
                    };
                    let buffer = black_buffer(shm, &queue.handle(), black_size.0, black_size.1)?;
                    native.attach(Some(&buffer), 0, 0);
                    native.damage(0, 0, width as i32, height as i32);
                    native.commit();
                    if let Some(previous) = item.black.replace(buffer) {
                        previous.destroy();
                    }
                    if let Some(video) = &item.video {
                        video.resize(width, height)?;
                    } else {
                        item.video = Some(crate::video::Player::new(
                            path,
                            Some((
                                conn.backend().display_ptr() as usize,
                                native.id().as_ptr() as usize,
                                width,
                                height,
                            )),
                            Some(wake_write.clone()),
                        )?);
                    }
                    item.dirty = false;
                    item.retry_at = None;
                    eprintln!(
                        "Native video attached on {} (logical {}×{}, no scene GPU initialized)",
                        item.info.name, width, height
                    );
                    continue;
                }
                let instance = instance.as_ref().context("scene GPU instance is missing")?;
                if item.surface.is_none() {
                    let display = WaylandDisplayHandle::new(
                        NonNull::new(conn.backend().display_ptr().cast())
                            .context("null Wayland display")?,
                    );
                    let window = WaylandWindowHandle::new(
                        NonNull::new(native.id().as_ptr().cast())
                            .context("null Wayland surface")?,
                    );
                    // SAFETY: conn and native outlive every GPU surface. Output::drop explicitly
                    // drops GPU presentation first; the connection remains owned by this function.
                    let surface = unsafe {
                        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                            raw_display_handle: Some(RawDisplayHandle::Wayland(display)),
                            raw_window_handle: RawWindowHandle::Wayland(window),
                        })
                    }?;
                    if gpu.is_none() {
                        gpu = Some(pollster::block_on(Gpu::new(
                            instance.clone(),
                            Some(&surface),
                        ))?);
                    }
                    item.surface = Some(surface);
                }
                let gpu = gpu.as_ref().context("GPU not initialized")?;
                let surface = item.surface.as_ref().context("surface not initialized")?;
                let config = gpu.configure(surface, layout.buffer.0, layout.buffer.1)?;
                if item
                    .painter
                    .as_ref()
                    .is_none_or(|painter| painter.format != config.format)
                {
                    item.painter = Some(Painter::new(
                        gpu,
                        &scene,
                        config.format,
                        config.width,
                        config.height,
                    ));
                } else if let Some(painter) = &item.painter {
                    painter.resize(gpu, &scene, config.width, config.height);
                }
                let painter = item
                    .painter
                    .as_ref()
                    .context("painter was not initialized")?;
                if item.recovery.request_callback() {
                    if let Some(presentation) = &presentation {
                        presentation.feedback(native, &queue.handle(), *id);
                    } else {
                        native.frame(&queue.handle(), *id);
                    }
                }
                item.dirty = !painter.present(gpu, surface)?;
                if item.dirty {
                    item.retry_count = item.retry_count.saturating_add(1);
                    item.retry_at = Some(
                        Instant::now()
                            + Duration::from_millis((50u64 << item.retry_count.min(5)).min(1000)),
                    );
                } else {
                    item.retry_count = 0;
                    item.retry_at = None;
                    item.recovery.submitted(Instant::now());
                    eprintln!(
                        "Wallpaper presented on {} ({}×{}, logical {}×{})",
                        item.info.name, config.width, config.height, width, height
                    );
                }
            }
        }
        conn.flush()?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut fds = [
            PollFd::new(&conn, PollFlags::IN),
            PollFd::new(&wake_read, PollFlags::IN),
        ];
        let next_wake = state
            .outputs
            .values()
            .filter_map(|item| item.retry_at)
            .chain(
                state
                    .outputs
                    .values()
                    .filter_map(|item| item.recovery.deadline()),
            )
            .chain(deadline)
            .min();
        let timeout = next_wake.map(|x| {
            let d = x.saturating_duration_since(Instant::now());
            Timespec {
                tv_sec: d.as_secs() as i64,
                tv_nsec: d.subsec_nanos() as i64,
            }
        });
        match poll(&mut fds, timeout.as_ref()) {
            Ok(_)
                if fds[0]
                    .revents()
                    .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) =>
            {
                bail!("Wayland compositor disconnected");
            }
            Ok(_) if fds[0].revents().contains(PollFlags::IN) => {
                guard.read()?;
            }
            Ok(_) => drop(guard),
            Err(rustix::io::Errno::INTR) => drop(guard),
            Err(error) => return Err(error.into()),
        }
    }
    state.outputs.clear();
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
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(6), qh, ()))
                }
                "zwlr_layer_shell_v1" => {
                    state.shell = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
                "wp_viewporter" => state.viewporter = Some(registry.bind(name, 1, qh, ())),
                "wp_fractional_scale_manager_v1" => {
                    state.fractional_manager = Some(registry.bind(name, 1, qh, ()))
                }
                "wp_presentation" => state.presentation = Some(registry.bind(name, 1, qh, ())),
                "wl_output" => {
                    let proxy = registry.bind(name, version.min(4), qh, name);
                    state.outputs.insert(
                        name,
                        Output {
                            proxy,
                            info: DisplayInfo {
                                name: format!("output-{name}"),
                                scale: 1,
                                ..Default::default()
                            },
                            surface: None,
                            painter: None,
                            layer: None,
                            native: None,
                            viewport: None,
                            fractional: None,
                            fractional_scale: None,
                            preferred_integer_scale: None,
                            black: None,
                            configured: None,
                            dirty: true,
                            closed: false,
                            rotated: false,
                            retry_at: None,
                            retry_count: 0,
                            recovery: Recovery::default(),
                            #[cfg(feature = "video")]
                            video: None,
                        },
                    );
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.remove(&name);
            }
            _ => {}
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
        let Some(item) = state.outputs.get_mut(id) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                transform: WEnum::Value(transform),
                ..
            } => {
                item.rotated = matches!(
                    transform,
                    wl_output::Transform::_90
                        | wl_output::Transform::_270
                        | wl_output::Transform::Flipped90
                        | wl_output::Transform::Flipped270
                );
            }
            wl_output::Event::Name { name } => item.info.name = name,
            wl_output::Event::Description { description } => item.info.description = description,
            wl_output::Event::Scale { factor } => {
                let factor = factor.max(1);
                if item.info.scale != factor {
                    item.info.scale = factor;
                    item.dirty = true;
                    item.retry_at = None;
                }
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => {
                item.info.width = width.max(1) as u32;
                item.info.height = height.max(1) as u32;
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(item) = state.outputs.get_mut(id) {
            match event {
                zwlr_layer_surface_v1::Event::Configure {
                    serial,
                    width,
                    height,
                } => {
                    layer.ack_configure(serial);
                    let (fallback_width, fallback_height) = if item.rotated {
                        (item.info.height, item.info.width)
                    } else {
                        (item.info.width, item.info.height)
                    };
                    let configured = Some((
                        if width == 0 {
                            (fallback_width / item.info.scale.max(1) as u32).max(1)
                        } else {
                            width
                        },
                        if height == 0 {
                            (fallback_height / item.info.scale.max(1) as u32).max(1)
                        } else {
                            height
                        },
                    ));
                    if item.configured != configured {
                        item.configured = configured;
                        item.dirty = true;
                        item.retry_at = None;
                    }
                }
                zwlr_layer_surface_v1::Event::Closed => {
                    item.closed = true;
                    item.release_surface();
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_region::WlRegion);
impl Dispatch<wl_callback::WlCallback, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(item) = state.outputs.get_mut(id) {
            item.recovery.callback_done();
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, u32> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation_feedback::Event::Presented { .. } = event
            && let Some(item) = state.outputs.get_mut(id)
        {
            item.recovery.callback_done();
        }
    }
}

delegate_noop!(State: ignore wp_presentation::WpPresentation);
impl Dispatch<wl_surface::WlSurface, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event
            && let Some(item) = state.outputs.get_mut(id)
            && item.preferred_integer_scale != Some(factor.max(1))
        {
            item.preferred_integer_scale = Some(factor.max(1));
            item.dirty = true;
            item.retry_at = None;
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(item) = state.outputs.get_mut(id)
            && item.fractional_scale != Some(scale)
        {
            item.fractional_scale = Some(scale);
            item.dirty = true;
            item.retry_at = None;
        }
    }
}

delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_viewport::WpViewport);
delegate_noop!(State: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);

#[cfg(feature = "video")]
fn black_buffer<D>(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<D>,
    width: u32,
    height: u32,
) -> Result<wl_buffer::WlBuffer>
where
    D: Dispatch<wl_shm_pool::WlShmPool, ()> + Dispatch<wl_buffer::WlBuffer, ()> + 'static,
{
    use std::os::fd::AsFd;
    let bytes = u64::from(width) * u64::from(height) * 4;
    anyhow::ensure!(
        width > 0 && height > 0 && width <= 16384 && height <= 16384 && bytes <= 256 * 1024 * 1024,
        "output exceeds presentation buffer limits"
    );
    // Anonymous memory backing works in read-only namespaces and does not depend
    // on the host's temporary filesystem. The compositor receives its own FD.
    let fd = rustix::fs::memfd_create(c"openwall-black", rustix::fs::MemfdFlags::CLOEXEC)?;
    let mut file = std::fs::File::from(fd);
    file.write_all(&0xff000000u32.to_ne_bytes().repeat((bytes / 4) as usize))?;
    let pool = shm.create_pool(file.as_fd(), bytes as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        (width * 4) as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    Ok(buffer)
}
