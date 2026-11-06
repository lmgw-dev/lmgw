//! A replayed reasoning item goes only where OpenAI takes it (WP8 review
//! S-R1): OpenAI refuses an input `reasoning` item with no item of the
//! assistant's after it ("Item 'rs_…' of type 'reasoning' was provided
//! without its required following item."). A turn can hold one: a reply
//! stopped or cut after its reasoning, one whose `max_output_tokens` went to
//! reasoning alone, a `/v1/messages` client's thinking block with nothing
//! after it. Sent as it is, every later turn of that conversation on the row
//! would fail until the turn was edited.
//!
//! So a sealed item is sent only when a message (a text, a refusal) or a
//! `function_call` of the same assistant turn follows it: the run of
//! assistant items before the next user, developer or tool-result item. A
//! turn, not one IR message: the Chat replays a tool turn's record and its
//! final answer as two assistant messages, the record's last reasoning item
//! ahead of the answer. Otherwise the item is left out of the body, a
//! translation like an unsealed part's: its text stays in the stored thread
//! (the Chat's reply, the client's own transcript), and only the platform's
//! view of that turn lacks the encrypted reasoning, as on a row that never
//! sealed it. Whether OpenAI takes `[reasoning, reasoning, message]` (two
//! items, then the one that follows both) is the WP8 probe's to show; it is
//! sent.

use serde_json::Value;

use crate::egress::openai_wire::responses_items::item;

/// What an input item is to the rule (module doc).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Reasoning,
    /// An assistant message or a `function_call`: what follows a reasoning
    /// item.
    Answer,
    /// Anything that ends the assistant's turn.
    Other,
}

fn kind(v: &Value) -> Kind {
    match v.get("type").and_then(Value::as_str) {
        Some(item::REASONING) => Kind::Reasoning,
        Some(item::FUNCTION_CALL) => Kind::Answer,
        _ if v.get("role").and_then(Value::as_str) == Some("assistant") => Kind::Answer,
        _ => Kind::Other,
    }
}

/// `input` without the reasoning items no answer of their turn follows.
pub(super) fn keep_followed(input: &mut Vec<Value>) {
    let mut answered = false;
    let mut keep = vec![true; input.len()];
    for (i, v) in input.iter().enumerate().rev() {
        match kind(v) {
            Kind::Answer => answered = true,
            Kind::Other => answered = false,
            Kind::Reasoning if !answered => {
                keep[i] = false;
                let id = v.get("id").and_then(Value::as_str).unwrap_or_default();
                tracing::debug!(
                    item = id,
                    "a replayed reasoning item no answer of its turn follows is left out of \
                     the Responses body: OpenAI refuses it, and its text stays in the thread"
                );
            }
            Kind::Reasoning => {}
        }
    }
    let mut keep = keep.into_iter();
    input.retain(|_| keep.next().unwrap_or(true));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rs(id: &str) -> Value {
        json!({"type": "reasoning", "id": id, "summary": [], "encrypted_content": "e"})
    }

    fn ids(input: &[Value]) -> Vec<String> {
        input
            .iter()
            .map(|v| {
                v.get("id")
                    .or_else(|| v.get("call_id"))
                    .or_else(|| v.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_reasoning_item_goes_only_with_an_answer_of_its_turn_after_it() {
        let user = |t: &str| json!({"type": "message", "role": "user", "content": t});
        let said = |t: &str| json!({"role": "assistant", "content": t});
        let call = json!({"type": "function_call", "call_id": "c1", "name": "f",
                          "arguments": "{}"});
        let result = json!({"type": "function_call_output", "call_id": "r1", "output": "x"});
        let mut input = vec![
            user("u1"),
            rs("rs_partial"),
            user("u2"),
            rs("rs_call"),
            call,
            result,
            rs("rs_a"),
            rs("rs_b"),
            said("answer"),
            rs("rs_trailing"),
        ];
        keep_followed(&mut input);
        assert_eq!(
            ids(&input),
            ["u1", "u2", "rs_call", "c1", "r1", "rs_a", "rs_b", "answer"]
        );
        // A tool turn's record and its answer are two messages, one turn.
        let mut two = vec![user("u"), rs("rs_last"), said("answer")];
        keep_followed(&mut two);
        assert_eq!(ids(&two), ["u", "rs_last", "answer"]);
    }
}
