// SPDX-License-Identifier: AGPL-3.0-or-later
use super::gl_api::Api;
use anyhow::{Context, Result, ensure};
use glow::HasContext;
use khronos_egl as egl;
use std::{cell::Cell, rc::Rc};
use wayland_client::{Connection, Proxy, protocol::wl_surface::WlSurface};

#[derive(Default)]
pub struct Counts {
    displays: Cell<u32>,
    contexts: Cell<u32>,
    surfaces: Cell<u32>,
    destroyed_surfaces: Cell<u32>,
    destroyed_contexts: Cell<u32>,
    terminated: Cell<u32>,
}
impl Counts {
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({"display_initializations":self.displays.get(),"context_creations":self.contexts.get(),
            "surface_creations":self.surfaces.get(),"surface_destructions":self.destroyed_surfaces.get(),
            "context_destructions":self.destroyed_contexts.get(),"display_terminations":self.terminated.get()})
    }
}

// One display/context owner per Wayland connection. Rc and Cell keep every native
// operation on the creating thread; windows retain it until their surfaces are gone.
pub struct Session {
    api: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    config: Option<egl::Config>,
    context: Option<egl::Context>,
    gl: Option<glow::Context>,
    selected: Api,
    counts: Rc<Counts>,
    initial_window_draw_buffer: Cell<Option<i32>>,
    rgba_bits: Option<[i32; 4]>,
    _connection: Connection,
}

impl Session {
    pub fn new(connection: &Connection, selected: Api) -> Result<Rc<Self>> {
        // SAFETY: load the system ABI; its library handle outlives native resources.
        let api = unsafe {
            egl::DynamicInstance::<egl::EGL1_5>::load_required_from_filename("libEGL.so.1")
        }?;
        // SAFETY: EGL_KHR_platform_wayland receives this retained connection's pointer.
        let display = unsafe {
            api.get_platform_display(
                0x31d8,
                connection.backend().display_ptr().cast(),
                &[egl::ATTRIB_NONE],
            )
        }?;
        api.initialize(display)?;
        let counts = Rc::new(Counts::default());
        counts.displays.set(1);
        let mut session = Self {
            api,
            display,
            config: None,
            context: None,
            gl: None,
            selected,
            counts,
            initial_window_draw_buffer: Cell::new(None),
            rgba_bits: None,
            _connection: connection.clone(),
        };
        session.api.bind_api(selected.egl_api())?;
        let config = session
            .api
            .choose_first_config(
                display,
                &[
                    egl::SURFACE_TYPE,
                    egl::WINDOW_BIT,
                    egl::RENDERABLE_TYPE,
                    selected.renderable(),
                    egl::RED_SIZE,
                    8,
                    egl::GREEN_SIZE,
                    8,
                    egl::BLUE_SIZE,
                    8,
                    egl::ALPHA_SIZE,
                    8,
                    egl::NONE,
                ],
            )?
            .context("no Wayland EGL configuration for the selected GL API")?;
        session.config = Some(config);
        let mut rgba_bits = [0; 4];
        for (value, attribute) in rgba_bits.iter_mut().zip([
            egl::RED_SIZE,
            egl::GREEN_SIZE,
            egl::BLUE_SIZE,
            egl::ALPHA_SIZE,
        ]) {
            *value = session.api.get_config_attrib(display, config, attribute)?;
        }
        session.rgba_bits = Some(rgba_bits);
        session.context = Some(session.api.create_context(
            display,
            config,
            None,
            selected.context_attributes(),
        )?);
        session.counts.contexts.set(1);
        // A live context with no output is required for resource cleanup and later
        // output recreation. Fail this diagnostic if the driver cannot support it.
        session
            .surfaceless()
            .context("owned EGL session requires a surfaceless current context")?;
        // SAFETY: this session's context is current and its EGL library is retained.
        session.gl = Some(unsafe {
            glow::Context::from_loader_function(|name| {
                session
                    .api
                    .get_proc_address(name)
                    .map_or(std::ptr::null(), |function| function as *const _)
            })
        });
        Ok(Rc::new(session))
    }

