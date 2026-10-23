// SPDX-License-Identifier: AGPL-3.0-or-later
use std::{path::PathBuf, process::Command};

fn sample(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../samples")
        .join(name)
}

#[test]
fn packed_scene_is_inspected_from_a_separate_process() {
    let result = Command::new(env!("CARGO_BIN_EXE_openwall"))
        .arg("inspect")
        .arg(sample("aurora-packed/project.json"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        report["schema_version"],
        openwall_core::inspection::SCHEMA_VERSION
    );
    assert!(openwall_core::inspection::Inspection::from_json(&result.stdout).is_ok());
    assert_eq!(report["layers"], 3);
    assert_eq!(report["packed_entries"], 10);
    assert_eq!(report["unsupported"], serde_json::json!([]));
}

#[test]
fn video_inspection_reports_versioned_deferred_metadata_without_decoding() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("undecoded-original.webm");
    std::fs::write(&path, b"inspection does not decode this fixture").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_openwall"))
        .arg("inspect")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let inspection = openwall_core::inspection::Inspection::from_json(&result.stdout).unwrap();
    assert_eq!(inspection.report.kind, "video");
    assert_eq!(
        (
            inspection.report.width,
            inspection.report.height,
            inspection.report.layers,
            inspection.report.packed_entries
        ),
        (0, 0, 0, 0)
    );
}

#[test]
fn missing_asset_fails_cleanly() {
    let result = Command::new(env!("CARGO_BIN_EXE_openwall"))
        .arg("inspect")
        .arg("/openwall-nonexistent/project.json")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("does not exist"));
}

#[cfg(target_os = "linux")]
#[test]
fn desktop_shells_require_their_adapters_instead_of_claiming_invisible_success() {
    for desktop in ["KDE", "GNOME"] {
        let result = Command::new(env!("CARGO_BIN_EXE_openwall"))
            .env("XDG_CURRENT_DESKTOP", desktop)
            .env_remove("WAYLAND_DISPLAY")
            .arg("apply")
            .arg(sample("aurora/project.json"))
            .arg("--seconds")
            .arg("0")
            .output()
            .unwrap();
        assert!(!result.status.success(), "{desktop}");
        assert!(String::from_utf8_lossy(&result.stderr).contains("dedicated desktop integration"));
    }
}

#[test]
fn gpu_render_matches_original_reference_art() {
    if std::env::var_os("OPENWALL_TEST_GPU").is_none() {
        eprintln!("GPU test not requested; set OPENWALL_TEST_GPU=1 on a Vulkan/Metal host");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("render.png");
    let result = Command::new(env!("CARGO_BIN_EXE_openwall"))
        .arg("render")
        .arg(sample("aurora-packed/project.json"))
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let actual = image::open(output).unwrap().to_rgba8();
    let expected = image::open(sample("aurora/preview.png"))
        .unwrap()
        .to_rgba8();
    assert_eq!(actual.dimensions(), expected.dimensions());
    let error: u64 = actual
        .as_raw()
        .iter()
        .zip(expected.as_raw())
        .map(|(a, b)| a.abs_diff(*b) as u64)
        .sum();
    let mean = error as f64 / actual.as_raw().len() as f64;
    assert!(
        mean < 0.5,
        "mean per-channel error {mean} exceeds tolerance"
    );
}

#[test]
fn gpu_applies_position_rotation_and_linear_alpha_blending() {
    if std::env::var_os("OPENWALL_TEST_GPU").is_none() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let write = |name: &str, value: serde_json::Value| {
        std::fs::write(root.path().join(name), serde_json::to_vec(&value).unwrap()).unwrap();
    };
    image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]))
        .save(root.path().join("red.png"))
        .unwrap();
    write(
        "project.json",
        serde_json::json!({"title":"Transform conformance", "type":"scene", "file":"scene.json"}),
    );
    write(
        "scene.json",
        serde_json::json!({"general":{"clearcolor":"0 0 0", "orthogonalprojection":{"width":32,"height":24}}, "objects":[{"image":"model.json","origin":"8 8 0","angles":"0 0 90","alpha":0.5}]}),
    );
    write(
        "model.json",
        serde_json::json!({"width":8,"height":4,"material":"material.json"}),
    );
    write(
        "material.json",
        serde_json::json!({"shader":"genericimage","textures":["red.png"]}),
    );
    let output = root.path().join("render.png");
    let result = Command::new(env!("CARGO_BIN_EXE_openwall"))
        .arg("render")
        .arg(root.path().join("project.json"))
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let image = image::open(output).unwrap().to_rgba8();
    assert_eq!(
        image.get_pixel(11, 16).0,
        [0, 0, 0, 255],
        "rotated narrow edge is misplaced"
    );
    let color = image.get_pixel(8, 13).0;
    assert!(
        (187..=189).contains(&color[0]),
        "alpha blending must happen in linear light: {color:?}"
    );
    assert_eq!([color[1], color[2], color[3]], [0, 0, 255]);
}

#[test]
fn thumbnail_is_windowless_bounded_and_never_overwrites_existing_files() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("preview.png");
    let invoke = || {
        Command::new(env!("CARGO_BIN_EXE_openwall"))
            .arg("thumbnail")
            .arg(sample("aurora"))
            .arg("--output")
            .arg(&output)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .output()
            .unwrap()
    };
    let result = invoke();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.stdout.is_empty());
    let bytes = std::fs::read(&output).unwrap();
    assert!(bytes.len() <= openwall_core::thumbnail::MAX_PNG_BYTES);
    let png = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).unwrap();
    assert!(png.width() <= 512 && png.height() <= 512);
    let second = invoke();
    assert!(!second.status.success(), "existing file was overwritten");
    assert_eq!(std::fs::read(output).unwrap(), bytes);
}
