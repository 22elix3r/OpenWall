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
    assert_eq!(report["layers"], 3);
    assert_eq!(report["packed_entries"], 10);
    assert_eq!(report["unsupported"], serde_json::json!([]));
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
