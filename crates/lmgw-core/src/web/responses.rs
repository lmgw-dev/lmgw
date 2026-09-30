//! Presentation helpers for the **Traffic · Responses** surface (§21 stage 2)
//! — the stored `/v1/responses` conversation manager.
//!
//! `previous_response_id` makes the API stateful, and state that nobody can see
//! is state that grows until something breaks. These are the small pure pieces
//! that make it legible: a size, a one-line outline of what a response
//! contains, the tool calls waiting on approval, and the eviction rules stated
//! as a sentence rather than two numbers to combine. The reads themselves live
//! in [`super::api`]; automatic eviction is [`crate::store::gc_responses`].

use serde_json::Value;

pub(crate) fn human_bytes(n: i64) -> String {
    const KIB: f64 = 1024.0;
    let n = n.max(0) as f64;
    if n < KIB {
        format!("{n:.0} B")
    } else if n < KIB * KIB {
        format!("{:.1} KiB", n / KIB)
    } else {
        format!("{:.1} MiB", n / (KIB * KIB))
    }
}

/// `2 mcp_call, message` — the shape of a response at a glance.
pub(crate) fn outline(body: &Value) -> String {
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

pub(crate) fn pending_names(raw: Option<&str>) -> Vec<String> {
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
