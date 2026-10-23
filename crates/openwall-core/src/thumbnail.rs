// SPDX-License-Identifier: AGPL-3.0-or-later
//! Static, bounded thumbnails; no GPU, video decode or wallpaper code execution.
use crate::Assets;
use anyhow::{Context, Result, ensure};
use image::ImageEncoder;
use std::path::Path;

pub const MAX_DIMENSION: u32 = 512;
pub const MAX_PNG_BYTES: usize = 2 * 1024 * 1024;

pub fn png(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = path
        .as_ref()
        .canonicalize()
        .context("thumbnail input is missing")?;
    let (root, name) = if path.is_dir() {
        (path.as_path(), "preview.png")
    } else {
        let root = path.parent().context("thumbnail input has no directory")?;
        let image = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "webp" | "tex"
                )
            });
        (
            root,
            if image {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .context("invalid image filename")?
            } else {
                "preview.png"
            },
        )
    };
    // Deliberately do not discover/load neighboring packages for a thumbnail.
    let pixels = Assets::loose(root)?.image(name)?;
    let longest = pixels.width().max(pixels.height()).max(MAX_DIMENSION);
    let width =
        (u64::from(pixels.width()) * u64::from(MAX_DIMENSION) / u64::from(longest)).max(1) as u32;
    let height =
        (u64::from(pixels.height()) * u64::from(MAX_DIMENSION) / u64::from(longest)).max(1) as u32;
    let thumbnail = image::imageops::thumbnail(&pixels, width, height);
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes).write_image(
        thumbnail.as_raw(),
        thumbnail.width(),
        thumbnail.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    ensure!(
        bytes.len() <= MAX_PNG_BYTES,
        "thumbnail PNG exceeds size limit"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_a_static_png_with_bounded_geometry_and_preserved_alpha() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("original.png");
        image::RgbaImage::from_pixel(1024, 256, image::Rgba([32, 64, 96, 128]))
            .save(&path)
            .unwrap();
        let bytes = png(&path).unwrap();
        assert!(bytes.len() <= MAX_PNG_BYTES);
        let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (512, 128));
        assert_eq!(decoded.get_pixel(0, 0).0, [32, 64, 96, 128]);
        image::RgbaImage::from_pixel(8, 16, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        assert_eq!(
            image::load_from_memory(&png(&path).unwrap())
                .unwrap()
                .width(),
            8
        );
    }

    #[test]
    fn directory_and_project_use_only_a_bounded_loose_preview() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project.json");
        std::fs::write(&project, b"not read by thumbnail generation").unwrap();
        std::fs::write(directory.path().join("scene.pkg"), b"not loaded either").unwrap();
        assert!(png(&project).is_err());
        image::RgbaImage::from_pixel(16, 8, image::Rgba([1, 2, 3, 255]))
            .save(directory.path().join("preview.png"))
            .unwrap();
        assert_eq!(png(&project).unwrap(), png(directory.path()).unwrap());
        std::fs::write(directory.path().join("preview.png"), b"malformed").unwrap();
        assert!(png(&project).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_cannot_escape_the_wallpaper_directory() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let image = outside.path().join("outside.png");
        image::RgbaImage::new(1, 1).save(&image).unwrap();
        std::os::unix::fs::symlink(&image, directory.path().join("preview.png")).unwrap();
        let error = png(directory.path()).unwrap_err().to_string();
        assert!(error.contains("escapes wallpaper root"), "{error}");
    }

    #[test]
    fn rgba_expansion_is_bounded_before_pixel_decode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gray.png");
        // A legal grayscale decode would fit the byte budget, but RGBA would not.
        image::GrayImage::new(4097, 4097).save(&path).unwrap();
        let error = png(&path).unwrap_err().to_string();
        assert!(error.contains("RGBA memory limit"), "{error}");
    }
}
