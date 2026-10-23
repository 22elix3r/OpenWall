// SPDX-License-Identifier: AGPL-3.0-or-later
//! Platform-independent bookkeeping for native presentation transitions.
pub mod feedback;
#[cfg(target_os = "linux")]
pub mod wayland_tree;
