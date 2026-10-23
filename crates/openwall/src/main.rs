// SPDX-License-Identifier: AGPL-3.0-or-later
mod gpu;
#[cfg(target_os = "macos")]
mod macos_display;
#[cfg(target_os = "macos")]
mod macos_video;
mod preview;
#[cfg(all(target_os = "linux", feature = "video"))]
mod video;
#[cfg(target_os = "linux")]
mod wayland;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use openwall_core::Scene;
use std::{path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(version, about = "OpenWall — open-source desktop wallpapers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the desktop capabilities actually available in this session.
    Doctor,
    /// List connected displays and identifiers accepted by --output.
    Outputs,
    /// Inspect the supported scene subset without executing wallpaper code.
    Inspect { path: PathBuf },
    /// Render a scene to PNG without creating a desktop window.
    Render {
        path: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        #[arg(long)]
        allow_partial: bool,
    },
    /// Open a wallpaper in a separate preview window.
    Preview {
        path: Option<PathBuf>,
        #[arg(long)]
        seconds: Option<u64>,
        #[arg(long)]
        allow_partial: bool,
    },
    /// Present behind desktop applications. Ctrl+C removes only OpenWall's surfaces.
    Apply {
        path: Option<PathBuf>,
        #[arg(long)]
        output: Option<String>,
        #[arg(long)]
        seconds: Option<u64>,
    },
}

fn main() {
    if let Err(error) = execute() {
        eprintln!("OpenWall: {error:#}");
        std::process::exit(1);
    }
}

fn execute() -> Result<()> {
    match Cli::parse().command {
        Command::Doctor => {
            #[cfg(target_os = "linux")]
            let desktop = match wayland::displays() {
                Ok((outputs, supported)) => {
                    serde_json::json!({"backend":"wayland", "outputs":outputs, "layer_shell":supported, "gnome_plugin":"experimental background probe; Apply not connected", "plasma_plugin":"experimental presentation bridge; Apply not connected"})
                }
                Err(error) => {
                    serde_json::json!({"backend":"unavailable", "reason":error.to_string()})
                }
            };
            #[cfg(not(target_os = "linux"))]
            let desktop = serde_json::json!({"backend":std::env::consts::OS, "hardware_qualification":"pending"});
            #[cfg(all(target_os = "linux", feature = "video"))]
            let linux_video = video::wayland_runtime_report();
            #[cfg(not(all(target_os = "linux", feature = "video")))]
            let linux_video = serde_json::Value::Null;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"version":env!("CARGO_PKG_VERSION"), "desktop":desktop, "linux_video":linux_video, "scene_support":"static 2D image layers; original sprite shader", "proprietary_engine_required":false, "static_scene_redraw":"on change only"})
                )?
            );
        }
        Command::Outputs => {
            #[cfg(target_os = "linux")]
            {
                let (outputs, layer_shell) = wayland::displays()?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"outputs":outputs,"layer_shell":layer_shell})
                    )?
                );
            }
            #[cfg(target_os = "macos")]
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "backend":"appkit", "outputs":macos_display::outputs()?,
                    "selector_lifetime":"current display configuration; not a persistent hardware identity",
                    "hardware_qualification":"pending"
                }))?
            );
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            anyhow::bail!("display enumeration is not implemented on this platform yet");
        }
        Command::Inspect { path } => {
            if let Some(video) = openwall_core::media::video_path(&path)? {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"schema_version":openwall_core::inspection::SCHEMA_VERSION,"title":video.file_stem().unwrap_or_default().to_string_lossy(), "kind":"video", "width":0, "height":0, "layers":0,"packed_entries":0, "unsupported":[], "metadata":"dimensions will be negotiated by the native decoder"})
                    )?
                );
            } else {
                let scene = Scene::load(path)?;
                let mut report = serde_json::to_value(&scene.report)?;
                report["schema_version"] = openwall_core::inspection::SCHEMA_VERSION.into();
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
        }
        Command::Render {
            path,
            output,
            width,
            height,
            allow_partial,
        } => {
            let scene = Scene::load(path)?;
            if !allow_partial {
                scene.require_supported()?;
            }
            gpu::render_png(
                &scene,
                &output,
                width.unwrap_or(scene.report.width),
                height.unwrap_or(scene.report.height),
            )?;
            println!("Rendered {}", output.display());
        }
        Command::Preview {
            path,
            seconds,
            allow_partial,
        } => {
            let path = path.unwrap_or_else(openwall_core::scene::sample_path);
            if let Some(video) = openwall_core::media::video_path(&path)? {
                return play_video(video, false, None, duration(seconds)?);
            }
            let scene = Scene::load(path)?;
            if !allow_partial {
                scene.require_supported()?;
            }
            preview::run(scene, duration(seconds)?, false, None)?;
        }
        Command::Apply {
            path,
            output,
            seconds,
        } => {
            let path = path.unwrap_or_else(openwall_core::scene::sample_path);
            if let Some(video) = openwall_core::media::video_path(&path)? {
                return play_video(video, true, output, duration(seconds)?);
            }
            let scene = Scene::load(path)?;
            scene.require_supported()?;
            #[cfg(target_os = "linux")]
            wayland::run(scene, output, duration(seconds)?)?;
            #[cfg(target_os = "macos")]
            preview::run(scene, duration(seconds)?, true, output)?;
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            anyhow::bail!(
                "desktop integration is currently implemented only for Wayland and macOS"
            );
        }
    }
    Ok(())
}

fn play_video(
    path: PathBuf,
    desktop: bool,
    output: Option<String>,
    duration: Option<Duration>,
) -> Result<()> {
    #[cfg(all(target_os = "linux", feature = "video"))]
    {
        if desktop
            || std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("WAYLAND_SOCKET").is_some()
        {
            video::prepare_wayland_runtime()?;
        }
        if desktop {
            wayland::run_video(path, output, duration)
        } else if std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("WAYLAND_SOCKET").is_some()
        {
            wayland::video_window::run(path, duration)
        } else {
            video::preview(&path, duration)
        }
    }
    #[cfg(target_os = "macos")]
    {
        preview::run_video(path, duration, desktop, output)
    }
    #[cfg(not(any(target_os = "macos", all(target_os = "linux", feature = "video"))))]
    {
        let _ = (path, desktop, output, duration);
        anyhow::bail!("native video playback is not available in this build")
    }
}

fn duration(seconds: Option<u64>) -> Result<Option<Duration>> {
    seconds
        .map(|seconds| {
            anyhow::ensure!(seconds <= 86400, "duration must not exceed one day");
            Ok(Duration::from_secs(seconds))
        })
        .transpose()
        .context("invalid duration")
}
