// SPDX-License-Identifier: AGPL-3.0-or-later
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use openwall_core::Scene;
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};

use crate::gpu::{Gpu, Painter};

pub fn run(
    scene: Scene,
    duration: Option<Duration>,
    desktop: bool,
    output: Option<String>,
) -> Result<()> {
    run_content(scene, duration, desktop, output, None)
}

#[cfg(target_os = "macos")]
pub fn run_video(
    path: PathBuf,
    duration: Option<Duration>,
    desktop: bool,
    output: Option<String>,
) -> Result<()> {
    let scene = Scene {
        report: openwall_core::CompatibilityReport {
            title: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
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
    run_content(scene, duration, desktop, output, Some(path))
}

fn run_content(
    scene: Scene,
    duration: Option<Duration>,
    desktop: bool,
    output: Option<String>,
    media: Option<PathBuf>,
) -> Result<()> {
    let mut builder = EventLoop::<()>::with_user_event();
    #[cfg(target_os = "macos")]
    if desktop {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = builder.build()?;
    let proxy = event_loop.create_proxy();
    ctrlc::set_handler(move || {
        let _ = proxy.send_event(());
    })?;
    let mut app = Preview {
        scene,
        window: None,
        surface: None,
        gpu: None,
        painter: None,
        deadline: duration.map(|x| Instant::now() + x),
        error: None,
        desktop,
        output,
        media,
        started: Instant::now(),
        #[cfg(target_os = "macos")]
        mac_player: None,
    };
    event_loop.run_app(&mut app)?;
    if let Some(error) = app.error {
        return Err(error);
    }
    Ok(())
}

struct Preview {
    #[cfg(target_os = "macos")]
    mac_player: Option<crate::macos_video::Player>,
    scene: Scene,
    // The GPU surface retains an Arc<Window>, keeping the native view alive.
    surface: Option<wgpu::Surface<'static>>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    painter: Option<Painter>,
    deadline: Option<Instant>,
    error: Option<anyhow::Error>,
    desktop: bool,
    output: Option<String>,
    media: Option<PathBuf>,
    started: Instant,
}

impl Preview {
    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let mut attributes = Window::default_attributes()
            .with_title(format!("{} — OpenWall", self.scene.report.title))
            .with_inner_size(LogicalSize::new(960.0, 540.0))
            .with_visible(false);
        if self.desktop {
            let monitor = if let Some(name) = &self.output {
                event_loop
                    .available_monitors()
                    .find(|m| m.name().as_deref() == Some(name.as_str()))
                    .with_context(|| format!("display not found: {name}"))?
            } else {
                event_loop
                    .primary_monitor()
                    .or_else(|| event_loop.available_monitors().next())
                    .context("no displays found")?
            };
            attributes = attributes
                .with_decorations(false)
                .with_active(false)
                .with_inner_size(monitor.size())
                .with_position(monitor.position());
        }
        let window = Arc::new(event_loop.create_window(attributes)?);
        #[cfg(target_os = "macos")]
        if self.desktop {
            configure_macos_desktop(&window)?;
        }
        #[cfg(target_os = "macos")]
        if let Some(path) = &self.media {
            self.mac_player = Some(crate::macos_video::Player::new(&window, path)?);
            window.set_visible(true);
            self.window = Some(window);
            return Ok(());
        }
        let instance = Gpu::instance();
        let surface = instance.create_surface(window.clone())?;
        let gpu = pollster::block_on(Gpu::new(instance, Some(&surface)))?;
        let size = window.inner_size();
        let config = gpu.configure(&surface, size.width, size.height)?;
        self.painter = Some(Painter::new(
            &gpu,
            &self.scene,
            config.format,
            config.width,
            config.height,
        ));
        self.surface = Some(surface);
        self.gpu = Some(gpu);
        window.set_visible(true);
        window.request_redraw();
        self.window = Some(window);
        Ok(())
    }
}

impl ApplicationHandler<()> for Preview {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none()
            && let Err(error) = self.initialize(event_loop)
        {
            self.error = Some(error);
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _: ()) {
        event_loop.exit();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let result = (|| -> Result<()> {
            match event {
                WindowEvent::CloseRequested => event_loop.exit(),
                WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                    if let (Some(gpu), Some(surface)) = (&self.gpu, &self.surface) {
                        let config = gpu.configure(surface, size.width, size.height)?;
                        if self
                            .painter
                            .as_ref()
                            .is_none_or(|painter| painter.format != config.format)
                        {
                            self.painter = Some(Painter::new(
                                gpu,
                                &self.scene,
                                config.format,
                                size.width,
                                size.height,
                            ));
                        } else if let Some(painter) = &self.painter {
                            painter.resize(gpu, &self.scene, size.width, size.height);
                        }
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
                WindowEvent::RedrawRequested => {
                    if let (Some(painter), Some(gpu), Some(surface)) =
                        (&self.painter, &self.gpu, &self.surface)
                    {
                        painter.present(gpu, surface)?;
                    }
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.error = Some(error);
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(not(target_os = "macos"))]
        let _ = (&self.media, &self.started);
        #[cfg(target_os = "macos")]
        if let Some(player) = &self.mac_player {
            match player.ready() {
                Err(error) => {
                    self.error = Some(error);
                    event_loop.exit();
                    return;
                }
                Ok(false) if self.started.elapsed() > Duration::from_secs(15) => {
                    self.error = Some(anyhow::anyhow!(
                        "video did not become ready within 15 seconds"
                    ));
                    event_loop.exit();
                    return;
                }
                Ok(false) => {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(
                        Instant::now() + Duration::from_millis(100),
                    ));
                    return;
                }
                Ok(true) => {}
            }
        }
        if let Some(deadline) = self.deadline {
            if Instant::now() >= deadline {
                event_loop.exit();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

#[cfg(target_os = "macos")]
fn configure_macos_desktop(window: &Window) -> Result<()> {
    use objc2_app_kit::{NSView, NSWindowCollectionBehavior};
    use objc2_core_graphics::{CGWindowLevelForKey, CGWindowLevelKey};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let RawWindowHandle::AppKit(handle) = window.window_handle()?.as_raw() else {
        anyhow::bail!("not an AppKit window")
    };
    // SAFETY: winit owns a live NSView and this callback runs on the AppKit main thread.
    let view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
    let native = view.window().context("native view has no window")?;
    native.setLevel(CGWindowLevelForKey(CGWindowLevelKey::DesktopWindowLevelKey) as isize);
    native.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    native.setIgnoresMouseEvents(true);
    native.setHasShadow(false);
    Ok(())
}
