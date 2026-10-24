// SPDX-License-Identifier: AGPL-3.0-or-later
//! Windowless conformance for device-owned scene image uploads and painter lifetime.
#[path = "../src/gpu.rs"]
#[allow(dead_code)]
mod gpu;
use anyhow::{Context, Result, ensure};
use openwall_core::{CompatibilityReport, Layer, Scene};
use std::{path::PathBuf, sync::Arc};

fn scene(colors: [[u8; 4]; 2]) -> Scene {
    let images =
        colors.map(|color| Arc::new(image::RgbaImage::from_pixel(8, 8, image::Rgba(color))));
    Scene {
        report: CompatibilityReport {
            title: "Original shared-image fixture".into(),
            kind: "scene".into(),
            width: 64,
            height: 64,
            layers: 4,
            packed_entries: 0,
            unsupported: Vec::new(),
        },
        background: [0.1, 0.1, 0.1, 1.0],
        layers: [
            (16.0, 16.0, 0),
            (48.0, 16.0, 1),
            (16.0, 48.0, 1),
            (48.0, 48.0, 0),
        ]
        .into_iter()
        .map(|(x, y, index)| Layer {
            pixels: images[index].clone(),
            center: [x, y],
            size: [32.0, 32.0],
            rotation: 0.0,
            opacity: 1.0,
        })
        .collect(),
    }
}

fn pixels(
    gpu: &gpu::Gpu,
    painter: &gpu::Painter,
    width: u32,
    height: u32,
    colors: [[u8; 4]; 2],
) -> Result<Vec<u8>> {
    let rgba = gpu::render_rgba(gpu, painter, width, height)?;
    for (x, y, index) in [(1, 1, 1), (3, 1, 0), (1, 3, 0), (3, 3, 1)] {
        let at = ((height * y / 4) * width + width * x / 4) as usize * 4;
        ensure!(
            rgba[at..at + 4] == colors[index],
            "shared scene pixels differ at ({x}/4,{y}/4)"
        );
    }
    Ok(rgba)
}

fn main() -> Result<()> {
    let output = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .context("provide a new output directory")?;
    std::fs::create_dir(&output)?;
    let gpu = pollster::block_on(gpu::Gpu::new(gpu::Gpu::instance(), None))?;
    let colors = [[255, 0, 0, 255], [0, 255, 0, 255]];
    let document = scene(colors);
    let source = Arc::downgrade(&document.layers[0].pixels);
    let first = gpu::Painter::new(&gpu, &document, wgpu::TextureFormat::Rgba8UnormSrgb, 64, 64);
    let second = gpu::Painter::new(&gpu, &document, wgpu::TextureFormat::Bgra8UnormSrgb, 64, 64);
    let shared = gpu.texture_stats();
    ensure!(
        shared.uploads == 2 && shared.live_images == 2 && shared.live_rgba_bytes == 512,
        "duplicate per-layer/per-painter uploads: {shared:?}"
    );
    let baseline = pixels(&gpu, &first, 64, 64, colors)?;
    ensure!(
        pixels(&gpu, &second, 64, 64, colors)? == baseline,
        "RGBA/BGRA pipelines disagree"
    );
    image::save_buffer(
        output.join("shared.png"),
        &baseline,
        64,
        64,
        image::ColorType::Rgba8,
    )?;
    drop(first);
    ensure!(
        gpu.texture_stats() == shared,
        "first painter retirement released shared resources"
    );
    drop(document);
    ensure!(
        source.upgrade().is_none(),
        "GPU cache retained decoded CPU pixels"
    );
    ensure!(
        pixels(&gpu, &second, 64, 64, colors)? == baseline,
        "retained painter lost pixels"
    );

    let changed_colors = [[0, 0, 255, 255], [255, 255, 0, 255]];
    let changed = scene(changed_colors);
    let third = gpu::Painter::new(&gpu, &changed, wgpu::TextureFormat::Rgba8UnormSrgb, 64, 64);
    let distinct = gpu.texture_stats();
    ensure!(
        distinct.uploads == 4 && distinct.live_images == 4 && distinct.live_rgba_bytes == 1024,
        "distinct image identities aliased: {distinct:?}"
    );
    let newer = pixels(&gpu, &third, 64, 64, changed_colors)?;
    image::save_buffer(
        output.join("distinct.png"),
        &newer,
        64,
        64,
        image::ColorType::Rgba8,
    )?;
    ensure!(
        pixels(&gpu, &second, 64, 64, colors)? == baseline,
        "new source changed retained source"
    );
    second.resize(&gpu, &changed, 128, 64); // Geometry is identical; images must remain its originals.
    pixels(&gpu, &second, 128, 64, colors)?;
    drop(third);
    ensure!(
        gpu.texture_stats() == shared_with_uploads(shared, 4),
        "temporary source retirement released retained images"
    );
    drop(second);
    let retired = gpu.texture_stats();
    ensure!(
        retired.uploads == 4 && retired.live_images == 0 && retired.live_rgba_bytes == 0,
        "last painter retained textures: {retired:?}"
    );

    let replacement =
        gpu::Painter::new(&gpu, &changed, wgpu::TextureFormat::Rgba8UnormSrgb, 64, 64);
    let recreated = gpu.texture_stats();
    ensure!(
        recreated.uploads == 6 && recreated.live_images == 2,
        "retired cache did not recreate uploads: {recreated:?}"
    );
    pixels(&gpu, &replacement, 64, 64, changed_colors)?;
    let other_gpu = pollster::block_on(gpu::Gpu::new(gpu::Gpu::instance(), None))?;
    let independent = gpu::Painter::new(
        &other_gpu,
        &changed,
        wgpu::TextureFormat::Bgra8UnormSrgb,
        64,
        64,
    );
    ensure!(
        other_gpu.texture_stats().uploads == 2 && gpu.texture_stats() == recreated,
        "cache crossed GPU devices"
    );
    pixels(&other_gpu, &independent, 64, 64, changed_colors)?;
    drop(independent);
    ensure!(
        other_gpu.texture_stats().live_images == 0,
        "independent device retained images"
    );
    drop(replacement);
    ensure!(
        gpu.texture_stats().live_images == 0,
        "replacement retirement retained images"
    );
    println!(
        "{}",
        serde_json::json!({"shared_scene_textures":"passed", "gpu":gpu.adapter.get_info().name,
        "shared":shared,"distinct":distinct,"retired":retired,"recreated":recreated,
        "checks":["shared images uploaded once per device", "RGBA/BGRA pipelines", "independent painter geometry", "one painter retirement", "CPU image release", "distinct source pixels", "last painter retirement", "recreated uploads", "independent device ownership"]})
    );
    Ok(())
}

fn shared_with_uploads(mut stats: gpu::TextureStats, uploads: u64) -> gpu::TextureStats {
    stats.uploads = uploads;
    stats
}
