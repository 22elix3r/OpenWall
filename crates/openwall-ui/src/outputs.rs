// SPDX-License-Identifier: AGPL-3.0-or-later
//! Labels are presentation text; selectors are the worker's exact output identifiers.
use serde_json::{Value, json};

pub fn choices(discovery: Option<&Value>) -> String {
    let label = if cfg!(target_os = "macos") {
        "Default display"
    } else {
        "All displays"
    };
    let mut choices = vec![json!({"label":label, "selector":""})];
    if let Some(displays) = discovery.and_then(|value| value["outputs"].as_array()) {
        for display in displays {
            let name = display["name"].as_str().filter(|name| !name.is_empty());
            let selector = match display.get("selector") {
                None => name,
                Some(value) => value.as_str().filter(|selector| !selector.is_empty()),
            };
            let label = display["label"]
                .as_str()
                .filter(|label| !label.is_empty())
                .or(name);
            if let (Some(selector), Some(label)) = (selector, label) {
                choices.push(json!({"label":label, "selector":selector}));
            }
        }
    }
    Value::Array(choices).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_human_names_keep_distinct_selection_values() {
        let discovered = json!({"outputs":[
            {"name":"Same monitor", "label":"Same monitor · Display 1", "selector":"macos:42"},
            {"name":"Same monitor", "label":"Same monitor · Display 2", "selector":"macos:87"}
        ]});
        let model: Value = serde_json::from_str(&choices(Some(&discovered))).unwrap();
        assert_eq!(model[0]["selector"], "");
        assert_eq!(model[1]["selector"], "macos:42");
        assert_eq!(model[2]["selector"], "macos:87");
        assert_eq!(model[2]["label"], "Same monitor · Display 2");
    }

    #[test]
    fn linux_names_remain_selectors_and_invalid_explicit_selectors_are_ignored() {
        let discovered = json!({"outputs":[
            {"name":"DP-1"}, {"name":"HDMI-A-1"},
            {"name":"invalid", "selector":""}, {"name":"invalid", "selector":42}, {}
        ]});
        let model: Value = serde_json::from_str(&choices(Some(&discovered))).unwrap();
        assert_eq!(model.as_array().unwrap().len(), 3);
        assert_eq!(model[1], json!({"label":"DP-1", "selector":"DP-1"}));
        assert_eq!(model[2], json!({"label":"HDMI-A-1", "selector":"HDMI-A-1"}));
        assert_eq!(
            serde_json::from_str::<Value>(&choices(None))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
