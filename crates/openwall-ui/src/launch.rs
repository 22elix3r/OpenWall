// SPDX-License-Identifier: AGPL-3.0-or-later
use std::{ffi::OsString, path::PathBuf};

/// Leave Qt's own arguments alone and accept one explicit local wallpaper path.
pub fn wallpaper(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<Option<PathBuf>, &'static str> {
    let mut arguments = arguments.into_iter();
    let mut wallpaper = None;
    while let Some(argument) = arguments.next() {
        if argument == "--open" {
            if wallpaper.is_some() {
                return Err("Choose one wallpaper with --open.");
            }
            let path = arguments
                .next()
                .filter(|path| !path.is_empty())
                .ok_or("--open requires a local wallpaper path.")?;
            wallpaper = Some(PathBuf::from(path));
        }
    }
    Ok(wallpaper)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(arguments: &[&str]) -> Result<Option<PathBuf>, &'static str> {
        wallpaper(arguments.iter().map(OsString::from))
    }
    #[test]
    fn preserves_local_paths_and_ignores_qt_options() {
        assert_eq!(parse(&["-platform", "offscreen", "--check"]).unwrap(), None);
        assert_eq!(
            parse(&["--open", "wallpapers/海 夜.png", "--check"]).unwrap(),
            Some(PathBuf::from("wallpapers/海 夜.png"))
        );
        assert_eq!(
            parse(&["--open", "-local.png"]).unwrap(),
            Some(PathBuf::from("-local.png"))
        );
        assert!(parse(&["--open"]).is_err());
        assert!(parse(&["--open", ""]).is_err());
        assert!(parse(&["--open", "one.png", "--open", "two.png"]).is_err());
    }
}
