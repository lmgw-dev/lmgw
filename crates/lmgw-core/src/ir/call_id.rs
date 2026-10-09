//! A Gemini `thoughtSignature` carried inside the tool call's id (gateway
//! design §7.1).
//!
//! Gemini 3 refuses (400) a request whose current turn replays one of its
//! own `functionCall` parts without the signature it came with, so the
//! signature has to survive the trip through the client: out in lmgw's
//! answer, back in the client's next request. The tool call's id is the one
//! value every client shape echoes verbatim — OpenAI chat `tool_calls[].id`
//! and `tool_call_id`, Anthropic `tool_use.id` and `tool_use_id`, Responses
//! and Realtime `call_id` — and the one lmgw's own stored threads keep as
//! they are. It adds no field an OpenAI- or Anthropic-shaped answer does not
//! have (Google's own OpenAI compatibility layer puts the signature in a
//! non-standard `extra_content` that most clients drop).
//!
//! The id the Gemini egress mints for a signed call is
//! `<bare id><marker><crc><payload>`:
//!
//! - **marker** [`THOUGHT_SIGNATURE_MARKER`] when the signature is standard
//!   padded base64, as Google's are: the payload is its decoded bytes in
//!   unpadded URL-safe base64, a quarter shorter than encoding the text
//!   again. [`THOUGHT_SIGNATURE_TEXT_MARKER`] for any other signature: the
//!   payload is its UTF-8 bytes in unpadded URL-safe base64. Both are
//!   lossless and stay inside `[A-Za-z0-9_-]`, the alphabet Anthropic's API
//!   allows in an id.
//! - **crc**: the CRC-32 of the signature text, eight lowercase hex digits.
//!   A client that truncated or rewrote the id then reads as a call without
//!   a signature (the Gemini egress gives it the skip value) rather than as
//!   a corrupt signature Gemini would refuse.
//!
//! The Gemini egress puts the signature back on its `functionCall` part;
//! every other egress sends the bare id ([`wire_call_id`]), so an OpenAI,
//! Anthropic or llama.cpp upstream sees the id Gemini's call was minted with
//! and no signature reaches a prompt template.

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;

/// What separates a call's bare id from its signature, transcoded from
/// standard base64 (the usual case).
pub const THOUGHT_SIGNATURE_MARKER: &str = "__thoughtsig_";

/// The same for a signature that is not standard padded base64, carried as
/// its text.
pub const THOUGHT_SIGNATURE_TEXT_MARKER: &str = "__thoughtsigtxt_";

/// What both markers start with; the bare id ends before it.
const STEM: &str = "__thoughtsig";

/// Hex digits of the CRC-32 that follows the marker.
const CRC_LEN: usize = 8;

/// `id` with `signature` carried in it; `id` itself when the signature is
/// empty.
pub fn call_id_with_signature(id: &str, signature: &str) -> String {
    if signature.is_empty() {
        return id.to_string();
    }
    let crc = crc32fast::hash(signature.as_bytes());
    match STANDARD
        .decode(signature)
        .ok()
        .filter(|b| STANDARD.encode(b) == signature)
    {
        Some(bytes) => format!(
            "{id}{THOUGHT_SIGNATURE_MARKER}{crc:08x}{}",
            URL_SAFE_NO_PAD.encode(bytes)
        ),
        None => format!(
            "{id}{THOUGHT_SIGNATURE_TEXT_MARKER}{crc:08x}{}",
            URL_SAFE_NO_PAD.encode(signature.as_bytes())
        ),
    }
}

/// A call id split into its bare id and the signature it carries. An id
/// without a marker is all bare id. One with a marker is the bare id before
/// it, and its signature only when the tail decodes and matches its CRC: a
/// tail a client truncated or rewrote carries none, so the call is replayed
/// as unsigned, never with a corrupt signature.
pub fn split_call_id(id: &str) -> (&str, Option<String>) {
    let Some(at) = id.find(STEM) else {
        return (id, None);
    };
    let rest = &id[at + STEM.len()..];
    let (text, tail) = if let Some(t) = rest.strip_prefix(&THOUGHT_SIGNATURE_MARKER[STEM.len()..]) {
        (false, t)
    } else if let Some(t) = rest.strip_prefix(&THOUGHT_SIGNATURE_TEXT_MARKER[STEM.len()..]) {
        (true, t)
    } else {
        // `__thoughtsig` followed by something else is no marker.
        return (id, None);
    };
    (&id[..at], decode_tail(tail, text))
}

