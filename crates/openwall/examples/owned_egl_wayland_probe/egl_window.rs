// SPDX-License-Identifier: AGPL-3.0-or-later
use super::gl_api::Api;
use anyhow::{Context, Result, ensure};
use glow::HasContext;
use khronos_egl as egl;
use std::{cell::Cell, marker::PhantomData, rc::Rc};
use wayland_client::{Connection, Proxy, protocol::wl_surface::WlSurface};

pub struct Window {
    api: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    context: Option<egl::Context>,
    surface: Option<egl::Surface>,
    window: Option<wayland_egl::WlEglSurface>,
    gl: Option<glow::Context>,
    size: Cell<(u32, u32)>,
    selected: Api,
    // The connection and wl_surface outlive the native EGL wrappers. The caller
    // must also keep the protocol object undestroyed until this guard is dropped.
    _connection: Connection,
    _surface: WlSurface,
    _thread: PhantomData<Rc<()>>,
}

impl Window {
    pub fn new(
        connection: &Connection,
        surface: &WlSurface,
        width: u32,
        height: u32,
        selected: Api,
    ) -> Result<Self> {
        validate_size(width, height)?;
        // SAFETY: system EGL ABI; library handle stays alive through native destruction.
        let api = unsafe {
            egl::DynamicInstance::<egl::EGL1_5>::load_required_from_filename("libEGL.so.1")
        }?;
        // EGL_KHR_platform_wayland specifies this enum and a live wl_display pointer.
        // SAFETY: system-backed Connection owns that pointer and remains in this guard.
        let display = unsafe {
            api.get_platform_display(
                0x31d8,
                connection.backend().display_ptr().cast(),
                &[egl::ATTRIB_NONE],
            )
        }?;
        api.initialize(display)?;
        let mut owned = Self {
            api,
            display,
            context: None,
            surface: None,
            window: None,
            gl: None,
            size: Cell::new((width, height)),
            selected,
            _connection: connection.clone(),
            _surface: surface.clone(),
            _thread: PhantomData,
        };
        owned.api.bind_api(selected.egl_api())?;
        let config = owned
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
        owned.context = Some(owned.api.create_context(
            display,
            config,
            None,
            selected.context_attributes(),
        )?);
        owned.window = Some(wayland_egl::WlEglSurface::new(
            surface.id(),
            width as i32,
            height as i32,
        )?);
        // SAFETY: this EGLDisplay and wl_egl_window belong to the same live Wayland
        // connection/platform. The window is retained until after EGLSurface destruction.
        owned.surface = Some(unsafe {
            owned.api.create_window_surface(
                display,
                config,
                owned
                    .window
                    .as_ref()
                    .context("native window missing")?
                    .ptr()
                    .cast_mut(),
                None,
            )
        }?);
        owned
            .api
            .make_current(display, owned.surface, owned.surface, owned.context)?;
        // SAFETY: EGL context is current on this thread and retained by this guard.
        owned.gl = Some(unsafe {
            glow::Context::from_loader_function(|name| {
                owned
                    .api
                    .get_proc_address(name)
                    .map_or(std::ptr::null(), |function| function as *const _)
            })
        });
        Ok(owned)
    }

    pub fn draw(&mut self, width: u32, height: u32) -> Result<()> {
        self.resize(width, height)?;
        let gl = self.gl.as_ref().context("GL context missing")?;
        // SAFETY: only this thread uses the current context; dimensions are bounded.
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
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
        if self.size.get() != (width, height) {
            self.window
                .as_ref()
                .context("native window missing")?
                .resize(width as i32, height as i32, 0, 0);
            self.size.set((width, height));
        }
        Ok(())
    }

    pub fn present(&self) -> Result<()> {
        self.api
            .swap_buffers(self.display, self.surface.context("EGL surface missing")?)?;
        Ok(())
    }

    pub fn gl(&self) -> Result<&glow::Context> {
        self.gl.as_ref().context("GL context missing")
    }

    // Used by the video diagnostic; the static-surface entry point needs no Gst context.
    #[allow(dead_code)]
    pub fn handles(&self) -> Result<(egl::Display, egl::Context)> {
        Ok((self.display, self.context.context("EGL context missing")?))
    }

    pub fn report(&self) -> Result<serde_json::Value> {
        let gl = self.gl()?;
        // SAFETY: context remains current on this thread.
        Ok(unsafe {
            serde_json::json!({"api":self.selected.name(),"vendor":gl.get_parameter_string(glow::VENDOR),
            "renderer":gl.get_parameter_string(glow::RENDERER),"version":gl.get_parameter_string(glow::VERSION)})
        })
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.gl.take();
        let _ = self.api.make_current(self.display, None, None, None);
        if let Some(surface) = self.surface.take() {
            let _ = self.api.destroy_surface(self.display, surface);
        }
        if let Some(context) = self.context.take() {
            let _ = self.api.destroy_context(self.display, context);
        }
        self.window.take();
        let _ = self.api.terminate(self.display);
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
