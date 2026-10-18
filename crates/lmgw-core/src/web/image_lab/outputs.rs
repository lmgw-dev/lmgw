//! A generation's pictures as output artifacts (labs-store design §5): each
//! `data[i].b64_json` of the route's answer, found where it is in the text,
//! so the stored answer names the kept file as `lab-artifact:<id>` in its
//! place and every other byte stays as the route sent it.
//!
//! The text is read through borrowed `RawValue`s (as the Decision lab's
//! override images are): each string is a slice of the text itself, so its
//! place is its pointer's offset, and nothing is re-serialized.

use axum::body::Bytes;
use base64::Engine as _;
use lmgw_api_types::systemone::Ordered;
use serde_json::value::RawValue;

/// One picture in the answer: its JSON string token's byte range in the
/// text and its decoded bytes (taken for its artifact, the range kept for
/// the splice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Picture {
    pub start: usize,
    pub end: usize,
    pub bytes: Bytes,
}

/// Every `data[i].b64_json` string of `text` that decodes as base64, in
/// order. One that does not decode stays inline (it is the route's text);
/// an answer that is no object with a `data` array has none.
pub(super) fn pictures(text: &str) -> Vec<Picture> {
    let Ok(body) = serde_json::from_str::<Ordered<&RawValue>>(text) else {
        return Vec::new();
    };
    let base = text.as_ptr() as usize;
    let mut out = Vec::new();
    for (key, raw) in body.iter() {
        if key != "data" {
            continue;
        }
        let Ok(items) = serde_json::from_str::<Vec<&RawValue>>(raw.get()) else {
            continue;
        };
        for item in items {
            let Ok(fields) = serde_json::from_str::<Ordered<&RawValue>>(item.get()) else {
                continue;
            };
            for (name, value) in fields.iter() {
                if name != "b64_json" {
                    continue;
                }
                let Ok(b64) = serde_json::from_str::<String>(value.get()) else {
                    continue;
                };
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
                    continue;
                };
                let start = value.get().as_ptr() as usize - base;
                out.push(Picture {
                    start,
                    end: start + value.get().len(),
                    bytes: Bytes::from(bytes),
                });
            }
        }
    }
    out
}

/// `text` with each picture's token replaced by the JSON string of its
/// replacement, in order: `refs[i]` for `pictures[i]`.
pub(super) fn splice(text: &str, pictures: &[Picture], refs: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (p, with) in pictures.iter().zip(refs) {
        out.push_str(&text[at..p.start]);
        out.push_str(&serde_json::Value::String(with.clone()).to_string());
        at = p.end;
    }
    out.push_str(&text[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_picture_is_found_where_it_is_and_replaced_there_only() {
        let text = r#"{"created": 1, "data": [ {"b64_json": "aVZC" , "revised_prompt": "b64_json"},
            {"url": "https://img.example/x.png"}, {"b64_json":"AAAA"}, {"b64_json": "***"} ],
            "output_format": "png", "note": {"data": [{"b64_json": "QUJD"}]}}"#;
        let found = pictures(text);
        assert_eq!(
            found.len(),
            2,
            "two decode, one is no base64, a nested one is not data"
        );
        assert_eq!(found[0].bytes, &b"iVB"[..]);
        assert_eq!(found[1].bytes, &[0u8, 0, 0][..]);
        let out = splice(
            text,
            &found,
            &["lab-artifact:7".into(), "lab-artifact:8".into()],
        );
        assert_eq!(
            out,
            r#"{"created": 1, "data": [ {"b64_json": "lab-artifact:7" , "revised_prompt": "b64_json"},
            {"url": "https://img.example/x.png"}, {"b64_json":"lab-artifact:8"}, {"b64_json": "***"} ],
            "output_format": "png", "note": {"data": [{"b64_json": "QUJD"}]}}"#
        );
        assert!(pictures("not json").is_empty());
        assert!(pictures(r#"{"error": {"message": "x"}}"#).is_empty());
    }
}
