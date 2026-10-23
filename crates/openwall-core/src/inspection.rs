// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bounded, versioned inspection responses shared by the CLI and GUI.
use crate::CompatibilityReport;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_REPORT_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
pub struct Inspection {
    pub schema_version: u32,
    #[serde(flatten)]
    pub report: CompatibilityReport,
}

impl Inspection {
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_REPORT_BYTES,
            "inspection report exceeds the GUI size limit"
        );
        let inspection: Self =
            serde_json::from_slice(bytes).context("invalid inspection report")?;
        ensure!(
            inspection.schema_version == SCHEMA_VERSION,
            "incompatible inspection report version; install matching OpenWall executables"
        );
        match inspection.report.kind.as_str() {
            "image" | "scene" => ensure!(
                inspection.report.width > 0 && inspection.report.height > 0,
                "inspection report has invalid dimensions"
            ),
            "video" => {} // Dimensions are negotiated later by the native decoder.
            _ => bail!("inspection report has an unsupported content kind"),
        }
        Ok(inspection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn report() -> serde_json::Value {
        json!({"schema_version":SCHEMA_VERSION,"title":"Original fixture","kind":"scene",
            "width":1280,"height":720,"layers":3,"packed_entries":10,"unsupported":[]})
    }

    #[test]
    fn valid_scene_and_deferred_video_metadata_preserve_compatibility_details() {
        let mut value = report();
        value["unsupported"] = json!(["unimplemented material"]);
        let parsed = Inspection::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(parsed.report.packed_entries, 10);
        assert_eq!(parsed.report.unsupported, ["unimplemented material"]);
        value["kind"] = json!("video");
        value["width"] = json!(0);
        value["height"] = json!(0);
        value["metadata"] = json!("dimensions are deferred");
        assert!(Inspection::from_json(&serde_json::to_vec(&value).unwrap()).is_ok());
    }

    #[test]
    fn incomplete_mismatched_and_malformed_success_responses_are_rejected() {
        for (key, invalid) in [
            ("schema_version", json!(2)),
            ("kind", json!("unknown")),
            ("width", json!(0)),
            ("height", json!(-1)),
            ("layers", json!("three")),
            ("unsupported", json!([42])),
        ] {
            let mut value = report();
            value[key] = invalid;
            assert!(
                Inspection::from_json(&serde_json::to_vec(&value).unwrap()).is_err(),
                "{key}"
            );
        }
        for bytes in [b"null".as_slice(), b"{}", b"[]", b"invalid", b"\xff"] {
            assert!(Inspection::from_json(bytes).is_err());
        }
        let mut missing = report();
        missing.as_object_mut().unwrap().remove("schema_version");
        assert!(Inspection::from_json(&serde_json::to_vec(&missing).unwrap()).is_err());
        assert!(Inspection::from_json(&vec![b' '; MAX_REPORT_BYTES + 1]).is_err());
    }
}
