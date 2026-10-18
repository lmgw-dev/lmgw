//! An override's images (labs-store design §1.3, §1.4): the string elements
//! of its top-level `images` and `files` arrays, found where they are in the
//! text, so that a data URL can become `lab-artifact:<id>` in the stored
//! override and the id a data URL again in the bytes a cell sends, and
//! every other byte stays as typed.
//!
//! The text is read through borrowed `RawValue`s: each element is a slice of
//! the text itself, so its place is its pointer's offset. Nothing is
//! re-serialized, and a `Value` (which would sort the members) is never on
//! the way.

use lmgw_api_types::systemone::Ordered;
use serde_json::value::RawValue;

/// One string element of the `images` or `files` array: its JSON token's
/// byte range in the text, and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Element {
    pub start: usize,
    pub end: usize,
    pub value: String,
}

/// The string elements of `text`'s top-level `images` and `files` arrays, in
/// the order they are written. A member that is no array, or an element that
/// is no string, is not one; `Err` when `text` is no JSON object.
pub(crate) fn elements(text: &str) -> Result<Vec<Element>, String> {
    let body: Ordered<&RawValue> =
        serde_json::from_str(text).map_err(|e| format!("not a JSON object: {e}"))?;
    let base = text.as_ptr() as usize;
    let mut out = Vec::new();
    for (key, raw) in body.iter() {
        if key != "images" && key != "files" {
            continue;
        }
        let Ok(items) = serde_json::from_str::<Vec<&RawValue>>(raw.get()) else {
            continue;
        };
        for item in items {
            let Ok(value) = serde_json::from_str::<String>(item.get()) else {
                continue;
            };
            let start = item.get().as_ptr() as usize - base;
            out.push(Element {
                start,
                end: start + item.get().len(),
                value,
            });
        }
    }
    out.sort_by_key(|e| e.start);
    Ok(out)
}

/// `text` with each `(element, replacement)`'s token replaced by the JSON
/// string of its replacement. The elements come from [`elements`] on this
/// same text.
pub(crate) fn splice(text: &str, mut swaps: Vec<(&Element, String)>) -> String {
    swaps.sort_by_key(|(e, _)| e.start);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (e, with) in swaps {
        out.push_str(&text[at..e.start]);
        out.push_str(&serde_json::Value::String(with).to_string());
        at = e.end;
    }
    out.push_str(&text[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_elements_are_found_where_they_are_and_only_there() {
        let text = r#"{"state": "data:x", "images": [ "data:a" , 7, "b\"c"],
            "model": "m", "files": ["data:z"], "questions": {"images": ["no"]}}"#;
        let found = elements(text).unwrap();
        let values: Vec<&str> = found.iter().map(|e| e.value.as_str()).collect();
        assert_eq!(values, ["data:a", "b\"c", "data:z"]);
        for e in &found {
            assert!(text[e.start..e.end].starts_with('"') && text[e.start..e.end].ends_with('"'));
        }
        let out = splice(
            text,
            vec![
                (&found[0], "lab-artifact:4".into()),
                (&found[2], "lab-artifact:5".into()),
            ],
        );
        assert_eq!(
            out,
            r#"{"state": "data:x", "images": [ "lab-artifact:4" , 7, "b\"c"],
            "model": "m", "files": ["lab-artifact:5"], "questions": {"images": ["no"]}}"#,
            "every other byte as typed"
        );
        assert!(elements("[1]").is_err());
        assert!(elements(r#"{"images": "data:a"}"#).unwrap().is_empty());
    }
}
