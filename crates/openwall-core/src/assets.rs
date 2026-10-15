// SPDX-License-Identifier: AGPL-3.0-or-later
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
use kirie_formats::pkg::Pkg;

pub const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 16_384;

/// Package data is immutable and never extracted onto the host filesystem.
#[derive(Clone, Debug)]
pub struct Assets {
    root: PathBuf,
    packed: BTreeMap<String, Arc<[u8]>>,
}

impl Assets {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .context("cannot open wallpaper directory")?;
        ensure!(root.is_dir(), "wallpaper root must be a directory");
        let mut assets = Self {
            root,
            packed: BTreeMap::new(),
        };
        let package = assets.root.join("scene.pkg");
        if package.exists() {
            let bytes = assets.read_file("scene.pkg", MAX_PACKAGE_BYTES)?;
            assets.load_package(&bytes)?;
        }
        Ok(assets)
    }

    fn load_package(&mut self, bytes: &[u8]) -> Result<()> {
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
        let package = Pkg::parse(bytes).context("invalid Wallpaper Engine package")?;
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
            self.packed
                .insert(name.to_owned(), Arc::from(package.read(entry)?));
        }
        Ok(())
    }

    pub fn read(&self, name: &str) -> Result<Arc<[u8]>> {
        validate_name(name)?;
        if let Some(data) = self.packed.get(name) {
            return Ok(data.clone());
        }
        self.read_file(name, MAX_ASSET_BYTES).map(Arc::from)
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
    if name.is_empty() || name.contains(['\\', '\0', ':']) || name.starts_with('/') {
        bail!("invalid asset path: {name:?}");
    }
    ensure!(name.len() <= 1024, "asset path too long");
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
}
