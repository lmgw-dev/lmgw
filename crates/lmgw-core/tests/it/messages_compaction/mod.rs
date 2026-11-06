//! Anthropic's compaction through `/v1/messages` (gateway design §6.1): a
//! `compaction` block goes between a Messages client and an Anthropic
//! upstream as Anthropic wrote it, whole and streamed, with its
//! `stop_reason` and `usage.iterations`; the requests that ask for one (a
//! `compact_*` edit, the top-level `compaction`) reach the upstream as
//! written; every other upstream is sent the conversation as Anthropic makes
//! of it (`ir::compaction::folded`), and counted so.

mod anthropic;
mod folded;
mod foreign;

use serde_json::{json, Value};

use crate::common::Gw;

/// On-demand compaction's beta flag.
const ON_DEMAND: &str = "compact-2026-09-04";
/// Threshold compaction's beta flag.
const THRESHOLD: &str = "compact-2026-01-12";

/// A signed block, as an on-demand compaction answers.
fn block() -> Value {
    json!({
        "type": "compaction",
        "content": "Summary: the user designs a recipe app; entities Recipe, Ingredient, Step.",
        "signature": "EuYBCkQYsig==",
    })
}

async fn post_with(gw: &Gw, path: &str, beta: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}{path}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", beta)
        .json(body)
        .send()
        .await
        .unwrap()
}

/// An Anthropic SSE body of `events`, each under its own `type`.
fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// The `data:` objects of a client stream.
fn events(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .collect()
}
