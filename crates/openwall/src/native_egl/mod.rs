// SPDX-License-Identifier: AGPL-3.0-or-later
//! Application-owned Wayland EGL resources, independent of content/media decoding.
//! One session owns each connection's display/context; windows retain that owner.
//! Callers retain protocol roles/surfaces until all native windows are destroyed.
pub mod gl_api;
mod ownership;
pub mod window;
