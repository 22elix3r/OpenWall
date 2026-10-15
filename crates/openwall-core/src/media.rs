// SPDX-License-Identifier: AGPL-3.0-or-later
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

pub fn video_path(path: &Path) -> Result<Option<PathBuf>> {
    let path = path
        .canonicalize()
        .context("wallpaper path does not exist")?;
    let extension = path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "mp4" | "m4v" | "webm" | "mkv" | "mov" | "avi"
    ) {
        ensure!(path.is_file(), "video must be a regular file");
        return Ok(Some(path));
    }
    if path.is_dir() || extension == "json" {
        let root = if path.is_dir() {
            path.clone()
        } else {
            path.parent()
                .context("missing parent directory")?
                .to_path_buf()
        };
        let name = if path.is_dir() {
            "project.json"
        } else {
            path.file_name()
                .and_then(|x| x.to_str())
                .context("invalid project filename")?
        };
        let assets = crate::Assets::open(&root)?;
        let project = assets.json(name)?;
        if project["type"].as_str() == Some("video") {
            let name = project["file"]
                .as_str()
                .context("video project is missing its file")?;
            crate::assets::validate_name(name)?;
            let video = root
                .join(name)
                .canonicalize()
                .context("video file is missing")?;
            ensure!(
                video.starts_with(root) && video.is_file(),
                "video must remain inside its project directory"
            );
            return Ok(Some(video));
        }
    }
    Ok(None)
}
