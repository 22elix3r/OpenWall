// SPDX-License-Identifier: AGPL-3.0-or-later
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::Value;

use crate::Assets;

#[derive(Debug, Clone, Serialize)]
pub struct CompatibilityReport {
    pub title: String,
    pub kind: String,
    pub width: u32,
    pub height: u32,
    pub layers: usize,
    pub packed_entries: usize,
    pub unsupported: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Layer {
    pub pixels: Arc<image::RgbaImage>,
    pub center: [f32; 2],
    pub size: [f32; 2],
    pub rotation: f32,
    pub opacity: f32,
}

#[derive(Debug, Clone)]
pub struct Scene {
    pub report: CompatibilityReport,
    pub background: [f64; 4],
    pub layers: Vec<Layer>,
}

impl Scene {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path
            .as_ref()
            .canonicalize()
            .context("wallpaper path does not exist")?;
        let (root, main) = if path.is_dir() {
            (path.clone(), "project.json".to_owned())
        } else {
            (
                path.parent()
                    .context("wallpaper has no parent directory")?
                    .to_path_buf(),
                path.file_name()
                    .context("wallpaper has no filename")?
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        let assets = Assets::open(root)?;
        if !main.ends_with(".json") {
            return Self::from_image(&assets, &main);
        }
        let project = assets.json(&main)?;
        let title = project
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Untitled wallpaper")
            .to_owned();
        let kind = project
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("scene");
        let file = project
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or("scene.json");
        match kind {
            "image" => {
                let mut scene = Self::from_image(&assets, file)?;
                scene.report.title = title;
                Ok(scene)
            }
            "scene" => Self::from_document(&assets, &assets.json(file)?, title),
            "video" => bail!(
                "video projects use the native video worker; choose Preview or Apply with the video file"
            ),
            "web" => bail!("web wallpapers are not supported yet; no web content was executed"),
            _ => bail!("unsupported wallpaper type: {kind}"),
        }
    }

    fn from_image(assets: &Assets, name: &str) -> Result<Self> {
        let pixels = Arc::new(assets.image(name)?);
        let (width, height) = pixels.dimensions();
        ensure!(width > 0 && height > 0, "empty image");
        Ok(Self {
            report: CompatibilityReport {
                title: name.to_owned(),
                kind: "image".into(),
                width,
                height,
                layers: 1,
                packed_entries: assets.packed_entries(),
                unsupported: Vec::new(),
            },
            background: [0.02, 0.02, 0.02, 1.0],
            layers: vec![Layer {
                pixels,
                center: [width as f32 / 2.0, height as f32 / 2.0],
                size: [width as f32, height as f32],
                rotation: 0.0,
                opacity: 1.0,
            }],
        })
    }

    fn from_document(assets: &Assets, document: &Value, title: String) -> Result<Self> {
        let general = &document["general"];
        let width = dimension(&general["orthogonalprojection"]["width"], 1920)?;
        let height = dimension(&general["orthogonalprojection"]["height"], 1080)?;
        ensure!(
            width > 0 && height > 0 && width <= 16384 && height <= 16384,
            "scene dimensions must be between 1 and 16384"
        );
        let rgb = vector(&general["clearcolor"], [0.025, 0.035, 0.06])?;
        let mut scene = Self {
            report: CompatibilityReport {
                title,
                kind: "scene".into(),
                width: width as u32,
                height: height as u32,
                layers: 0,
                packed_entries: assets.packed_entries(),
                unsupported: Vec::new(),
            },
            background: [rgb[0] as f64, rgb[1] as f64, rgb[2] as f64, 1.0],
            layers: Vec::new(),
        };
        let objects = document["objects"]
            .as_array()
            .context("scene must contain an objects array")?;
        ensure!(objects.len() <= 1024, "scene exceeds 1024 layers");
        collect_unsupported(document, "scene", &mut scene.report.unsupported);
        unknown_fields(
            document,
            &["general", "objects"],
            "scene",
            &mut scene.report.unsupported,
        );
        unknown_fields(
            &general["orthogonalprojection"],
            &["width", "height"],
            "projection",
            &mut scene.report.unsupported,
        );
        unknown_fields(
            general,
            &["clearcolor", "orthogonalprojection", "parallax"],
            "general",
            &mut scene.report.unsupported,
        );
        if document.get("camera").is_some() {
            scene
                .report
                .unsupported
                .push("scene.camera: custom cameras are not implemented".into());
        }
        let mut textures: BTreeMap<String, Arc<image::RgbaImage>> = BTreeMap::new();
        let mut decoded_bytes = 0u64;
        for (index, object) in objects.iter().enumerate() {
            if object["visible"].as_bool() == Some(false) {
                continue;
            }
            let Some(model_name) = object["image"].as_str() else {
                scene.report.unsupported.push(format!(
                    "layer {index}: only image layers are currently supported"
                ));
                continue;
            };
            let model = assets.json(model_name)?;
            unknown_fields(
                object,
                &[
                    "id", "name", "image", "origin", "scale", "angles", "alpha", "visible",
                ],
                &format!("layer {index}"),
                &mut scene.report.unsupported,
            );
            unknown_fields(
                &model,
                &["material", "width", "height"],
                &format!("model {model_name}"),
                &mut scene.report.unsupported,
            );
            let material_name = model["material"]
                .as_str()
                .context("image model is missing its material")?;
            let material = assets.json(material_name)?;
            unknown_fields(
                &material,
                &["shader", "textures"],
                &format!("material {material_name}"),
                &mut scene.report.unsupported,
            );
            let shader = material["shader"].as_str().unwrap_or("genericimage");
            if !matches!(shader, "genericimage" | "genericimage2" | "openwall/sprite") {
                scene.report.unsupported.push(format!(
                    "layer {index}: shader {shader:?} is not implemented"
                ));
                continue;
            }
            let texture = material["textures"]
                .as_array()
                .and_then(|x| x.first())
                .and_then(Value::as_str)
                .context("material is missing its first texture")?;
            if material["textures"].as_array().is_some_and(|x| x.len() > 1) {
                scene.report.unsupported.push(format!(
                    "layer {index}: multi-texture materials are not implemented"
                ));
            }
            let texture = if Path::new(texture).extension().is_none() {
                format!("{texture}.tex")
            } else {
                texture.to_owned()
            };
            let pixels = if let Some(image) = textures.get(&texture) {
                image.clone()
            } else {
                let image = Arc::new(assets.image(&texture)?);
                decoded_bytes += image.as_raw().len() as u64;
                ensure!(
                    decoded_bytes <= 256 * 1024 * 1024,
                    "scene exceeds decoded texture memory budget"
                );
                textures.insert(texture.clone(), image.clone());
                image
            };
            let size = [
                model["width"].as_f64().unwrap_or(pixels.width() as f64) as f32,
                model["height"].as_f64().unwrap_or(pixels.height() as f64) as f32,
            ];
            let origin = vector(
                &object["origin"],
                [width as f32 / 2.0, height as f32 / 2.0, 0.0],
            )?;
            ensure!(
                origin.iter().all(|x| x.abs() <= 10_000_000.0),
                "layer origin exceeds coordinate limits"
            );
            let scale = vector(&object["scale"], [1.0, 1.0, 1.0])?;
            let angles = vector(&object["angles"], [0.0; 3])?;
            if angles[0] != 0.0 || angles[1] != 0.0 || origin[2] != 0.0 {
                scene
                    .report
                    .unsupported
                    .push(format!("layer {index}: 3D transforms are not implemented"));
            }
            let opacity = object["alpha"].as_f64().unwrap_or(1.0) as f32;
            ensure!(
                opacity.is_finite() && (0.0..=1.0).contains(&opacity),
                "layer alpha must be between zero and one"
            );
            ensure!(
                size.iter()
                    .all(|x| x.is_finite() && *x > 0.0 && *x <= 16384.0),
                "invalid layer dimensions"
            );
            ensure!(
                (size[0] * scale[0]).is_finite() && (size[1] * scale[1]).is_finite(),
                "scaled layer dimensions overflow"
            );
            scene.layers.push(Layer {
                pixels,
                center: [origin[0], origin[1]],
                size: [size[0] * scale[0], size[1] * scale[1]],
                rotation: angles[2].to_radians(),
                opacity,
            });
        }
        scene.report.layers = scene.layers.len();
        scene.report.unsupported.sort();
        scene.report.unsupported.dedup();
        Ok(scene)
    }

    pub fn require_supported(&self) -> Result<()> {
        ensure!(
            self.report.unsupported.is_empty(),
            "wallpaper uses unsupported features:\n{}",
            self.report.unsupported.join("\n")
        );
        Ok(())
    }
}

fn dimension(value: &Value, fallback: u64) -> Result<u64> {
    if value.is_null() {
        return Ok(fallback);
    }
    value
        .as_u64()
        .context("scene dimensions must be positive integers")
}

fn unknown_fields(value: &Value, supported: &[&str], path: &str, out: &mut Vec<String>) {
    if let Some(object) = value.as_object() {
        for key in object.keys() {
            if !supported.contains(&key.as_str()) {
                out.push(format!("{path}.{key}: not implemented"));
            }
        }
    }
}

fn vector(value: &Value, default: [f32; 3]) -> Result<[f32; 3]> {
    if value.is_null() {
        return Ok(default);
    }
    let values: Vec<f32> = if let Some(text) = value.as_str() {
        text.split_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()?
    } else if let Some(array) = value.as_array() {
        array
            .iter()
            .map(|x| {
                x.as_f64()
                    .map(|x| x as f32)
                    .context("vector components must be numbers")
            })
            .collect::<Result<_>>()?
    } else {
        bail!("expected a vector string or array")
    };
    ensure!(
        values.len() == 3 && values.iter().all(|x| x.is_finite()),
        "vectors must have three finite components"
    );
    Ok([values[0], values[1], values[2]])
}

fn collect_unsupported(value: &Value, path: &str, out: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(
                    key.as_str(),
                    "script" | "scripts" | "effects" | "puppet" | "animations"
                ) && !value.is_null()
                    && value.as_array().is_none_or(|x| !x.is_empty())
                {
                    out.push(format!("{path}.{key}: not implemented"));
                }
                if key == "parallax" && value.as_bool() == Some(true) {
                    out.push(format!("{path}.parallax: not implemented"));
                }
                collect_unsupported(value, &format!("{path}.{key}"), out);
            }
        }
        Value::Array(array) => {
            for (index, value) in array.iter().enumerate() {
                collect_unsupported(value, &format!("{path}[{index}]"), out);
            }
        }
        _ => {}
    }
}

