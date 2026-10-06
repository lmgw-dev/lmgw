//! `/v1/responses`' native passthrough on a fallback that cannot see
//! (`gate::fallback_images`, the owner's ruling of 2026-10-06: a configured
//! fallback is always used, with no exception by content).
//!
//! Every other send carries the IR, which `gate::fit_chat` hands over with
//! the images as placeholders. The native passthrough forwards the client's
//! own body, so the same decision
//! ([`crate::gate::fallback_images::blind_fallback`]) puts the same
//! placeholders into that body instead: each `input_image` part of a
//! message's `content`, and of a `function_call_output`'s array `output`,
//! becomes an `input_text` part saying what it was and why it is not there.
//! Nothing else in the body changes.
//!
//! The body is read for its images itself, not the IR: the IR keeps some of
//! a `function_call_output`'s images as text (one by URL as a resource, one
//! by `file_id` or in a format the ingress does not take as its JSON), and
//! those reach a native upstream as images all the same.

use serde_json::{json, Value};

use crate::config::Route;
use crate::gate::fallback_images::{blind_fallback, warn, Omitted, PLACEHOLDER_REASON};
use crate::ir::{image_note, image_placeholder, ContentPart};
use crate::state::SharedState;

/// `body` as the native upstream `route` goes to may take it: its images as
/// placeholders, with the WARN, when `route` is a fallback that cannot see;
/// untouched otherwise. `requested` is the name the client asked for. What
/// it left out: how many images (`x-lmgw-images-omitted`), and the row's
/// marker.
pub(super) async fn fit(
    state: &SharedState,
    route: &Route,
    requested: &str,
    body: &mut Value,
) -> Option<Omitted> {
    if !carries_images(body) {
        return None;
    }
    let unseen = blind_fallback(state, route, requested).await?;
    let dropped = without_images(body);
    warn(&unseen, &dropped);
    Some(Omitted {
        count: dropped.len(),
        marker: unseen.marker(dropped.len()),
    })
}

/// Whether `part` is an `input_image` part.
fn is_image(part: &Value) -> bool {
    part.get("type").and_then(Value::as_str) == Some("input_image")
}

/// The part arrays of `body`'s `input` that can hold an image: each item's
/// `content` (a message) and `output` (a `function_call_output`), when an
/// array.
const PART_ARRAYS: [&str; 2] = ["content", "output"];

/// Whether `body` carries an `input_image` part anywhere [`without_images`]
/// looks.
fn carries_images(body: &Value) -> bool {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return false;
    };
    items.iter().any(|item| {
        PART_ARRAYS.iter().any(|key| {
            item.get(*key)
                .and_then(Value::as_array)
                .is_some_and(|parts| parts.iter().any(is_image))
        })
    })
}

/// Replace every `input_image` part of `body`'s `input` with its
/// placeholder; what each stood for, in request order.
fn without_images(body: &mut Value) -> Vec<String> {
    let mut dropped = Vec::new();
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return dropped;
    };
    for item in items {
        for key in PART_ARRAYS {
            let Some(parts) = item.get_mut(key).and_then(Value::as_array_mut) else {
                continue;
            };
            for part in parts.iter_mut().filter(|p| is_image(p)) {
                let (note, text) = placeholder(part, PLACEHOLDER_REASON);
                dropped.push(note);
                *part = json!({"type": "input_text", "text": text});
            }
        }
    }
    dropped
}

/// What an `input_image` part is, and the text it becomes: the IR's own
/// placeholder for its URL or `data:` URI (as the ingress reads it), or one
/// naming its `file_id`.
fn placeholder(part: &Value, why: &str) -> (String, String) {
    let url = part
        .get("image_url")
        .and_then(Value::as_str)
        .or_else(|| part.pointer("/image_url/url").and_then(Value::as_str))
        .filter(|u| !u.is_empty());
    if let Some(url) = url {
        if let ContentPart::Image { mime, source } = crate::ingress::openai::parse_image_url(url) {
            return (
                image_note(&mime, &source),
                image_placeholder(&mime, &source, why),
            );
        }
    }
    let note = "image by file_id".to_string();
    let text = format!("[{note} — omitted: {why}]");
    (note, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_input_image_becomes_its_placeholder_and_nothing_else_changes() {
        let mut body = json!({
            "model": "local-vl",
            "input": [
                {"role": "user", "content": [
                    {"type": "input_text", "text": "what is this"},
                    {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo="},
                ]},
                {"type": "function_call_output", "call_id": "c1", "output": [
                    {"type": "input_image", "file_id": "file_1"},
                ]},
                {"type": "function_call_output", "call_id": "c2", "output": "plain"},
            ],
            "reasoning": {"effort": "low"},
        });
        assert!(carries_images(&body));
        let dropped = without_images(&mut body);
        let why = PLACEHOLDER_REASON;
        assert_eq!(
            body["input"][0]["content"][1],
            json!({"type": "input_text", "text": format!(
                "[image/png image, 12 base64 bytes — omitted: {why}]"
            )})
        );
        assert_eq!(body["input"][0]["content"][0]["text"], "what is this");
        assert_eq!(
            body["input"][1]["output"][0],
            json!({"type": "input_text", "text": format!(
                "[image by file_id — omitted: {why}]"
            )})
        );
        assert_eq!(body["input"][2]["output"], "plain");
        assert_eq!(body["reasoning"], json!({"effort": "low"}));
        assert_eq!(
            dropped,
            vec![
                "image/png image, 12 base64 bytes".to_string(),
                "image by file_id".to_string()
            ]
        );
        assert!(!carries_images(&body));
    }

    /// The images the IR keeps as text (review I3): a tool output's image by
    /// URL, by `file_id`, in a format the ingress does not read. Each is
    /// found in the body, and its placeholder leaves the URL out.
    #[test]
    fn a_tool_outputs_images_the_ir_keeps_as_text_are_found_too() {
        let mut body = json!({"model": "m", "input": [
            {"type": "function_call_output", "call_id": "c1", "output": [
                {"type": "input_image", "image_url": "https://example.com/a.png?sig=s"},
                {"type": "input_image", "image_url": "data:image/svg+xml;base64,PHN2Zy8+"},
            ]},
        ]});
        assert!(carries_images(&body));
        let dropped = without_images(&mut body);
        assert_eq!(
            dropped,
            vec![
                "image/png image by URL".to_string(),
                "image/svg+xml image, 8 base64 bytes".to_string()
            ]
        );
        assert!(!body.to_string().contains("example.com"), "{body}");
    }

    #[test]
    fn a_string_input_has_nothing_to_replace() {
        let mut body = json!({"model": "m", "input": "hello"});
        assert!(!carries_images(&body));
        assert!(without_images(&mut body).is_empty());
        assert_eq!(body, json!({"model": "m", "input": "hello"}));
    }
}
