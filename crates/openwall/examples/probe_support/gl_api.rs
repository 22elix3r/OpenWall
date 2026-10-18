// SPDX-License-Identifier: AGPL-3.0-or-later
use glow::HasContext;
use khronos_egl as egl;

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum Api {
    #[default]
    Gles,
    Desktop,
}

impl Api {
    pub fn select_default_draw_buffer(self, gl: &glow::Context) {
        // SAFETY: diagnostics call this with the owned context current and its
        // default window framebuffer bound. Surfaceless initialization can leave
        // GL_NONE selected; window swaps need writes to the back buffer.
        unsafe {
            match self {
                Self::Desktop => gl.draw_buffer(glow::BACK),
                Self::Gles => gl.draw_buffers(&[glow::BACK]),
            }
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Gles => "gles",
            Self::Desktop => "desktop",
        }
    }

    pub fn egl_api(self) -> egl::Enum {
        match self {
            Self::Gles => egl::OPENGL_ES_API,
            Self::Desktop => egl::OPENGL_API,
        }
    }

    pub fn renderable(self) -> egl::Int {
        match self {
            Self::Gles => egl::OPENGL_ES3_BIT,
            Self::Desktop => egl::OPENGL_BIT,
        }
    }

    pub fn context_attributes(self) -> &'static [egl::Int] {
        match self {
            Self::Gles => &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
            Self::Desktop => &[
                egl::CONTEXT_MAJOR_VERSION,
                3,
                egl::CONTEXT_MINOR_VERSION,
                3,
                egl::CONTEXT_OPENGL_PROFILE_MASK,
                egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
                egl::NONE,
            ],
        }
    }
}