pub fn sample_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/aurora/project.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_scene_loads_without_engine_assets() {
        let scene = Scene::load(sample_path()).unwrap();
        scene.require_supported().unwrap();
        assert_eq!((scene.report.width, scene.report.height), (1280, 720));
        assert_eq!(scene.layers.len(), 3);
    }

    #[test]
    fn packed_and_loose_scenes_have_identical_pixels() {
        let loose = Scene::load(sample_path()).unwrap();
        let packed = Scene::load(
            sample_path()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("aurora-packed/project.json"),
        )
        .unwrap();
        packed.require_supported().unwrap();
        assert_eq!(packed.report.packed_entries, 10);
        for (a, b) in loose.layers.iter().zip(&packed.layers) {
            assert_eq!(a.pixels, b.pixels);
        }
    }

    #[test]
    fn reports_unimplemented_code_and_effects() {
        let mut report = Vec::new();
        collect_unsupported(
            &serde_json::json!({"objects":[{"effects":[{}], "script":"while(true){}"}]}),
            "scene",
            &mut report,
        );
        assert_eq!(report.len(), 2);
    }

    #[test]
    fn rejects_nonfinite_and_malformed_vectors() {
        for v in ["NaN 0 0", "inf 0 0", "1 2", "1 2 3 4"] {
            assert!(vector(&Value::from(v), [0.0; 3]).is_err());
        }
    }

    #[test]
    fn malformed_dimensions_are_not_silently_defaulted() {
        for value in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("1920"),
        ] {
            assert!(dimension(&value, 1920).is_err());
        }
        assert_eq!(dimension(&Value::Null, 1920).unwrap(), 1920);
    }

    #[test]
    fn root_scene_features_are_reported() {
        let mut report = Vec::new();
        unknown_fields(
            &serde_json::json!({"general":{}, "objects":[], "timeline":{}}),
            &["general", "objects"],
            "scene",
            &mut report,
        );
        assert_eq!(report, ["scene.timeline: not implemented"]);
    }
}