fn decode_tail(tail: &str, text: bool) -> Option<String> {
    let (crc, payload) = (tail.get(..CRC_LEN)?, tail.get(CRC_LEN..)?);
    if !crc.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let crc = u32::from_str_radix(crc, 16).ok()?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let sig = if text {
        String::from_utf8(bytes).ok()?
    } else {
        STANDARD.encode(bytes)
    };
    (!sig.is_empty() && crc32fast::hash(sig.as_bytes()) == crc).then_some(sig)
}

/// The id an upstream other than Gemini is sent: the bare id.
pub fn wire_call_id(id: &str) -> &str {
    split_call_id(id).0
}

/// A client's Responses body as it goes on to an upstream that implements
/// `/v1/responses` itself (the native passthrough): every input item's
/// `call_id` bare (`function_call`, `function_call_output` and any other
/// item that has one), so a Gemini signature from an earlier step stays
/// with Gemini.
pub fn wire_call_ids_in_responses_body(body: &mut serde_json::Value) {
    let Some(items) = body.get_mut("input").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for item in items {
        bare_in_place(item.get_mut("call_id"));
    }
}

/// A client's Anthropic Messages body as it goes on to an Anthropic upstream
/// verbatim (`/v1/messages/count_tokens`): every `tool_use` block's `id` and
/// every block's `tool_use_id` bare. A signature would otherwise reach the
/// provider and be counted as part of the prompt.
pub fn wire_call_ids_in_messages_body(body: &mut serde_json::Value) {
    let Some(messages) = body.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for m in messages {
        let Some(blocks) = m.get_mut("content").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                bare_in_place(b.get_mut("id"));
            }
            bare_in_place(b.get_mut("tool_use_id"));
        }
    }
}

