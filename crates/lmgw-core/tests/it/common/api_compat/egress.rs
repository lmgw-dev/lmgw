//! (a): every request body an egress builds, against its provider's schema.
//!
//! Installed as lmgw-core's `egress::watch` watcher for the whole test
//! process ([`super::install`]), so a test that sends to a fake upstream
//! checks what lmgw sent without asking for it.

use lmgw_core::config::Protocol;
use lmgw_core::egress::watch::Built;
use serde_json::Value;

use super::diagnose::{self, Leaf};
use super::schemas::{self, Entry, Provider, Side};
use super::{extensions, known, ledger};

/// What the check found in one body.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The entry point it was checked against; `None` for a route the check
    /// does not cover yet.
    pub entry: Option<Entry>,
    /// The violations, as the report prints them.
    pub violations: Vec<Leaf>,
    /// The violations [`known::KNOWN`] lists.
    pub known: Vec<(Leaf, &'static str)>,
}

/// The watcher: checks `b`, prints and keeps what is wrong.
pub fn watched(b: &Built<'_>) {
    let out = check(b);
    let Some(entry) = out.entry else { return };
    for (leaf, reason) in &out.known {
        ledger::note(format!("{entry} at {} — {reason}\n  {leaf}", b.url));
    }
    if !out.violations.is_empty() {
        let body = serde_json::from_slice::<Value>(b.body)
            .map(|v| diagnose::short(&v))
            .unwrap_or_default();
        let list: Vec<String> = out.violations.iter().map(|l| format!("  {l}")).collect();
        ledger::record(format!(
            "lmgw sent {} ({:?} upstream) a body its schema refuses:\n{}\n  body: {body}",
            b.url,
            b.protocol,
            list.join("\n")
        ));
    }
}

/// `b` checked against its schema.
pub fn check(b: &Built<'_>) -> Outcome {
    let mut body: Value = match serde_json::from_slice(b.body) {
        Ok(v) => v,
        Err(e) => {
            return Outcome {
                entry: entry_for(b.protocol, b.url, &Value::Null),
                violations: vec![Leaf {
                    path: String::new(),
                    keyword: "json".into(),
                    unexpected: vec![],
                    missing: None,
                    message: format!("the body is not JSON: {e}"),
                    value: Value::Null,
                }],
                known: vec![],
            }
        }
    };
    let Some(entry) = entry_for(b.protocol, b.url, &body) else {
        return Outcome::default();
    };
    let sent = body.clone();
    extensions::strip(b.protocol, &mut body);
    let mut out = Outcome {
        entry: Some(entry),
        ..Outcome::default()
    };
    for mut leaf in diagnose::leaves(schemas::validator(entry), &body) {
        // The client's own top-level keys, forwarded as sent, are the
        // client's to answer for (`Built::passthrough`), their contents too.
        // A key lmgw wrote into (its value differs from the client's) is
        // lmgw's.
        let top = leaf.path.split('/').nth(1).unwrap_or("");
        if verbatim(b, &sent, top) {
            continue;
        }
        if leaf.keyword == "additionalProperties" && leaf.path.is_empty() {
            leaf.unexpected.retain(|k| !verbatim(b, &sent, k));
            if leaf.unexpected.is_empty() {
                continue;
            }
            leaf.message = format!("unknown key(s) {:?}", leaf.unexpected);
        }
        match known::matching(entry, &leaf) {
            Some(k) => out.known.push((leaf, k.reason)),
            None => out.violations.push(leaf),
        }
    }
    out
}

/// Key `k` of the body went out as the client sent it.
fn verbatim(b: &Built<'_>, sent: &Value, k: &str) -> bool {
    b.passthrough.get(k).is_some_and(|v| sent.get(k) == Some(v))
}

/// The entry point a body posted to `url` on a `protocol` upstream answers
/// to; `None` for a route not covered yet (llama-server's `/tokenize` and
/// `/apply-template`, rerank, System One, the legacy completions).
fn entry_for(protocol: Protocol, url: &str, body: &Value) -> Option<Entry> {
    let path = url.split('?').next().unwrap_or(url);
    let (provider, name) = match protocol {
        Protocol::Anthropic if path.ends_with("/v1/messages/count_tokens") => {
            (Provider::Anthropic, "count_tokens_request")
        }
        Protocol::Anthropic if path.ends_with("/v1/messages") => {
            if body["stream"] == Value::Bool(true) {
                (Provider::Anthropic, "messages_request_streaming")
            } else {
                (Provider::Anthropic, "messages_request")
            }
        }
        Protocol::Gemini if path.ends_with(":generateContent") => {
            (Provider::Gemini, "generate_content_request")
        }
        Protocol::Gemini if path.ends_with(":streamGenerateContent") => {
            (Provider::Gemini, "generate_content_request")
        }
        Protocol::Gemini if path.ends_with(":countTokens") => {
            (Provider::Gemini, "count_tokens_request")
        }
        Protocol::Gemini if path.ends_with(":batchEmbedContents") => {
            (Provider::Gemini, "batch_embed_contents_request")
        }
        Protocol::Openai | Protocol::OpenaiCompatible | Protocol::LlamaCpp => {
            if path.ends_with("/chat/completions") {
                (Provider::Openai, "chat_request")
            } else if path.ends_with("/responses") {
                (Provider::Openai, "responses_request")
            } else if path.ends_with("/embeddings") {
                (Provider::Openai, "embeddings_request")
            } else {
                return None;
            }
        }
        Protocol::Anthropic | Protocol::Gemini => return None,
    };
    Some(Entry {
        provider,
        side: Side::Request,
        name,
    })
}
