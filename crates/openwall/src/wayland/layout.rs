// SPDX-License-Identifier: AGPL-3.0-or-later
//! Keep surface-local coordinates separate from bounded physical buffer sizes.
use anyhow::{Result, ensure};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Layout {
    pub logical: (u32, u32),
    pub buffer: (u32, u32),
    pub buffer_scale: i32,
    pub viewport: bool,
}

impl Layout {
    pub fn new(
        logical: (u32, u32),
        integer_scale: i32,
        fractional_scale: Option<u32>,
        viewport: bool,
    ) -> Result<Self> {
        ensure!(logical.0 > 0 && logical.1 > 0, "empty surface dimensions");
        ensure!(
            logical.0 <= 16384 && logical.1 <= 16384,
            "output exceeds logical dimension limits"
        );
        let buffer_scale = if viewport { 1 } else { integer_scale.max(1) };
        let scale = if viewport {
            fractional_scale
                .map(u64::from)
                .unwrap_or(integer_scale.max(1) as u64 * 120)
        } else {
            buffer_scale as u64 * 120
        };
        ensure!(scale > 0, "compositor supplied a zero fractional scale");
        // The protocol specifies nearest rounding with halfway values away from zero.
        let width = ((u64::from(logical.0) * scale + 60) / 120).max(1);
        let height = ((u64::from(logical.1) * scale + 60) / 120).max(1);
        ensure!(
            width <= 16384 && height <= 16384,
            "output exceeds presentation dimension limits"
        );
        ensure!(
            width * height * 4 <= 256 * 1024 * 1024,
            "output exceeds presentation buffer budget"
        );
        Ok(Self {
            logical,
            buffer: (width as u32, height as u32),
            buffer_scale,
            viewport,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_scale_uses_physical_pixels_and_logical_viewport() {
        let layout = Layout::new((1280, 720), 2, Some(150), true).unwrap();
        assert_eq!(layout.buffer, (1600, 900));
        assert_eq!(layout.logical, (1280, 720));
        assert_eq!(layout.buffer_scale, 1);
        assert!(layout.viewport);
        assert_eq!(
            Layout::new((101, 51), 2, Some(180), true).unwrap().buffer,
            (152, 77)
        );
    }

    #[test]
    fn fallback_does_not_apply_fractional_scale_without_a_viewport() {
        let layout = Layout::new((640, 360), 2, Some(150), false).unwrap();
        assert_eq!(layout.buffer, (1280, 720));
        assert_eq!(layout.buffer_scale, 2);
        assert!(!layout.viewport);
        assert_eq!(
            Layout::new((640, 360), 2, None, true).unwrap().buffer,
            (1280, 720)
        );
    }

    #[test]
    fn compositor_sizes_are_bounded_before_allocation_or_protocol_requests() {
        for (size, scale) in [
            ((0, 720), 120),
            ((640, 0), 120),
            ((640, 360), 0),
            ((u32::MAX, u32::MAX), u32::MAX),
            ((16384, 16384), 120),
            ((16384, 1), 240),
        ] {
            assert!(Layout::new(size, 1, Some(scale), true).is_err());
        }
        assert_eq!(
            Layout::new((1, 1), 1, Some(1), true).unwrap().buffer,
            (1, 1)
        );
        assert!(Layout::new((16384, 1), i32::MAX, None, false).is_err());
    }
}