    pub fn counts(&self) -> Rc<Counts> {
        self.counts.clone()
    }

    pub fn surfaceless(&self) -> Result<()> {
        self.api
            .make_current(self.display, None, None, self.context)?;
        Ok(())
    }

    pub fn gl(&self) -> Result<&glow::Context> {
        self.gl.as_ref().context("GL context missing")
    }

    #[allow(dead_code)] // GL borrowing is needed by the video diagnostic.
    pub fn handles(&self) -> Result<(egl::Display, egl::Context)> {
        Ok((self.display, self.context.context("EGL context missing")?))
    }

    pub fn report(&self) -> Result<serde_json::Value> {
        let gl = self.gl()?;
        // SAFETY: a window or surfaceless binding keeps this context current.
        Ok(unsafe {
            serde_json::json!({"api":self.selected.name(),"vendor":gl.get_parameter_string(glow::VENDOR),
            "renderer":gl.get_parameter_string(glow::RENDERER),"version":gl.get_parameter_string(glow::VERSION),
            "rgba_bits":self.rgba_bits,
            "initial_window_draw_buffer":self.initial_window_draw_buffer.get().map(|buffer|format!("{buffer:#x}"))})
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.gl.take();
        let _ = self.api.make_current(self.display, None, None, None);
        if let Some(context) = self.context.take()
            && self.api.destroy_context(self.display, context).is_ok()
        {
            self.counts
                .destroyed_contexts
                .set(self.counts.destroyed_contexts.get() + 1);
        }
        if self.api.terminate(self.display).is_ok() {
            self.counts.terminated.set(self.counts.terminated.get() + 1);
        }
    }
}

pub struct Window {
    session: Rc<Session>,
    surface: Option<egl::Surface>,
    window: Option<wayland_egl::WlEglSurface>,
    size: Cell<(u32, u32)>,
    refresh_after_swap: Cell<bool>,
    // The protocol root must remain undestroyed until after these native wrappers.
    _surface: WlSurface,
}

impl Window {
    pub fn on_session(
        session: Rc<Session>,
        surface: &WlSurface,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        validate_size(width, height)?;
        ensure!(
            surface
                .backend()
                .upgrade()
                .is_some_and(|backend| backend == session._connection.backend()),
            "native surface belongs to another Wayland connection"
        );
        let mut window = Self {
            session,
            surface: None,
            window: None,
            size: Cell::new((width, height)),
            refresh_after_swap: Cell::new(false),
            _surface: surface.clone(),
        };
        window.window = Some(wayland_egl::WlEglSurface::new(
            surface.id(),
            width as i32,
            height as i32,
        )?);
        // SAFETY: native window and session share the live Wayland connection; the
        // native window stays alive until after EGLSurface destruction.
        window.surface = Some(unsafe {
            window.session.api.create_window_surface(
                window.session.display,
                window.session.config.context("EGL config missing")?,
                window
                    .window
                    .as_ref()
                    .context("native window missing")?
                    .ptr()
                    .cast_mut(),
                None,
            )
        }?);
        window
            .session
            .counts
            .surfaces
            .set(window.session.counts.surfaces.get() + 1);
        window.activate()?;
        if window.session.initial_window_draw_buffer.get().is_none() {
            let gl = window.gl()?;
            // SAFETY: the context is current on this first native window. Query
            // once, before either painter selects its destination explicitly.
            let initial = unsafe { gl.get_parameter_i32(glow::DRAW_BUFFER0) };
            ensure!(
                unsafe { gl.get_error() } == glow::NO_ERROR,
                "could not query initial draw buffer"
            );
            window.session.initial_window_draw_buffer.set(Some(initial));
        }
        Ok(window)
    }

    pub fn activate(&self) -> Result<()> {
        self.session.api.make_current(
            self.session.display,
            self.surface,
            self.surface,
            self.session.context,
        )?;
        Ok(())
    }

    pub fn draw(&mut self, width: u32, height: u32) -> Result<()> {
        self.resize(width, height)?;
        let gl = self.gl()?;
        // SAFETY: only this thread uses the current context; dimensions are bounded.
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.session.selected.select_default_draw_buffer(gl);
            gl.viewport(0, 0, width as i32, height as i32);
            gl.enable(glow::SCISSOR_TEST);
            let left = width / 2;
            let bottom = height / 2;
            for (x, y, w, h, color) in [
                (0, bottom, left, height - bottom, [220, 20, 30]),
                (left, bottom, width - left, height - bottom, [20, 210, 40]),
                (0, 0, left, bottom, [30, 60, 220]),
                (left, 0, width - left, bottom, [230, 210, 20]),
            ] {
                gl.scissor(x as i32, y as i32, w as i32, h as i32);
                gl.clear_color(
                    color[0] as f32 / 255.,
                    color[1] as f32 / 255.,
                    color[2] as f32 / 255.,
                    1.,
                );
                gl.clear(glow::COLOR_BUFFER_BIT);
            }
            gl.disable(glow::SCISSOR_TEST);
            ensure!(
                gl.get_error() == glow::NO_ERROR,
                "owned Wayland GL draw failed"
            );
        }
        self.present()
    }

    pub fn resize(&self, width: u32, height: u32) -> Result<()> {
        validate_size(width, height)?;
        // Older NVIDIA EGL Wayland implementations defer a non-current window's
        // resize until swap, producing one frame with the previous buffer extent.
        // Bind this window before invoking its native resize callback.
        self.activate()?;
        if self.size.get() != (width, height) {
            self.window
                .as_ref()
                .context("native window missing")?
                .resize(width as i32, height as i32, 0, 0);
            let surface = self.surface.context("EGL surface missing")?;
            let actual = (
                self.session
                    .api
                    .query_surface(self.session.display, surface, egl::WIDTH)?,
                self.session
                    .api
                    .query_surface(self.session.display, surface, egl::HEIGHT)?,
            );
            if actual != (width as i32, height as i32) {
                // Some software paths retain the old drawable until swap. One
                // follow-up frame corrects that geometry without a render timer.
                self.refresh_after_swap.set(true);
            }
            self.size.set((width, height));
            // Mesa refreshes the drawable after the native resize notification;
            // keep the pre-resize binding for older NVIDIA's callback as well.
            self.activate()?;
        }
        Ok(())
    }

    pub fn present(&self) -> Result<()> {
        // The renderer binds this window before painting. Verify that binding
        // still holds before submitting its buffer to the compositor.
        ensure!(
            self.session.api.get_current_context() == self.session.context
                && self.session.api.get_current_surface(egl::DRAW) == self.surface,
            "EGL window must remain current between painting and presentation"
        );
        self.session.api.swap_buffers(
            self.session.display,
            self.surface.context("EGL surface missing")?,
        )?;
        Ok(())
    }

    pub fn take_refresh(&self) -> bool {
        self.refresh_after_swap.replace(false)
    }

    pub fn gl(&self) -> Result<&glow::Context> {
        self.session.gl()
    }
    #[allow(dead_code)] // GL borrowing is needed by the video diagnostic.
    pub fn handles(&self) -> Result<(egl::Display, egl::Context)> {
        self.session.handles()
    }
    pub fn report(&self) -> Result<serde_json::Value> {
        self.session.report()
    }
    pub fn counts(&self) -> Rc<Counts> {
        self.session.counts()
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // Keep the shared context alive without a drawable before deleting this
        // surface. Other windows can bind their own surface on their next draw.
        let _ = self.session.surfaceless();
        if let Some(surface) = self.surface.take()
            && self
                .session
                .api
                .destroy_surface(self.session.display, surface)
                .is_ok()
        {
            self.session
                .counts
                .destroyed_surfaces
                .set(self.session.counts.destroyed_surfaces.get() + 1);
        }
        self.window.take();
    }
}

pub fn validate_size(width: u32, height: u32) -> Result<()> {
    ensure!(
        width > 0
            && height > 0
            && width <= 16_384
            && height <= 16_384
            && u64::from(width) * u64::from(height) * 4 <= 256 * 1024 * 1024,
        "Wayland buffer dimensions exceed probe limits"
    );
    Ok(())
}
