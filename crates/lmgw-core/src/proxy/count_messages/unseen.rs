//! `/v1/messages/count_tokens` on an Anthropic route that is a fallback
//! which cannot see (`gate::fallback_images`, the owner's ruling of
//! 2026-10-06): the count of what the send carries.
//!
//! Every other route counts the IR as the send carries it
//! (`fallback_images::counted`). An Anthropic route counts the client's own
//! body on the provider (server tools and all), so the same placeholders go
//! into that body first: each `image` block of a message's `content`, and of
//! a `tool_result`'s `content`, becomes a `text` block with the placeholder
//! the send would carry. Nothing else in the body changes.

use serde_json::{json, Value};

use crate::config::Route;
use crate::gate::fallback_images::{blind_fallback, PLACEHOLDER_REASON};
use crate::ir::{image_placeholder, tool_image_placeholder, ContentPart};
use crate::state::SharedState;

/// `body` as the send on `route` would carry it: its images as placeholders
/// when `route` is a fallback that cannot see (`requested` is the name the
/// client asked for); untouched otherwise.
pub(super) async fn counted(state: &SharedState, route: &Route, requested: &str, body: &mut Value) {
    if image_blocks(body) == 0 {
        return;
    }
    if blind_fallback(state, route, requested).await.is_some() {
        without_images(body);
    }
}

/// Whether `block` is an `image` block.
fn is_image(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("image")
}

/// How many image blocks [`without_images`] would replace.
fn image_blocks(body: &Value) -> usize {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return 0;
    };
    let blocks = |v: Option<&Value>| v.and_then(Value::as_array).cloned().unwrap_or_default();
    messages
        .iter()
        .flat_map(|m| blocks(m.get("content")))
        .map(|b| {
            if is_image(&b) {
                1
            } else {
                blocks(b.get("content"))
                    .iter()
                    .filter(|t| is_image(t))
                    .count()
            }
        })
        .sum()
}

/// Replace every image block of `body`'s messages, a `tool_result`'s
/// included, with a text block holding its placeholder.
fn without_images(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for m in messages {
        let Some(blocks) = m.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for b in blocks {
            if is_image(b) {
                *b = text_block(b);
                continue;
            }
            if let Some(inner) = b.get_mut("content").and_then(Value::as_array_mut) {
                for t in inner.iter_mut().filter(|t| is_image(t)) {
                    *t = text_block(t);
                }
            }
        }
    }
}

/// The text block an image block becomes: the IR's placeholder for its
/// source, as the ingress reads it.
fn text_block(image: &Value) -> Value {
    let src = image.get("source");
    let field = |k: &str| src.and_then(|s| s.get(k)).and_then(Value::as_str);
    let text = match field("type") {
        Some("url") => match crate::ingress::openai::parse_image_url(field("url").unwrap_or("")) {
            ContentPart::Image { mime, source } => {
                image_placeholder(&mime, &source, PLACEHOLDER_REASON)
            }
            _ => format!("[image — omitted: {PLACEHOLDER_REASON}]"),
        },
        Some("base64") | None => tool_image_placeholder(
            field("media_type").unwrap_or("image/png"),
            field("data").unwrap_or_default(),
            PLACEHOLDER_REASON,
        ),
        Some(_) => format!("[image — omitted: {PLACEHOLDER_REASON}]"),
    };
    json!({"type": "text", "text": text})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_image_block_becomes_the_placeholder_the_send_carries() {
        let mut body = json!({"model": "m", "max_tokens": 9, "messages": [
            {"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                    "data": "iVBORw0KGgo="}},
                {"type": "text", "text": "what is this"},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "the screen:"},
                    {"type": "image", "source": {"type": "url",
                        "url": "https://example.com/s.png?sig=x"}},
                ]},
            ]},
        ]});
        assert_eq!(image_blocks(&body), 2);
        without_images(&mut body);
        assert_eq!(image_blocks(&body), 0);
        assert_eq!(
            body["messages"][0]["content"][0],
            json!({"type": "text", "text":
                "[image/png image, 12 base64 bytes — omitted: the answering model cannot see \
                 images]"})
        );
        assert_eq!(body["messages"][0]["content"][1]["text"], "what is this");
        assert_eq!(
            body["messages"][1]["content"][0]["content"][1],
            json!({"type": "text", "text":
                "[image/png image by URL — omitted: the answering model cannot see images]"})
        );
        assert_eq!(body["max_tokens"], 9);
    }
}
