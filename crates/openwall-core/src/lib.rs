// SPDX-License-Identifier: AGPL-3.0-or-later
//! Data, bounded asset access, and the explicitly supported scene subset.
//!
//! This crate has no desktop, GUI, or GPU dependencies.

pub mod assets;
pub mod inspection;
pub mod media;
pub mod scene;
pub mod thumbnail;

pub use assets::Assets;
pub use scene::{CompatibilityReport, Layer, Scene};
