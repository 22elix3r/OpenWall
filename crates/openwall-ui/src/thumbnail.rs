// SPDX-License-Identifier: AGPL-3.0-or-later
//! Qt receives only an app-generated PNG retained in a private temporary directory.
use crate::query::{Cancellation, run_cancellable};
use std::{io::Read, path::Path, process::Command, time::Duration};

pub struct Thumbnail {
    _directory: tempfile::TempDir,
    pub url: String,
}

pub fn load(player: &Path, input: &Path, cancellation: &Cancellation) -> Option<Thumbnail> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("openwall-thumbnail-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let directory = builder.tempdir().ok()?;
    let output = directory.path().join("preview.png");
    let result = run_cancellable(
        Command::new(player)
            .arg("thumbnail")
            .arg(input)
            .arg("--output")
            .arg(&output),
        Duration::from_secs(5),
        cancellation,
    )
    .ok()?;
    if !result.status.success() || cancellation.is_cancelled() {
        return None;
    }
    let mut file = std::fs::File::open(&output).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > openwall_core::thumbnail::MAX_PNG_BYTES as u64 {
        return None;
    }
    let mut header = [0; 24];
    file.read_exact(&mut header).ok()?;
    if &header[..16] != b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR" {
        return None;
    }
    let width = u32::from_be_bytes(header[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(header[20..24].try_into().ok()?);
    if width == 0
        || height == 0
        || width > openwall_core::thumbnail::MAX_DIMENSION
        || height > openwall_core::thumbnail::MAX_DIMENSION
    {
        return None;
    }
    let url = url::Url::from_file_path(&output).ok()?.to_string();
    Some(Thumbnail {
        _directory: directory,
        url,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn player(directory: &Path) -> std::path::PathBuf {
        let player = directory.join("thumbnail-worker.py");
        std::fs::write(&player, "#!/usr/bin/env python3\nimport pathlib, shutil, sys\nsource=pathlib.Path(sys.argv[2]); target=pathlib.Path(sys.argv[4])\npathlib.Path(str(source)+'.where').write_text(str(target.parent))\nshutil.copyfile(source, target)\n").unwrap();
        std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o700)).unwrap();
        player
    }

    #[test]
    fn generated_preview_is_private_independent_and_removed_when_released() {
        let directory = tempfile::tempdir().unwrap();
        let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/aurora/preview.png");
        let input = directory.path().join("worker-generated.png");
        std::fs::write(&input, openwall_core::thumbnail::png(sample).unwrap()).unwrap();
        let thumbnail = load(&player(directory.path()), &input, &Cancellation::default()).unwrap();
        let output = url::Url::parse(&thumbnail.url)
            .unwrap()
            .to_file_path()
            .unwrap();
        assert_ne!(output, input);
        assert!(output.is_file());
        assert_eq!(
            output
                .parent()
                .unwrap()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
        std::fs::remove_file(input).unwrap();
        assert!(output.is_file(), "thumbnail must survive source removal");
        let private = output.parent().unwrap().to_path_buf();
        drop(thumbnail);
        assert!(!private.exists(), "released preview directory leaked");
    }

    #[test]
    fn rejects_missing_malformed_oversized_and_excessive_dimension_results() {
        let directory = tempfile::tempdir().unwrap();
        let player = player(directory.path());
        let input = directory.path().join("invalid.png");
        assert!(load(&player, &input, &Cancellation::default()).is_none());
        for kind in ["malformed", "dimensions", "bytes"] {
            let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
            bytes.extend_from_slice(&513u32.to_be_bytes());
            bytes.extend_from_slice(&1u32.to_be_bytes());
            if kind == "malformed" {
                bytes = b"not a PNG".to_vec();
            }
            std::fs::write(&input, bytes).unwrap();
            if kind == "bytes" {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&input)
                    .unwrap()
                    .set_len(openwall_core::thumbnail::MAX_PNG_BYTES as u64 + 1)
                    .unwrap();
            }
            assert!(
                load(&player, &input, &Cancellation::default()).is_none(),
                "accepted {kind}"
            );
            let private = std::fs::read_to_string(format!("{}.where", input.display())).unwrap();
            assert!(
                !Path::new(&private).exists(),
                "failed preview directory leaked"
            );
        }
    }
}
