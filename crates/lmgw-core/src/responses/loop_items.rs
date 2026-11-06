//! The items lmgw's own loop made up, on the native path (WP8 review W-R1):
//! a `/v1/responses` loop answer's `message` items (`msg_…`) and
//! `function_call` items (`fc_…`) carry ids lmgw minted, which the platform
//! never stored and no answer recorded. A client that takes a loop
//! transcript to a request the platform serves natively (its next turn has
//! no tool only the loop runs) sends them back, and the item check would
//! refuse the unknown ids, or the platform would look them up.
//!
//! Such an item is whole: the client sends its content, not a reference. So
//! an assistant `message` or a `function_call` whose id the reader was never
//! given goes without the id, the shape lmgw's own Responses bodies send
//! (`egress/openai/responses/body.rs`): a `function_call` with its `id` left
//! out (optional in the create reference), a `message` as the
//! `EasyInputMessage` `{"role": "assistant", "content": "<its text>"}` (an
//! output `message` input item requires its `id`), its refusal parts as
//! their text and its `phase` kept. An item with an id the reader was given
//! is the platform's own and goes as it is. Nothing in it reaches another
//! key's data: its content is the client's own words.
//!
//! A `reasoning` item without encrypted content (neither OpenAI's nor
//! lmgw's carrier) is a summary the platform cannot use: under `store:
//! false` nothing is kept upstream to look its id up in (WP8 verify V-F1).
//! The loop writes one for a step whose encrypted reasoning never came (a
//! fallback answered it, or the platform sent none), under an id it minted.
//! One whose id the reader was never given is left out of the body, a
//! translation: its summary stays the client's, and the platform could not
//! have read it. One the reader was given is the platform's own (a stored
//! answer's) and goes as it is.
//!
//! The loop's `mcp_call`, `mcp_list_tools` and `mcp_approval_request` items
//! are not translated yet: the platform requires their `id`, and their
//! label names a server only lmgw serves. A transcript holding them stays
//! with the loop as long as it sends that server's `mcp` tool, and on the
//! native path a client key's is refused as an unknown item, as before.

use std::collections::HashSet;

use serde_json::{json, Value};

/// Whether `item` is whole, so that its id may go (module doc).
pub(super) fn is_whole(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => true,
        Some("message") => item.get("role").and_then(Value::as_str) == Some("assistant"),
        _ => false,
    }
}

/// Whether `item` is a `reasoning` item without encrypted content: a
/// summary alone, which the platform cannot use unless it stored the item
/// itself (module doc).
fn is_unsealed_reasoning(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("reasoning")
        && item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
}

/// Whether what becomes of `item` on the native path is decided by whether
/// the reader was given its id ([`forward_as_given`]) rather than by the
/// item check: a whole item, or a reasoning item without encrypted content.
pub(super) fn goes_as_given(item: &Value) -> bool {
    is_whole(item) || is_unsealed_reasoning(item)
}

/// The ids of the items `body`'s input names that [`goes_as_given`].
pub(super) fn given_candidates(body: &Value) -> Vec<String> {
    input(body)
        .filter(|i| goes_as_given(i))
        .filter_map(|i| i.get("id").and_then(Value::as_str))
        .map(String::from)
        .collect()
}

fn input(body: &Value) -> impl Iterator<Item = &Value> {
    body.get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// `body`'s whole items whose id is not in `given` (the reader's recorded
/// items), without it, and its reasoning items without encrypted content
/// whose id is not in `given` left out (module doc).
pub(super) fn forward_as_given(body: &mut Value, given: &HashSet<String>) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let foreign = |item: &Value| {
        item.get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !given.contains(id))
    };
    items.retain(|item| !(is_unsealed_reasoning(item) && foreign(item)));
    for item in items.iter_mut() {
        if !(foreign(item) && is_whole(item)) {
            continue;
        }
        if item.get("type").and_then(Value::as_str) == Some("function_call") {
            if let Some(o) = item.as_object_mut() {
                o.remove("id");
            }
            continue;
        }
        *item = easy_message(item);
    }
}

/// An output `message` item as the `EasyInputMessage` that says the same.
fn easy_message(item: &Value) -> Value {
    let text = match item.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("output_text") | Some("input_text") => p.get("text"),
                Some("refusal") => p.get("refusal"),
                _ => None,
            })
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    let mut out = json!({"role": "assistant", "content": text});
    if let Some(phase) = item.get("phase").filter(|p| !p.is_null()) {
        out["phase"] = phase.clone();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_item_the_reader_was_not_given_goes_without_its_id() {
        let mut body = json!({"input": [
            {"role": "user", "content": "hi"},
            {"type": "reasoning", "id": "rs_raw", "summary": [], "encrypted_content": "gAAA"},
            {"type": "function_call", "id": "fc_loop", "call_id": "c1", "name": "f",
             "arguments": "{}", "status": "completed"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"},
            {"type": "message", "id": "msg_loop", "role": "assistant", "status": "completed",
             "phase": "final_answer", "content": [
                {"type": "output_text", "text": "Hello", "annotations": []},
                {"type": "refusal", "refusal": "not that"}]},
            {"type": "message", "id": "msg_own", "role": "assistant", "status": "completed",
             "content": [{"type": "output_text", "text": "Mine", "annotations": []}]},
        ]});
        assert_eq!(given_candidates(&body), ["fc_loop", "msg_loop", "msg_own"]);
        let given: HashSet<String> = ["msg_own".to_string()].into();
        let before = body.clone();
        forward_as_given(&mut body, &given);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0], before["input"][0]);
        assert_eq!(input[1], before["input"][1], "a reference is the check's");
        assert_eq!(
            input[2],
            json!({"type": "function_call", "call_id": "c1", "name": "f",
                   "arguments": "{}", "status": "completed"})
        );
        assert_eq!(input[3], before["input"][3]);
        assert_eq!(
            input[4],
            json!({"role": "assistant", "content": "Hello\nnot that",
                   "phase": "final_answer"})
        );
        assert_eq!(
            input[5], before["input"][5],
            "the platform's own goes as it is"
        );
    }

    /// A reasoning item without encrypted content goes only when the reader
    /// was given its id; one with encrypted content (OpenAI's, or lmgw's
    /// carrier) is the item check's and the seals' (WP8 verify V-F1).
    #[test]
    fn an_unsealed_reasoning_item_goes_only_when_it_was_given() {
        let mut body = json!({"input": [
            {"role": "user", "content": "hi"},
            {"type": "reasoning", "id": "rs_loop", "summary": [], "encrypted_content": null},
            {"type": "reasoning", "id": "rs_empty", "summary": [], "encrypted_content": ""},
            {"type": "reasoning", "id": "rs_stored", "summary": []},
            {"type": "reasoning", "id": "rs_raw", "summary": [], "encrypted_content": "gAAA"},
            {"type": "reasoning", "id": "rs_ours", "summary": [],
             "encrypted_content": "lmgw-openai:4:cfg:rs_up:enc"},
        ]});
        assert_eq!(
            given_candidates(&body),
            ["rs_loop", "rs_empty", "rs_stored"]
        );
        let given: HashSet<String> = ["rs_stored".to_string()].into();
        let before = body.clone();
        forward_as_given(&mut body, &given);
        let input = &before["input"];
        assert_eq!(
            body["input"],
            json!([input[0], input[3], input[4], input[5]])
        );
    }
}
