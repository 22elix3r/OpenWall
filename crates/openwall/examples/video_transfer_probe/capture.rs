// SPDX-License-Identifier: AGPL-3.0-or-later
use anyhow::{Context, Result, ensure};
use gstreamer as gst;
use gstreamer_video::{VideoFormat, VideoFrameExt, VideoFrameRef, VideoInfo};
use image::ImageEncoder;
use std::{fs::OpenOptions, path::Path};

const MAX_BYTES: usize = 256 * 1024 * 1024;

pub struct CapturedFrame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    caps: String,
    pts_ns: Option<u64>,
}

impl CapturedFrame {
    pub fn read(buffer: &gst::BufferRef, caps: &gst::CapsRef) -> Result<Self> {
        let info = VideoInfo::from_caps(caps).context("invalid capture video caps")?;
        ensure!(info.format() == VideoFormat::Rgba, "capture requires RGBA");
        let (width, height) = (info.width(), info.height());
        ensure!(
            width > 0 && height > 0 && width <= 16_384 && height <= 16_384,
            "capture dimensions exceed limits"
        );
        ensure!(info.size() <= MAX_BYTES, "capture video exceeds limits");
        // VideoFrame mapping honors GstVideoMeta offsets, strides and allocator maps.
        // Treating a GstBuffer's bytes as tightly packed RGBA would ignore those layouts.
        let frame = VideoFrameRef::from_buffer_ref_readable(buffer, &info)
            .context("could not map RGBA capture")?;
        ensure!(
            frame.format() == VideoFormat::Rgba
                && frame.width() == width
                && frame.height() == height
                && frame.n_planes() == 1,
            "capture layout differs from negotiated RGBA dimensions"
        );
        let stride = usize::try_from(frame.plane_stride()[0])
            .context("negative capture stride is unsupported")?;
        ensure!(
            stride >= width as usize * 4
                && stride
                    .checked_mul(height as usize)
                    .is_some_and(|n| n <= MAX_BYTES),
            "capture stride exceeds limits"
        );
        let data = frame.plane_data(0).context("RGBA plane is missing")?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(width as usize * height as usize * 4)?;
        for row in 0..height as usize {
            rgba.extend_from_slice(
                data.get(row * stride..row * stride + width as usize * 4)
                    .context("capture plane is truncated")?,
            );
        }
        Ok(Self {
            width,
            height,
            rgba,
            caps: caps.to_string(),
            pts_ns: buffer.pts().map(|pts| pts.nseconds()),
        })
    }

    pub fn write_png(&self, path: &Path) -> Result<serde_json::Value> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .context("could not create a new frame capture file")?;
        image::codecs::png::PngEncoder::new(file).write_image(
            &self.rgba,
            self.width,
            self.height,
            image::ExtendedColorType::Rgba8,
        )?;
        Ok(serde_json::json!({
            "width": self.width, "height": self.height, "pts_ns": self.pts_ns,
            "caps": self.caps, "bytes": self.rgba.len(),
            "encoding": "PNG RGBA8; no color-profile transformation",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_uses_video_meta_offsets_and_row_padding() -> Result<()> {
        gst::init()?;
        let first_row = [255, 0, 0, 255, 0, 255, 0, 255];
        let second_row = [0, 0, 255, 255, 255, 255, 255, 255];
        let mut bytes = vec![0xcc; 32];
        bytes[4..12].copy_from_slice(&first_row);
        bytes[16..24].copy_from_slice(&second_row);
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        let info = VideoInfo::builder(VideoFormat::Rgba, 2, 2).build()?;
        gstreamer_video::VideoMeta::add_full(
            buffer.get_mut().unwrap(),
            gstreamer_video::VideoFrameFlags::empty(),
            VideoFormat::Rgba,
            2,
            2,
            &[4],
            &[12],
        )?;
        let captured = CapturedFrame::read(&buffer, info.to_caps()?.as_ref())?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("frame.png");
        captured.write_png(&path)?;
        let png = image::open(&path)?.to_rgba8();
        assert_eq!(png.dimensions(), (2, 2));
        assert_eq!(png.as_raw(), &[first_row, second_row].concat());
        assert!(captured.write_png(&path).is_err());
        assert_eq!(image::open(path)?.to_rgba8(), png);
        Ok(())
    }

    #[test]
    fn capture_rejects_non_rgba_instead_of_reinterpreting_pixels() -> Result<()> {
        gst::init()?;
        let buffer = gst::Buffer::from_mut_slice(vec![0; 16]);
        let info = VideoInfo::builder(VideoFormat::I420, 2, 2).build()?;
        assert!(CapturedFrame::read(&buffer, info.to_caps()?.as_ref()).is_err());
        Ok(())
    }
}
