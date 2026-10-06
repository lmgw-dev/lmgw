//! A `function_call_output`'s `output` → IR tool-result blocks (llama-egress
//! design §8.3).
//!
//! `output` is a string, or an array of `input_text` and `input_image` items
//! — the shape a client uses to hand a tool's screenshot back. That array used
//! to become its own JSON text, so an image's base64 reached the prompt as
//! characters. It is now read item by item, as llama-server's own Responses
//! converter reads it (`server-chat.cpp:204-229` at 0c6a6a7), into the blocks
//! the Anthropic ingress and MCP results already produce. Each egress then
//! renders what its protocol takes: an Anthropic upstream gets the image
//! itself, and a text-only tool slot gets the named placeholder instead of
//! the bytes.
//!
//! - `input_text` becomes a [`ToolResultBlock::Text`].
//! - An `input_image` with a base64 `data:` URL of a png, jpeg, gif or webp
//!   whose bytes are that format becomes a [`ToolResultBlock::Image`], its
//!   mime the format's own lowercase one (`image/jpg` and `image/PNG` say
//!   `image/jpeg` and `image/png`); a URL that names no type takes the
//!   format its bytes are. These four are what Anthropic's `media_type`
//!   takes, so an Anthropic upstream is never handed an image block it
//!   refuses with a 400 where the item used to go as text.
//! - An `input_image` with any other URL becomes a
//!   [`ToolResultBlock::Resource`] naming the URL. The IR's tool image carries
//!   bytes and lmgw fetches nothing for a client, so the model reads where the
//!   image is.
//! - Anything else stays as it was: its own JSON text, with a WARN naming it.
//!   That covers an unknown item type, an `input_text` without text, an image
//!   by `file_id`, a `data:` URL that is not base64 (SVG markup sent as
//!   `utf8`), and a base64 one that is not one of the four formats, says one
//!   format and is another, or does not decode (an SVG, a BMP). llama-server
//!   refuses such an item with a 400; lmgw keeps it, so a conversation that
//!   worked before still works.

use serde_json::Value;

use crate::ir::ToolResultBlock;
use crate::mcp::spec::kind_of;

/// The blocks of one `function_call_output`. `call_id` only names the item in
/// the WARN.
pub(super) fn output_blocks(output: Option<&Value>, call_id: &str) -> Vec<ToolResultBlock> {
    let items = match output {
        Some(Value::String(s)) => return ToolResultBlock::one(s.clone()),
        Some(Value::Array(items)) => items,
        None => return ToolResultBlock::one(""),
        // Not a shape the API defines; kept as its JSON text, as it always was.
        Some(other) => return ToolResultBlock::one(other.to_string()),
    };
    let mut kept_as_json: Vec<String> = Vec::new();
    let mut blocks: Vec<ToolResultBlock> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            item_block(item).unwrap_or_else(|| {
                kept_as_json.push(format!("#{i} {}", item_type(item)));
                ToolResultBlock::text(item.to_string())
            })
        })
        .collect();
    if !kept_as_json.is_empty() {
        tracing::warn!(
            call_id,
            "function_call_output items that are neither input_text nor a readable \
             input_image reach the model as their JSON text: {}",
            kept_as_json.join(", ")
        );
    }
    // An empty array is a result with nothing in it. One empty text block is
    // how MCP's empty result looks too, and every egress renders it.
    if blocks.is_empty() {
        blocks = ToolResultBlock::one("");
    }
    blocks
}

/// One item as a block, or `None` when it is kept as its JSON text.
fn item_block(item: &Value) -> Option<ToolResultBlock> {
    match item.get("type").and_then(Value::as_str)? {
        "input_text" => item
            .get("text")
            .and_then(Value::as_str)
            .map(ToolResultBlock::text),
        "input_image" => {
            // The Responses API spells it as a string; the `{url}` object is
            // chat-completions' spelling, which the message parser takes too.
            let url = item
                .get("image_url")
                .and_then(Value::as_str)
                .or_else(|| item.pointer("/image_url/url").and_then(Value::as_str))
                .filter(|u| !u.is_empty())?;
            image_block(url)
        }
        _ => None,
    }
}

/// An image URL as a block: a base64 `data:` URL of an image Anthropic takes
/// → the image ([`image_mime`]), any other `data:` URL → `None`, anything
/// else → a resource naming it.
fn image_block(url: &str) -> Option<ToolResultBlock> {
    let Some(rest) = url.strip_prefix("data:") else {
        return Some(url_image_resource(url));
    };
    let (meta, data) = rest.split_once(',')?;
    let mut params = meta.split(';');
    let mime = params.next().unwrap_or_default().trim();
    if !params.any(|p| p.trim().eq_ignore_ascii_case("base64")) {
        return None;
    }
    Some(ToolResultBlock::Image {
        mime: image_mime(mime, data)?.to_string(),
        data: data.to_string(),
    })
}

/// The mime a base64 image goes as: png, jpeg, gif or webp by its magic
/// numbers (`extract::sniff`, the chat upload's sniff, which knows exactly
/// these four), when its data decodes as standard base64 and its declared
/// type, case aside and with `image/jpg` for `image/jpeg`, names the same
/// format. RFC 2397 leaves the type out when it is the default: then the
/// bytes alone decide. `None` for anything else.
fn image_mime(declared: &str, data: &str) -> Option<&'static str> {
    use crate::extract::sniff::{sniff, Kind};
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    let found = sniff(&bytes[..bytes.len().min(16)])
        .ok()
        .filter(|s| s.kind == Kind::Image)?
        .mime;
    let declared = declared.to_ascii_lowercase();
    let declared = match declared.as_str() {
        "image/jpg" => "image/jpeg",
        other => other,
    };
    (declared.is_empty() || declared == found).then_some(found)
}

/// A tool image given by a URL, as the resource that names it: the IR's tool
/// image carries bytes and lmgw fetches nothing for a client, so the model
/// reads where the image is. Also the Anthropic ingress's, for a
/// `tool_result` image whose source is a URL.
pub(crate) fn url_image_resource(url: &str) -> ToolResultBlock {
    ToolResultBlock::Resource {
        uri: url.to_string(),
        mime: Some(guess_image_mime(url)),
        text: None,
    }
}

/// The type a URL's path names, when it names an image type. `image/*` says
/// "an image" without guessing which.
fn guess_image_mime(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    mime_guess::from_path(path)
        .first_raw()
        .filter(|m| m.starts_with("image/"))
        .unwrap_or("image/*")
        .to_string()
}

/// An item's `type` for the WARN, or the JSON kind of an item that has none.
fn item_type(item: &Value) -> String {
    match item.get("type").and_then(Value::as_str) {
        Some(t) => t.to_string(),
        None if item.is_object() => "untyped object".to_string(),
        None => kind_of(item).to_string(),
    }
}
