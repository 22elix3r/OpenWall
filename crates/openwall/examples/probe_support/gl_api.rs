// SPDX-License-Identifier: AGPL-3.0-or-later
use khronos_egl as egl;

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum Api {
    #[default]
    Gles,
    Desktop,
}

impl Api {
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
