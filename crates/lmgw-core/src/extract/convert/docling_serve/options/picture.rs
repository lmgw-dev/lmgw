//! docling's picture-description settings objects (`picture_description_api`,
//! `_local`, `_custom_config`) read key by key (design §7.4.2): the note
//! names every key the object has and what became of it, so nothing a
//! caller sends there is dropped unsaid. Checked against docling-serve
//! 1.38.2 (docling 2.142.1, docling-jobkit's `PictureDescriptionApi`,
//! `PictureDescriptionLocal` and the engine options a custom config takes),
//! 2026-11-19.
//!
//! - `prompt`: the native `picture_prompt`;
//! - the model's own name, generation, connection and batching settings
//!   ([`REPLACED`]): the configured picture-description model stands in;
//! - `url` and `headers`: never followed (D21);
//! - `picture_area_threshold`: docling-serve overwrites it with the
//!   top-level `picture_description_area_threshold`, which is the native
//!   `describe_min_area`;
//! - `classification_allow`, `_deny`, `_min_confidence`: the picture
//!   classification gate on descriptions is not built yet (§12);
//! - any other key: named as not used (bounded as unknown option names are,
//!   [`super::unknown`]).

use serde_json::Value;

/// The settings the configured model stands in for.
pub const REPLACED: &[&str] = &[
    "repo_id",
    "generation_config",
    "params",
    "timeout",
    "concurrency",
    "batch_size",
    "scale",
    "engine_options",
    "model_spec",
    "provenance",
    "usage_response_key",
    "padding_side",
    "kind",
];

/// Never followed: lmgw calls nothing a caller names (D21).
const UNFOLLOWED: &[&str] = &["url", "headers"];

/// Not built yet (§12).
const CLASSIFICATION: &[&str] = &[
    "classification_allow",
    "classification_deny",
    "classification_min_confidence",
];

/// The keys docling-serve overwrites from a top-level option.
const OVERWRITTEN: &str = "picture_area_threshold";

/// `names` joined with "and" (a note's detail holds no commas).
fn and(names: &[String]) -> String {
    names.join(" and ")
}

/// The note's detail for the settings object `v`: `model` (the
/// picture-description model's words), then what became of each key.
pub fn detail(model: &str, v: &Value) -> String {
    let empty = serde_json::Map::new();
    let keys = v.as_object().unwrap_or(&empty);
    let of = |list: &[&str]| -> Vec<String> {
        list.iter()
            .filter(|k| keys.contains_key(**k))
            .map(|k| (*k).to_string())
            .collect()
    };
    let mut d = model.to_string();
    let replaced = of(REPLACED);
    if !replaced.is_empty() {
        d.push_str(&format!(" in place of {}", and(&replaced)));
    }
    let unfollowed = of(UNFOLLOWED);
    if !unfollowed.is_empty() {
        d.push_str(&format!(" with its {} not followed", and(&unfollowed)));
    }
    if keys.contains_key(OVERWRITTEN) {
        d.push_str(&format!(
            " with its {OVERWRITTEN} replaced by picture_description_area_threshold"
        ));
    }
    let classification = of(CLASSIFICATION);
    if !classification.is_empty() {
        d.push_str(&format!(" with {} not yet", and(&classification)));
    }
    let mut names = super::unknown::Names::default();
    let other: Vec<String> = keys
        .keys()
        .filter(|k| {
            k.as_str() != "prompt"
                && k.as_str() != OVERWRITTEN
                && ![REPLACED, UNFOLLOWED, CLASSIFICATION]
                    .iter()
                    .any(|l| l.contains(&k.as_str()))
        })
        .filter_map(|k| names.note(k).map(|n| n.name))
        .collect();
    if !other.is_empty() {
        d.push_str(&format!(" with {} not used", and(&other)));
    }
    if let Some(rest) = names.more() {
        d.push_str(&format!(" and {rest} more keys not used"));
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_key_of_the_object_is_named() {
        let d = detail(
            "picture-description model",
            &json!({
                "prompt": "Describe.", "url": "https://vision.example/v1",
                "headers": {"x-example": "1"}, "params": {}, "timeout": 20,
                "engine_options": {}, "model_spec": {}, "picture_area_threshold": 0.2,
                "classification_allow": ["bar_chart"], "example_key": 1,
            }),
        );
        assert_eq!(
            d,
            "picture-description model in place of params and timeout and engine_options and \
             model_spec with its url and headers not followed with its picture_area_threshold \
             replaced by picture_description_area_threshold with classification_allow not yet \
             with example_key not used"
        );
        assert_eq!(
            detail("m", &json!({"prompt": "Describe."})),
            "m",
            "the prompt alone is used as it is"
        );
    }

    #[test]
    fn many_unknown_keys_are_bounded() {
        let many: serde_json::Map<String, Value> = (0..super::super::unknown::MAX_LISTED + 5)
            .map(|i| (format!("example_{i:03}"), json!(1)))
            .collect();
        let d = detail("m", &Value::Object(many));
        assert!(d.ends_with(" and 5 more keys not used"), "{d}");
    }
}
