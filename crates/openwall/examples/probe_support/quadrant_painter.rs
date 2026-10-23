// SPDX-License-Identifier: AGPL-3.0-or-later
//! Original diagnostic pattern, separate from native resource ownership.
use anyhow::{Result, ensure};
use glow::HasContext;
use openwall::native_egl::window::Window;

#[allow(dead_code)] // Staged diagnostics paint separately from their final swap.
pub trait Quadrants {
    fn paint(&self, width: u32, height: u32) -> Result<()>;
    fn draw(&self, width: u32, height: u32) -> Result<()>;
}
impl Quadrants for Window {
    fn draw(&self, width: u32, height: u32) -> Result<()> {
        self.paint(width, height)?;
        self.present()
    }

    fn paint(&self, width: u32, height: u32) -> Result<()> {
        self.resize(width, height)?;
        let gl = self.gl()?;
        // SAFETY: only this thread uses the current context; dimensions are bounded.
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.api().select_default_draw_buffer(gl);
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
        Ok(())
    }
}