fn bare_in_place(v: Option<&mut serde_json::Value>) {
    if let Some(serde_json::Value::String(id)) = v {
        let bare = wire_call_id(id);
        if bare.len() != id.len() {
            *id = bare.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_safe(id: &str) -> bool {
        id.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }

    #[test]
    fn a_base64_signature_is_transcoded() {
        for sig in ["Eq0BCqoBAXLI2n+/sig/A+A=", "EpoGCpcGAXLI2nx/+abc", "QQ=="] {
            let id = call_id_with_signature("call_0", sig);
            assert!(id_safe(&id), "{id}");
            assert!(id.contains(THOUGHT_SIGNATURE_MARKER), "{id}");
            assert!(!id.contains(THOUGHT_SIGNATURE_TEXT_MARKER), "{id}");
            assert_eq!(split_call_id(&id), ("call_0", Some(sig.to_string())));
            assert_eq!(wire_call_id(&id), "call_0");
        }
    }

    #[test]
    fn transcoding_is_a_quarter_shorter_than_encoding_the_text() {
        // 600 bytes: 800 base64 characters, unpadded either way.
        let sig = STANDARD.encode([7u8; 600]);
        let payload = |id: &str, marker: &str| id.len() - "c".len() - marker.len() - CRC_LEN;
        let transcoded = payload(&call_id_with_signature("c", &sig), THOUGHT_SIGNATURE_MARKER);
        let as_text = URL_SAFE_NO_PAD.encode(sig.as_bytes()).len();
        assert_eq!(transcoded, sig.len());
        assert_eq!((transcoded, as_text), (800, 1067));
    }

    #[test]
    fn any_other_signature_rides_as_its_text() {
        // Unpadded, non-canonical padding, not base64 at all.
        for sig in [
            "x",
            "QQ",
            "QR==",
            "not base64 at all: ü/+=",
            "Eq0BCqoBAXLI2n+/sig/A+B==",
        ] {
            let id = call_id_with_signature("call_0", sig);
            assert!(id_safe(&id), "{id}");
            assert!(id.contains(THOUGHT_SIGNATURE_TEXT_MARKER), "{id}");
            assert_eq!(split_call_id(&id), ("call_0", Some(sig.to_string())));
            assert_eq!(wire_call_id(&id), "call_0");
        }
    }

    #[test]
    fn an_id_without_a_signature_is_its_own_bare_id() {
        assert_eq!(call_id_with_signature("call_1", ""), "call_1");
        for id in ["call_1", "toolu_01AbC", "lmgw_task_7", "", "a__thoughtsigx"] {
            assert_eq!(split_call_id(id), (id, None));
            assert_eq!(wire_call_id(id), id);
        }
    }

    #[test]
    fn a_damaged_tail_is_a_call_without_a_signature() {
        let good = call_id_with_signature("call_0", &STANDARD.encode([42u8; 300]));
        let text = call_id_with_signature("call_0", "plain text signature");
        let mut damaged: Vec<String> = Vec::new();
        for id in [&good, &text] {
            // Every truncation, down to the bare marker.
            let marker_end = id.find(STEM).unwrap();
            for cut in marker_end..id.len() {
                damaged.push(id[..cut].to_string());
            }
            // One character changed in the CRC and in the payload.
            let marker = if id.contains(THOUGHT_SIGNATURE_TEXT_MARKER) {
                THOUGHT_SIGNATURE_TEXT_MARKER
            } else {
                THOUGHT_SIGNATURE_MARKER
            };
            let crc_at = marker_end + marker.len();
            for at in [crc_at, crc_at + CRC_LEN + 3, id.len() - 1] {
                let mut s = id.clone().into_bytes();
                s[at] = if s[at] == b'A' { b'B' } else { b'A' };
                damaged.push(String::from_utf8(s).unwrap());
            }
        }
        damaged.extend(
            [
                "call_0__thoughtsig_",
                "call_0__thoughtsig_!!",
                "call_0__thoughtsig_A",
                "call_0__thoughtsig_zzzzzzzzQUFB",
                "call_0__thoughtsigtxt_00000000QUFB",
            ]
            .map(String::from),
        );
        for id in &damaged {
            if id.contains(THOUGHT_SIGNATURE_MARKER) || id.contains(THOUGHT_SIGNATURE_TEXT_MARKER) {
                assert_eq!(split_call_id(id), ("call_0", None), "{id}");
            } else {
                // Cut inside the marker itself: no marker left.
                assert_eq!(split_call_id(id).1, None, "{id}");
            }
        }
    }

    #[test]
    fn raw_bodies_lose_their_signatures() {
        let signed = call_id_with_signature("call_7", "QUJD");
        let mut responses = serde_json::json!({"input": [
            {"role": "user", "content": "hi"},
            {"type": "function_call", "id": "fc_1", "call_id": signed, "name": "f",
             "arguments": "{}"},
            {"type": "function_call_output", "call_id": signed, "output": "ok"},
            {"type": "function_call_output", "call_id": "toolu_x", "output": "ok"},
        ]});
        wire_call_ids_in_responses_body(&mut responses);
        let ids: Vec<_> = responses["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i["call_id"].as_str())
            .collect();
        assert_eq!(ids, ["call_7", "call_7", "toolu_x"]);
        assert_eq!(responses["input"][1]["id"], "fc_1");

        let mut messages = serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "x"},
                {"type": "tool_use", "id": signed, "name": "f", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": signed, "content": "ok"}]},
        ]});
        wire_call_ids_in_messages_body(&mut messages);
        assert_eq!(messages["messages"][1]["content"][1]["id"], "call_7");
        assert_eq!(
            messages["messages"][2]["content"][0]["tool_use_id"],
            "call_7"
        );
        // Bodies of another shape are left alone.
        let mut other = serde_json::json!({"input": "text", "messages": "x"});
        wire_call_ids_in_responses_body(&mut other);
        wire_call_ids_in_messages_body(&mut other);
        assert_eq!(other, serde_json::json!({"input": "text", "messages": "x"}));
    }
}
