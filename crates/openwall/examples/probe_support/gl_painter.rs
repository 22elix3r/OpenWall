// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fixed RGBA texture painter used by the off-screen and native diagnostics.
use super::{gl_api::Api, shared_gl::Source};
use anyhow::{Context, Result, bail, ensure};
use glow::HasContext;

pub struct Painter<'a> {
    gl: &'a glow::Context,
    sampler: Option<glow::NativeSampler>,
    program: Option<glow::NativeProgram>,
    vertex_array: Option<glow::NativeVertexArray>,
}

impl<'a> Painter<'a> {
    pub fn new(gl: &'a glow::Context, selected: Api) -> Result<Self> {
        let mut painter = Self {
            gl,
            sampler: None,
            program: None,
            vertex_array: None,
        };
        // SAFETY: the caller retains the current context for this guard's lifetime.
        // Only fixed repository shaders are compiled; no wallpaper code is executed.
        unsafe {
            let sampler = gl.create_sampler().map_err(anyhow::Error::msg)?;
            painter.sampler = Some(sampler);
            for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                gl.sampler_parameter_i32(sampler, parameter, glow::LINEAR as i32);
            }
            for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                gl.sampler_parameter_i32(sampler, parameter, glow::CLAMP_TO_EDGE as i32);
            }
            let program = gl.create_program().map_err(anyhow::Error::msg)?;
            painter.program = Some(program);
            let header = match selected {
                Api::Gles => "#version 300 es\nprecision highp float;",
                Api::Desktop => "#version 330 core",
            };
            let vertex = shader(
                gl,
                glow::VERTEX_SHADER,
                &r#"#version 300 es
precision highp float;
uniform vec2 source_scale;
out vec2 uv;
void main() {
    vec2 p = vec2[](vec2(-1., -1.), vec2(3., -1.), vec2(-1., 3.))[gl_VertexID];
    gl_Position = vec4(p, 0., 1.);
    uv = vec2((p.x + 1.) * .5, (1. - p.y) * .5) * source_scale;
}"#
                .replacen("#version 300 es\nprecision highp float;", header, 1),
            )?;
            let fragment = match shader(
                gl,
                glow::FRAGMENT_SHADER,
                &r#"#version 300 es
precision highp float;
uniform sampler2D source_texture;
in vec2 uv;
out vec4 color;
void main() { color = texture(source_texture, uv); }
"#
                .replacen("#version 300 es\nprecision highp float;", header, 1),
            ) {
                Ok(value) => value,
                Err(error) => {
                    gl.delete_shader(vertex);
                    return Err(error);
                }
            };
            gl.attach_shader(program, vertex);
            gl.attach_shader(program, fragment);
            gl.link_program(program);
            gl.detach_shader(program, vertex);
            gl.detach_shader(program, fragment);
            gl.delete_shader(vertex);
            gl.delete_shader(fragment);
            ensure!(
                gl.get_program_link_status(program),
                "owned shader link failed: {}",
                gl.get_program_info_log(program)
            );
            let vao = gl.create_vertex_array().map_err(anyhow::Error::msg)?;
            painter.vertex_array = Some(vao);
            gl.bind_vertex_array(Some(vao));
            gl.use_program(Some(program));
            gl.uniform_1_i32(
                gl.get_uniform_location(program, "source_texture").as_ref(),
                0,
            );
            ensure!(gl.get_error() == glow::NO_ERROR, "owned GL setup failed");
            Ok(painter)
        }
    }

    pub fn draw(
        &self,
        source: &Source,
        framebuffer: Option<glow::NativeFramebuffer>,
        viewport: [u32; 4],
    ) -> Result<()> {
        let gl = self.gl;
        // SAFETY: context is current; source texture is validated and retained by
        // a lease. The caller bounds the destination dimensions before this call.
        unsafe {
            let program = self.program.context("owned program missing")?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, framebuffer);
            gl.bind_vertex_array(self.vertex_array);
            gl.use_program(Some(program));
            gl.uniform_2_f32(
                gl.get_uniform_location(program, "source_scale").as_ref(),
                source.width as f32 / source.texture_size.0 as f32,
                source.height as f32 / source.texture_size.1 as f32,
            );
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(source.texture));
            gl.bind_sampler(0, self.sampler);
            gl.viewport(
                viewport[0] as i32,
                viewport[1] as i32,
                viewport[2] as i32,
                viewport[3] as i32,
            );
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            ensure!(gl.get_error() == glow::NO_ERROR, "owned GL draw failed");
        }
        Ok(())
    }
}

impl Drop for Painter<'_> {
    fn drop(&mut self) {
        // SAFETY: the current context outlives this guard. GL defers deletion of
        // application objects referenced by commands already submitted.
        unsafe {
            if let Some(value) = self.vertex_array.take() {
                self.gl.delete_vertex_array(value);
            }
            if let Some(value) = self.program.take() {
                self.gl.delete_program(value);
            }
            if let Some(value) = self.sampler.take() {
                self.gl.delete_sampler(value);
            }
        }
    }
}

fn shader(gl: &glow::Context, kind: u32, source: &str) -> Result<glow::NativeShader> {
    // SAFETY: the native context is current; shader strings are fixed repository code.
    unsafe {
        let shader = gl.create_shader(kind).map_err(anyhow::Error::msg)?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            let error = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            bail!("owned shader compile failed: {error}");
        }
        Ok(shader)
    }
}
