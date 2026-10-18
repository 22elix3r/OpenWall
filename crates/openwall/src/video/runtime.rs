// SPDX-License-Identifier: AGPL-3.0-or-later
//! Load an app-local upstream hotfix before GStreamer can load the system helper.
use anyhow::{Context, Result, ensure};
use gstreamer::{self as gst, prelude::*};
use libloading::os::unix::{Library, RTLD_GLOBAL, RTLD_NOW};
use std::{fs::File, io::Read, sync::OnceLock};

// GStreamer retains native callbacks and GTypes beyond any individual player.
// Keep this library loaded for the entire process, including failed startup.
static LIBRARY: OnceLock<Library> = OnceLock::new();
static PREPARED: OnceLock<Result<(), String>> = OnceLock::new();

pub fn prepare_wayland_runtime() -> Result<()> {
    PREPARED
        .get_or_init(|| prepare().map_err(|error| format!("{error:#}")))
        .as_ref()
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

pub fn wayland_runtime_report() -> serde_json::Value {
    let result = prepare_wayland_runtime();
    serde_json::json!({
        "available": result.is_ok(),
        "gstreamer": gst::version_string().to_string(),
        "helper": if LIBRARY.get().is_some() { "app-local 1.28.8 with output-change fix" } else { "system" },
        "reason": result.err().map(|error| format!("{error:#}")),
        "scope": "runtime availability; hardware/presentation qualification is separate",
    })
}

fn affected(version: &str) -> bool {
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
    let patch = parts.next().and_then(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .ok()
    });
    major == Some(1) && minor == Some(28) && patch.is_some_and(|patch| patch <= 8)
}

fn prepare() -> Result<()> {
    let (major, minor, _, _) = gst::version();
    if (major, minor) == (1, 28) {
        let executable = std::env::current_exe()?.canonicalize()?;
        let runtime = executable
            .parent()
            .context("worker executable has no parent")?
            .join("openwall-runtime/gstreamer-1.28");
        let path = runtime.join("libgstwayland-1.0.so.0");
        if path.try_exists()? {
            let mut manifest = Vec::new();
            File::open(runtime.join("build.json"))?
                .take(16_385)
                .read_to_end(&mut manifest)?;
            ensure!(
                manifest.len() <= 16_384,
                "Wayland runtime manifest exceeds limits"
            );
            let manifest: serde_json::Value = serde_json::from_slice(&manifest)?;
            ensure!(
                manifest["schema_version"] == 1
                    && manifest["core_abi"] == "1.28"
                    && manifest["upstream_version"] == "1.28.8",
                "app-local Wayland helper has incompatible build metadata"
            );
            // SAFETY: this is trusted native code adjacent to the worker executable,
            // never a wallpaper resource or a path supplied by imported content.
            // It implements the GStreamer 1.28 helper ABI and must precede plugin
            // loading. RTLD_GLOBAL lets the system waylandsink resolve that ABI;
            // LIBRARY retains every callback/GType until the process exits.
            let library = unsafe { Library::open(Some(&path), RTLD_NOW | RTLD_GLOBAL) }
                .context("could not load the app-local Wayland helper")?;
            ensure!(
                LIBRARY.set(library).is_ok(),
                "Wayland helper was already loaded"
            );
            eprintln!("OpenWall Wayland runtime: app-local GStreamer 1.28.8 output-change fix");
        }
    }
    gst::init()?;
    let factory =
        gst::ElementFactory::find("waylandsink").context("GStreamer waylandsink is missing")?;
    let plugin = factory
        .plugin()
        .context("waylandsink has no plugin metadata")?;
    ensure!(
        !affected(plugin.version().as_str()) || LIBRARY.get().is_some(),
        "native Wayland video unavailable: waylandsink {} has an upstream output-change crash; build the app-local helper with python3 scripts/build_wayland_runtime.py --binary /path/to/openwall (see integrations/gstreamer/README.md)",
        plugin.version()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_known_affected_plugin_release_range_is_guarded() {
        assert!(affected("1.28.0"));
        assert!(affected("1.28.2"));
        assert!(affected("1.28.8-custom"));
        assert!(!affected("1.24.2"));
        assert!(!affected("1.26.11"));
        assert!(!affected("1.28.9"));
        assert!(!affected("1.30.0"));
    }
}
