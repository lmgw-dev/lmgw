//! What lmgw sends on purpose beyond its provider's pinned schema, per
//! upstream protocol.
//!
//! `llama_cpp` and `openai_compatible` rows speak OpenAI's wire, and lmgw
//! claims OpenAI's shape there, so their bodies are checked against OpenAI's
//! schema. A few fields are sent on purpose that OpenAI's schema does not
//! have: llama-server reads them, and so do the compatible servers they are
//! for (llama egress design, decision 17 and §7, §8). Each such field is one
//! row below, with the protocols it is sent to and why; the check removes
//! exactly those before it validates, and the rest of the body must be
//! OpenAI's. Anything not listed here is a violation like any other — this
//! is not an off-switch for unknown keys.
//!
//! The rows match the egress code; a new extension gets a row here in the
//! same change, or the check names it.

use lmgw_core::config::Protocol;
use serde_json::Value;

pub struct Extension {
    pub protocols: &'static [Protocol],
    pub what: &'static str,
    pub why: &'static str,
    strip: fn(&mut Value),
}

const LOCAL_AND_COMPATIBLE: &[Protocol] = &[Protocol::LlamaCpp, Protocol::OpenaiCompatible];
const LLAMA: &[Protocol] = &[Protocol::LlamaCpp];
const ANTHROPIC: &[Protocol] = &[Protocol::Anthropic];

pub const EXTENSIONS: &[Extension] = &[
    Extension {
        protocols: ANTHROPIC,
        what: "compaction blocks in a message's content",
        why: "Anthropic's compaction block (betas compact-2026-09-04 and compact-2026-01-12), \
              which the pinned non-beta request type has no block for: the client's, sent back \
              to Anthropic as Anthropic wrote it (gateway design §6.1)",
        strip: |b| {
            for m in messages(b) {
                if let Some(blocks) = m["content"].as_array_mut() {
                    blocks.retain(|c| c["type"] != "compaction");
                }
            }
        },
    },
    Extension {
        protocols: LOCAL_AND_COMPATIBLE,
        what: "top-level top_k, min_p, repeat_penalty",
        why: "llama.cpp's samplers, modelled (BodyDialect::COMPATIBLE; llama egress design \
              decision 17)",
        strip: |b| remove_top(b, &["top_k", "min_p", "repeat_penalty"]),
    },
    Extension {
        protocols: LLAMA,
        what: "top-level chat_template_kwargs, reasoning_budget_tokens, thinking_budget_tokens",
        why: "llama-server's thinking switch and budget, under both builds' names (llama \
              egress design §7, I3)",
        strip: |b| {
            remove_top(
                b,
                &[
                    "chat_template_kwargs",
                    "reasoning_budget_tokens",
                    "thinking_budget_tokens",
                ],
            )
        },
    },
    Extension {
        protocols: LOCAL_AND_COMPATIBLE,
        what: "reasoning_content on an assistant message",
        why: "the reasoning replay llama-server's --reasoning-preserve and DeepSeek read (llama \
              egress design decision 17)",
        strip: |b| {
            for m in messages(b) {
                if m["role"] == "assistant" {
                    if let Some(o) = m.as_object_mut() {
                        o.remove("reasoning_content");
                    }
                }
            }
        },
    },
    Extension {
        protocols: LOCAL_AND_COMPATIBLE,
        what: "a json_object response_format's schema, and a json_schema one without its \
               json_schema object",
        why: "llama-server's spellings of structured output, sent back as the client wrote them \
              (structured-output design §11.2, §11.17)",
        strip: |b| {
            let Some(f) = b.get_mut("response_format").and_then(Value::as_object_mut) else {
                return;
            };
            match f.get("type").and_then(Value::as_str) {
                Some("json_object") => {
                    f.remove("schema");
                }
                Some("json_schema") if !f.contains_key("json_schema") => {
                    f.insert("type".into(), "json_object".into());
                }
                _ => {}
            }
        },
    },
    Extension {
        protocols: LOCAL_AND_COMPATIBLE,
        what: "a top-level reasoning object on a chat request",
        why: "OpenRouter's reasoning object, the client's own, that lmgw brings in line with \
              the control it resolved (openai_wire::body::reconcile_reasoning_object, \
              openai egress design §5.3); OpenRouter-shaped compatible rows read it",
        strip: |b| {
            if b.get("messages").is_some() {
                remove_top(b, &["reasoning"]);
            }
        },
    },
    Extension {
        protocols: LLAMA,
        what: "image_url parts in a tool message's content",
        why: "a tool result's images, where the route's frozen decision lets them go \
              (llama egress design §8.1)",
        strip: |b| {
            for m in messages(b) {
                if m["role"] != "tool" {
                    continue;
                }
                if let Some(parts) = m["content"].as_array_mut() {
                    for p in parts.iter_mut().filter(|p| p["type"] == "image_url") {
                        *p = serde_json::json!({"type": "text", "text": "[image]"});
                    }
                }
            }
        },
    },
];

/// `body` without the extensions `protocol` is sent on purpose.
pub fn strip(protocol: Protocol, body: &mut Value) {
    for e in EXTENSIONS
        .iter()
        .filter(|e| e.protocols.contains(&protocol))
    {
        (e.strip)(body);
    }
}

fn remove_top(body: &mut Value, keys: &[&str]) {
    if let Some(o) = body.as_object_mut() {
        for k in keys {
            o.remove(*k);
        }
    }
}

fn messages(body: &mut Value) -> impl Iterator<Item = &mut Value> {
    body.get_mut("messages")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
}
