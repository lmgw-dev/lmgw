//! Presentation helpers for the **Traffic · Responses** surface (§21 stage 2)
//! — the stored `/v1/responses` conversation manager.
//!
//! `previous_response_id` makes the API stateful, and state that nobody can see
//! is state that grows until something breaks. These are the small pure pieces
//! that make it legible: a size, a one-line outline of what a response
//! contains, the tool calls waiting on approval, and the eviction rules stated
//! as a sentence rather than two numbers to combine. The reads themselves live
//! in [`super::api`], which builds the document's rows with the two `From`
//! impls below; automatic eviction is [`crate::store::gc_responses`].

use lmgw_api_types as dto;
use lmgw_api_types::units::fmt_bytes;
use serde_json::Value;

use crate::store::{ResponseChain, StoredResponse};

/// A conversation as the manager tab's list shows it.
impl From<&ResponseChain> for dto::ChainRow {
    fn from(c: &ResponseChain) -> Self {
        Self {
            chain_id: c.chain_id.clone(),
            head_id: c.head_id.clone(),
            model: c.model.clone(),
            status: c.status.clone(),
            responses: c.responses,
            first_at: c.first_at.clone(),
            last_at: c.last_at.clone(),
            input_tokens: c.input_tokens,
            output_tokens: c.output_tokens,
            size: fmt_bytes(c.bytes.max(0) as u64),
            awaiting_approval: c.awaiting_approval,
        }
    }
}

/// One stored response as the conversation view shows it: its body is read
/// for the outline and the text, and kept pretty-printed (a body that is not
/// JSON reads as `null`), each reasoning item's encrypted content shown as
/// its size only ([`sealed_shown`]).
impl From<&StoredResponse> for dto::ResponseItem {
    fn from(r: &StoredResponse) -> Self {
        let mut body: Value = serde_json::from_str(&r.body).unwrap_or(Value::Null);
        sealed_shown(&mut body);
        Self {
            id: r.id.clone(),
            created_at: r.created_at.clone(),
            status: r.status.clone(),
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            outline: outline(&body),
            text: crate::ingress::responses::output_text(&body),
            pending: pending_names(r.pending.as_deref()),
            body: serde_json::to_string_pretty(&body).unwrap_or_default(),
        }
    }
}

/// Every reasoning item's `encrypted_content` in `body` as a placeholder
/// that says what it is and how big (`<sealed reasoning, 1.2 KiB>`): the
/// encrypted reasoning is the upstream's to read, never a page's or a
/// device's (WP8 verify V-F3, as the Chat's surfaces show no seal), and in a
/// loop answer it is the whole carrier, its row and item named. The store
/// keeps it for the chain's next turn.
fn sealed_shown(v: &mut Value) {
    match v {
        Value::Object(o) => {
            if o.get("type").and_then(Value::as_str) == Some("reasoning") {
                if let Some(Value::String(data)) = o.get_mut("encrypted_content") {
                    if !data.is_empty() {
                        let size = fmt_bytes(data.len() as u64);
                        *data = format!("<sealed reasoning, {size}>");
                    }
                }
            }
            o.values_mut().for_each(sealed_shown);
        }
        Value::Array(a) => a.iter_mut().for_each(sealed_shown),
        _ => {}
    }
}

/// `2 mcp_call, message` — the shape of a response at a glance.
fn outline(body: &Value) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for item in body["output"].as_array().into_iter().flatten() {
        let ty = item["type"].as_str().unwrap_or("?").to_string();
        match counts.iter_mut().find(|(t, _)| *t == ty) {
            Some((_, n)) => *n += 1,
            None => counts.push((ty, 1)),
        }
    }
    counts
        .into_iter()
        .map(|(t, n)| if n == 1 { t } else { format!("{n} {t}") })
        .collect::<Vec<_>>()
        .join(", ")
}

fn pending_names(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else { return Vec::new() };
    serde_json::from_str::<Vec<crate::agent::PendingCall>>(raw)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.needs_approval)
        .map(|p| format!("{} ({})", p.name, p.approval_id))
        .collect()
}

/// The eviction rules in one sentence, so the UI states what will happen
/// rather than leaving two numbers for the reader to combine.
pub(crate) fn describe_rules(retention_hours: i64, max_chains: i64) -> String {
    let age = match retention_hours {
        0 => "kept indefinitely".to_string(),
        1 => "evicted after 1 idle hour".to_string(),
        h if h % 24 == 0 => format!("evicted after {} idle day(s)", h / 24),
        h => format!("evicted after {h} idle hours"),
    };
    let count = match max_chains {
        0 => "with no cap on how many are kept".to_string(),
        n => format!("keeping at most {n}"),
    };
    format!("{age}, {count}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_reasoning_item_s_encrypted_content_shows_as_its_size() {
        let mut body = json!({"output": [
            {"type": "reasoning", "id": "rs_1", "summary": [],
             "encrypted_content": "lmgw-openai:4:cfg:rs_up:gAAAAB-sealed"},
            {"type": "reasoning", "id": "rs_2", "summary": [], "encrypted_content": ""},
            {"type": "message", "encrypted_content": "not reasoning"},
        ], "input": [{"type": "reasoning", "encrypted_content": "x".repeat(2048)}]});
        sealed_shown(&mut body);
        assert_eq!(
            body["output"][0]["encrypted_content"],
            "<sealed reasoning, 37 B>"
        );
        assert_eq!(body["output"][1]["encrypted_content"], "");
        assert_eq!(body["output"][2]["encrypted_content"], "not reasoning");
        assert_eq!(
            body["input"][0]["encrypted_content"],
            "<sealed reasoning, 2.0 KiB>"
        );
    }
}
