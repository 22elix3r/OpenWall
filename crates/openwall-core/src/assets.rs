// SPDX-License-Identifier: AGPL-3.0-or-later
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    ops::{Deref, Range},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
use kirie_formats::pkg::Pkg;

pub const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 16_384;

/// Immutable bytes owned by a loose file or a shared package snapshot. A packed
/// view keeps that package alive, without copying an entry or exposing mutation.
#[derive(Clone)]
pub struct AssetBytes {
    data: Arc<Vec<u8>>,
    range: Range<usize>,
}

impl AssetBytes {
    fn file(bytes: Vec<u8>) -> Self {
        let end = bytes.len();
        Self {
            data: Arc::new(bytes),
            range: 0..end,
        }
    }
}
impl Deref for AssetBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data[self.range.clone()]
    }
}
impl AsRef<[u8]> for AssetBytes {
    fn as_ref(&self) -> &[u8] {
        self
    }
}
impl std::fmt::Debug for AssetBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssetBytes")
            .field("length", &self.len())
            .finish_non_exhaustive()
    }
}

/// Package data is immutable and never extracted onto the host filesystem.
#[derive(Clone, Debug)]
pub struct Assets {
    root: PathBuf,
    packed: BTreeMap<String, AssetBytes>,
}

impl Assets {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let mut assets = Self::loose(root)?;
        let package = assets.root.join("scene.pkg");
        if package.exists() {
            let bytes = assets.read_file("scene.pkg", MAX_PACKAGE_BYTES)?;
            assets.load_package(bytes)?;
        }
        Ok(assets)
    }

    /// Standalone image access must not discover unrelated neighboring packages.
    pub fn loose(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .context("cannot open wallpaper directory")?;
        ensure!(root.is_dir(), "wallpaper root must be a directory");
        Ok(Self {
            root,
            packed: BTreeMap::new(),
        })
    }

    fn load_package(&mut self, bytes: Vec<u8>) -> Result<()> {
        // Check the entry count before handing untrusted lengths to the upstream parser.
        ensure!(bytes.len() >= 8, "truncated package header");
        let magic_len = u32::from_le_bytes(bytes[..4].try_into()?) as usize;
        ensure!(magic_len <= 64, "package magic is too long");
        let count_at = 4usize
            .checked_add(magic_len)
            .context("package header overflow")?;
        let raw_count = bytes
            .get(count_at..count_at + 4)
            .context("truncated package entry count")?;
        let count = u32::from_le_bytes(raw_count.try_into()?) as usize;
        ensure!(count <= MAX_ENTRIES, "package has too many entries");
        // Move the buffer into a shared owner; do not copy it into an Arc slice.
        // Entry offsets may overlap, so a package-size limit cannot bound the sum
        // of independently copied entries.
        let bytes = Arc::new(bytes);
        let package = Pkg::parse(&bytes).context("invalid Wallpaper Engine package")?;
        for entry in package.entries() {
            let name = entry
                .name_str()
                .context("package contains a non-UTF-8 asset name")?;
            validate_name(name)?;
            ensure!(
                u64::from(entry.len) <= MAX_ASSET_BYTES,
                "package asset exceeds size limit: {name}"
            );
            ensure!(
                !self.packed.contains_key(name),
                "duplicate package asset: {name}"
            );
            let payload = package.read(entry)?;
            let start = package
                .base_offset()
                .checked_add(entry.offset as usize)
                .context("package asset offset overflow")?;
            let end = start
                .checked_add(payload.len())
                .context("package asset length overflow")?;
            ensure!(
                bytes.get(start..end).is_some(),
                "invalid package asset range"
            );
            self.packed.insert(
                name.to_owned(),
                AssetBytes {
                    data: bytes.clone(),
                    range: start..end,
                },
            );
        }
        Ok(())
    }

    pub fn read(&self, name: &str) -> Result<AssetBytes> {
        validate_name(name)?;
        if let Some(data) = self.packed.get(name) {
            return Ok(data.clone());
        }
        self.read_file(name, MAX_ASSET_BYTES).map(AssetBytes::file)
    }

    fn read_file(&self, name: &str, limit: u64) -> Result<Vec<u8>> {
        validate_name(name)?;
        let path = self
            .root
            .join(name)
            .canonicalize()
            .with_context(|| format!("missing asset: {name}"))?;
        ensure!(
            path.starts_with(&self.root),
            "asset escapes wallpaper root: {name}"
        );
        ensure!(
            path.metadata()?.is_file(),
            "asset must be a regular file: {name}"
        );
        let file = File::open(&path).with_context(|| format!("cannot read asset: {name}"))?;
        ensure!(
            file.metadata()?.is_file(),
            "asset must be a regular file: {name}"
        );
        ensure!(
            file.metadata()?.len() <= limit,
            "asset exceeds size limit: {name}"
        );
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= limit,
            "asset grew beyond size limit: {name}"
        );
        Ok(bytes)
    }

    pub fn json(&self, name: &str) -> Result<serde_json::Value> {
        let bytes = self.read(name)?;
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "JSON asset exceeds 4 MiB: {name}"
        );
        serde_json::from_slice(&bytes).with_context(|| format!("invalid JSON in {name}"))
    }

    pub fn image(&self, name: &str) -> Result<image::RgbaImage> {
        let bytes = self.read(name)?;
        if name.ends_with(".tex") {
            ensure!(bytes.len() >= 63, "truncated TEX header");
            let images = u32::from_le_bytes(bytes[55..59].try_into()?);
            ensure!(images == 1, "only single-image static TEX is supported");
            let flags =
                kirie_formats::tex::TextureFlags(u32::from_le_bytes(bytes[22..26].try_into()?));
            ensure!(
                !flags.is_gif() && !flags.video(),
                "animated and video TEX are not supported yet"
            );
            let mip_offset = match &bytes[46..55] {
                b"TEXB0001\0" | b"TEXB0002\0" => 59,
                b"TEXB0003\0" => 63,
                b"TEXB0004\0" => 67,
                _ => bail!("unsupported TEX image container"),
            };
            let mip_count = bytes
                .get(mip_offset..mip_offset + 4)
                .context("truncated TEX mip count")?;
            let mip_count = u32::from_le_bytes(mip_count.try_into()?);
            ensure!(
                (1..=16).contains(&mip_count),
                "TEX mip count exceeds limits"
            );
            let tex = kirie_formats::tex::Tex::parse(&bytes).context("invalid TEX texture")?;
            ensure!(
                tex.width <= 8192 && tex.height <= 8192,
                "TEX exceeds dimension limit"
            );
            ensure!(
                u64::from(tex.width) * u64::from(tex.height) * 4 <= MAX_ASSET_BYTES,
                "decoded TEX exceeds memory limit"
            );
            ensure!(
                tex.fif.is_raw() && !tex.flags.is_gif() && !tex.flags.video(),
                "embedded media and animated TEX are not supported yet"
            );
            for image in &tex.images {
                for mip in &image.mipmaps {
                    ensure!(
                        mip.width <= 8192
                            && mip.height <= 8192
                            && u64::from(mip.width) * u64::from(mip.height) * 4 <= MAX_ASSET_BYTES
                            && mip.uncompressed_size <= MAX_ASSET_BYTES as usize,
                        "TEX mip exceeds memory limit"
                    );
                }
            }
            let decoded = tex.decode_rgba8(0, 0).context("unsupported TEX texture")?;
            return image::RgbaImage::from_raw(decoded.width, decoded.height, decoded.pixels)
                .context("invalid decoded TEX dimensions");
        }
        let mut reader =
            image::ImageReader::new(std::io::Cursor::new(&*bytes)).with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(MAX_ASSET_BYTES);
        reader.limits(limits.clone());
        let (width, height) = reader.into_dimensions()?;
        ensure!(
            u64::from(width) * u64::from(height) * 4 <= MAX_ASSET_BYTES,
            "decoded image exceeds RGBA memory limit: {name}"
        );
        let mut reader =
            image::ImageReader::new(std::io::Cursor::new(&*bytes)).with_guessed_format()?;
        reader.limits(limits);
        reader
            .decode()
            .with_context(|| format!("cannot decode image: {name}"))
            .map(|x| x.to_rgba8())
    }

    pub fn packed_entries(&self) -> usize {
        self.packed.len()
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(name.len() <= 1024, "asset path too long");
    if name.is_empty() || name.contains(['\\', '\0', ':']) || name.starts_with('/') {
        bail!("invalid asset path: {name:?}");
    }
    ensure!(
        name.split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "invalid asset path: {name:?}"
    );
    ensure!(
        Path::new(name)
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "invalid asset path: {name:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_host_paths_and_traversal() {
        for name in [
            "",
            "../secret",
            "/etc/passwd",
            "a/../../secret",
            "C:/secret",
            "a\\b",
            "a//b",
            "a/./b",
            "a\0b",
        ] {
            assert!(validate_name(name).is_err(), "accepted {name:?}");
        }
        assert!(validate_name("materials/海.json").is_ok());
    }

    #[test]
    fn rejects_package_allocation_bombs() {
        let root = tempfile::tempdir().unwrap();
        let mut bytes = Vec::from(8u32.to_le_bytes());
        bytes.extend_from_slice(b"PKGV0001");
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(root.path().join("scene.pkg"), bytes).unwrap();
        assert!(
            Assets::open(root.path())
                .unwrap_err()
                .to_string()
                .contains("too many entries")
        );
    }

    #[test]
    fn original_tex_fixture_decodes_known_colors() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples");
        let texture = Assets::open(root).unwrap().image("checker.tex").unwrap();
        assert_eq!(texture.dimensions(), (2, 2));
        assert_eq!(texture.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(texture.get_pixel(1, 1).0, [255, 255, 255, 255]);
    }

    #[test]
    fn tex_image_count_is_bounded_before_upstream_allocation() {
        let root = tempfile::tempdir().unwrap();
        let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/checker.tex");
        let mut bytes = std::fs::read(sample).unwrap();
        bytes[55..59].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(root.path().join("bomb.tex"), bytes).unwrap();
        assert!(
            Assets::open(root.path())
                .unwrap()
                .image("bomb.tex")
                .unwrap_err()
                .to_string()
                .contains("single-image")
        );
    }

    #[test]
    fn package_cannot_contain_host_paths() {
        let root = tempfile::tempdir().unwrap();
        let name = b"../outside";
        let mut bytes = Vec::from(8u32.to_le_bytes());
        bytes.extend_from_slice(b"PKGV0001");
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(0);
        std::fs::write(root.path().join("scene.pkg"), bytes).unwrap();
        assert!(
            Assets::open(root.path())
                .unwrap_err()
                .to_string()
                .contains("invalid asset path")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_root() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.path().join("escape")).unwrap();
        assert!(Assets::open(root.path()).unwrap().read("escape").is_err());
    }

    fn package(entries: &[(&str, u32, u32)], payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::from(8u32.to_le_bytes());
        bytes.extend_from_slice(b"PKGV0001");
        bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for &(name, offset, length) in entries {
            bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(&offset.to_le_bytes());
            bytes.extend_from_slice(&length.to_le_bytes());
        }
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn overlapping_entries_share_one_immutable_package_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("scene.pkg");
        let names = (0..512)
            .map(|index| format!("alias-{index}"))
            .collect::<Vec<_>>();
        let entries = names
            .iter()
            .map(|name| (name.as_str(), 0, 64 * 1024))
            .collect::<Vec<_>>();
        let bytes = package(&entries, &vec![0x5a; 64 * 1024]);
        let size = bytes.len();
        std::fs::write(&path, bytes).unwrap();
        let assets = Assets::open(root.path()).unwrap();
        let clone = assets.clone();
        let first = assets.read(&names[0]).unwrap();
        for name in &names {
            let view = clone.read(name).unwrap();
            assert!(
                Arc::ptr_eq(&first.data, &view.data),
                "package payload copied for {name}"
            );
            assert_eq!(first.as_ptr(), view.as_ptr());
            assert_eq!(view.len(), 64 * 1024);
        }
        assert_eq!(
            first.data.len(),
            size,
            "backing storage grew with alias count"
        );
        std::fs::write(&path, b"replaced on disk").unwrap();
        drop(assets);
        drop(clone);
        assert_eq!(first.as_ref(), &[0x5a; 64 * 1024]);
        assert!(
            format!("{first:?}").len() < 100,
            "debug output dumped package bytes"
        );
    }

    #[test]
    fn partial_overlap_and_out_of_bounds_ranges_are_checked() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("scene.pkg");
        std::fs::write(&path, package(&[("a", 0, 4), ("b", 2, 4)], b"abcdef")).unwrap();
        let assets = Assets::open(root.path()).unwrap();
        let a = assets.read("a").unwrap();
        let b = assets.read("b").unwrap();
        assert_eq!(a.as_ref(), b"abcd");
        assert_eq!(b.as_ref(), b"cdef");
        assert_eq!(a.as_ptr().wrapping_add(2), b.as_ptr());
        for entries in [
            vec![("a", u32::MAX, 1)],
            vec![("a", 0, u32::MAX)],
            vec![("a", 5, 2)],
            vec![("same", 0, 1), ("same", 1, 1)],
        ] {
            std::fs::write(&path, package(&entries, b"abcdef")).unwrap();
            assert!(Assets::open(root.path()).is_err(), "accepted {entries:?}");
        }
    }

    #[test]
    fn oversized_path_errors_are_bounded_before_formatting() {
        let name = format!("/{}", "x".repeat(64 * 1024));
        let error = validate_name(&name).unwrap_err().to_string();
        assert_eq!(error, "asset path too long");
    }

    #[test]
    fn unsupported_animated_tex_is_rejected_before_animation_parsing() {
        let root = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/checker.tex");
        for flag in [
            kirie_formats::tex::TextureFlags::IS_GIF,
            kirie_formats::tex::TextureFlags::VIDEO,
        ] {
            let mut bytes = std::fs::read(&fixture).unwrap();
            bytes[22..26].copy_from_slice(&flag.to_le_bytes());
            std::fs::write(root.path().join("unsupported.tex"), bytes).unwrap();
            let error = Assets::loose(root.path())
                .unwrap()
                .image("unsupported.tex")
                .unwrap_err()
                .to_string();
            assert!(error.contains("animated and video TEX"), "{error}");
        }
    }
}
