// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native layer-shell presentation. No XWayland window placement workaround.
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
        wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
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
    proxy: wl_output::WlOutput,
    info: DisplayInfo,
    configured: Option<(u32, u32)>,
    dirty: bool,
    closed: bool,
}

impl Drop for Output {
    fn drop(&mut self) {
        #[cfg(feature = "video")]
        self.video.take();
        if let Some(buffer) = self.black.take() {
            buffer.destroy();
        }
        self.surface.take();
        self.painter.take();
        if let Some(layer) = self.layer.take() {
            layer.destroy();
        }
        if let Some(surface) = self.native.take() {
            surface.destroy();
        }
    }
}

struct State {
    outputs: BTreeMap<u32, Output>,
    compositor: Option<wl_compositor::WlCompositor>,
    shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    shm: Option<wl_shm::WlShm>,
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
    #[cfg(not(feature = "video"))]
    let _ = &video_path;
    let (conn, mut queue, mut state) = connect()?;
    let compositor = state
        .compositor
        .clone()
        .context("compositor is missing wl_compositor")?;
    let shell = state.shell.clone().context("this compositor has no layer-shell; GNOME and Plasma require dedicated integrations (not implemented yet)")?;
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
            if item.native.is_none() {
                let native = compositor.create_surface(&queue.handle(), ());
                let region = compositor.create_region(&queue.handle(), ());
                native.set_input_region(Some(&region));
                region.destroy();
                native.set_buffer_scale(item.info.scale.max(1));
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
                && let Some((width, height)) = item.configured
            {
                let native = item.native.as_ref().context("missing Wayland surface")?;
                let scale = item.info.scale.max(1) as u32;
                native.set_buffer_scale(scale as i32);
                #[cfg(feature = "video")]
                if let Some(path) = &video_path {
                    let shm = shm
                        .as_ref()
                        .context("compositor has no shared-memory buffers")?;
                    let buffer = black_buffer(shm, &queue.handle(), width, height, scale)?;
                    native.attach(Some(&buffer), 0, 0);
                    native.damage(0, 0, width as i32, height as i32);
                    native.commit();
                    item.video.take();
                    if let Some(previous) = item.black.replace(buffer) {
                        previous.destroy();
                    }
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
                    item.dirty = false;
                    eprintln!(
                        "Native video attached on {} (no scene GPU initialized)",
                        item.info.name
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
                let config = gpu.configure(surface, width * scale, height * scale)?;
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
                item.dirty = !painter.present(gpu, surface)?;
                eprintln!(
                    "Wallpaper presented on {} ({}×{}, scale {})",
                    item.info.name, config.width, config.height, scale
                );
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
        let timeout = deadline.map(|x| {
            let d = x.saturating_duration_since(Instant::now());
            Timespec {
                tv_sec: d.as_secs() as i64,
                tv_nsec: d.subsec_nanos() as i64,
            }
        });
        match poll(&mut fds, timeout.as_ref()) {
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
                            black: None,
                            configured: None,
                            dirty: true,
                            closed: false,
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
            wl_output::Event::Name { name } => item.info.name = name,
            wl_output::Event::Description { description } => item.info.description = description,
            wl_output::Event::Scale { factor } => {
                item.info.scale = factor.max(1);
                item.dirty = true;
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
                    item.configured = Some((
                        if width == 0 {
                            (item.info.width / item.info.scale.max(1) as u32).max(1)
                        } else {
                            width
                        },
                        if height == 0 {
                            (item.info.height / item.info.scale.max(1) as u32).max(1)
                        } else {
                            height
                        },
                    ));
                    item.dirty = true;
                }
                zwlr_layer_surface_v1::Event::Closed => {
                    item.closed = true;
                    #[cfg(feature = "video")]
                    item.video.take();
                    item.surface.take();
                    item.painter.take();
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_region::WlRegion);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);

#[cfg(feature = "video")]
fn black_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    width: u32,
    height: u32,
    scale: u32,
) -> Result<wl_buffer::WlBuffer> {
    use std::os::fd::AsFd;
    let width = width.checked_mul(scale).context("output width overflow")?;
    let height = height
        .checked_mul(scale)
        .context("output height overflow")?;
    let bytes = u64::from(width) * u64::from(height) * 4;
    anyhow::ensure!(
        width <= 16384 && height <= 16384 && bytes <= 256 * 1024 * 1024,
        "output exceeds presentation buffer limits"
    );
    let mut file = tempfile::tempfile()?;
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
