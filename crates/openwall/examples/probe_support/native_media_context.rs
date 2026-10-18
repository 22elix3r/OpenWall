// SPDX-License-Identifier: AGPL-3.0-or-later
//! Borrow one native EGL session for the private shared-video diagnostics.
use super::{egl_window, gl_api};
use anyhow::{Context, Result};
use gstreamer_gl::{self as gst_gl, prelude::*};
use std::rc::Rc;

pub struct Sharing {
    // Field order releases the borrowed context before its display wrapper and
    // retains the native owner until both are gone. Pipelines/leases drop first.
    pub wrapped: gst_gl::GLContext,
    pub display: gstreamer_gl_egl::GLDisplayEGL,
    _session: Rc<egl_window::Session>,
}

impl Sharing {
    pub fn new(session: Rc<egl_window::Session>, selected: gl_api::Api) -> Result<Self> {
        session.surfaceless()?;
        let (display, context) = session.handles()?;
        // SAFETY: foreign wrapper does not terminate EGLDisplay. The retained
        // native owner outlives it and every producer borrowed by the caller.
        let display =
            unsafe { gstreamer_gl_egl::GLDisplayEGL::with_egl_display(display.as_ptr() as usize) }
                .context("could not borrow owned Wayland EGLDisplay")?;
        let api = match selected {
            gl_api::Api::Desktop => gst_gl::GLAPI::OPENGL3,
            gl_api::Api::Gles => gst_gl::GLAPI::GLES2,
        };
        display.filter_gl_api(api);
        // SAFETY: this creating thread has the retained native context current.
        let wrapped = unsafe {
            gst_gl::GLContext::new_wrapped(
                &display,
                context.as_ptr() as usize,
                gst_gl::GLPlatform::EGL,
                api,
            )
        }
        .context("could not wrap current Wayland GL context")?;
        // Install the drop guard before fallible activation/metadata filling.
        let sharing = Self {
            wrapped,
            display,
            _session: session,
        };
        sharing.wrapped.activate(true)?;
        sharing.wrapped.fill_info()?;
        Ok(sharing)
    }
}

impl Drop for Sharing {
    fn drop(&mut self) {
        let _ = self.wrapped.activate(false);
    }
}
