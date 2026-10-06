//! The auxiliary endpoints and the error mapping shared by the OpenAI-shaped
//! egresses (llama-egress design §2): embeddings, the Jina-shape rerank, the
//! count in a `/tokenize` answer, and an error body read as a
//! [`GatewayError`].

use serde_json::{json, Value};

use super::decode::parse_usage;
use crate::config::Upstream;
use crate::egress::apply_bearer_auth;
use crate::error::GatewayError;
use crate::ir::{
    EmbeddingsRequest, EmbeddingsResponse, RerankRequest, RerankResponse, RerankScore,
};

/// llama-server's `exceed_context_size_error` body (ladder design §2.1 fact 3):
/// `{"error":{"code":400,"message":"…","type":"exceed_context_size_error",
/// "n_prompt_tokens":8010,"n_ctx":4096}}`. Measured verbatim on this machine's
/// image (`server-task.cpp:1503`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceedContext {
    pub n_prompt_tokens: u64,
    pub n_ctx: u64,
}

/// Recognise llama-server's own over-long-prompt refusal in a raw error body,
/// so [`map_error`] can turn it into a [`GatewayError::ContextExceeded`]
/// instead of a generic `Upstream` 400.
/// `None` for every other shape — including the shared-pool overflow (fact 3
/// again: `type: "server_error"`, no `n_prompt_tokens`/`n_ctx`), which must
/// keep falling through to the generic path since it is not this refusal.
pub fn parse_exceed_context(body: &[u8]) -> Option<ExceedContext> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let error = v.get("error")?;
    if error.get("type").and_then(Value::as_str) != Some("exceed_context_size_error") {
        return None;
    }
    Some(ExceedContext {
        n_prompt_tokens: error.get("n_prompt_tokens").and_then(Value::as_u64)?,
        n_ctx: error.get("n_ctx").and_then(Value::as_u64)?,
    })
}

/// The error mapping of every OpenAI-shaped egress: llama.cpp's
/// [`context_refusal`] first, then [`upstream_error`]. Shared rather than
/// llama-only because a `generic` row in front of llama-server or llama-swap
/// gets the same refusal, and maps it today.
pub fn map_error(status: u16, body: &[u8]) -> GatewayError {
    context_refusal(status, body).unwrap_or_else(|| upstream_error(status, body))
}

/// llama-server's own context-size refusal (ladder design §2.1 fact 3,
/// §2.2 fact 14: "has no dedicated handling" until now) as a
/// [`GatewayError::ContextExceeded`] — the backstop for when the gate's own
/// pre-check undercounted (ladder design §3.1: "climb to the smallest rung
/// that fits … and retry once"). Checked ahead of the generic `Upstream`
/// fallback so a client always sees the stable `context_length_exceeded`
/// code instead of whatever wording that build of llama-server used. `None`
/// for any other status or body.
pub fn context_refusal(status: u16, body: &[u8]) -> Option<GatewayError> {
    if status != 400 {
        return None;
    }
    let ec = parse_exceed_context(body)?;
    // `map_error` carries no model/alias — that identity lives on the
    // `Route` its caller already has, several layers above the trait
    // method. `model` is left empty here, and every chat send site fills it
    // from that route with `crate::gate::attribute` before the error
    // reaches a client or a log line.
    Some(GatewayError::ContextExceeded {
        model: String::new(),
        prompt_tokens: ec.n_prompt_tokens,
        max_output: None,
        limit: ec.n_ctx,
        top_rung: None,
    })
}

/// Any upstream error body as [`GatewayError::Upstream`], with the provider's
/// `error.message` and `error.type` when the body is OpenAI-shaped.
pub fn upstream_error(status: u16, body: &[u8]) -> GatewayError {
    let v: Value = serde_json::from_slice(body).unwrap_or_default();
    let message = v
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(500).collect());
    let provider_type = v
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .map(String::from);
    GatewayError::Upstream {
        status,
        provider_type,
        message,
    }
}

pub fn build_embeddings(
    http: &reqwest::Client,
    up: &Upstream,
    model: &str,
    req: &EmbeddingsRequest,
) -> Result<reqwest::RequestBuilder, GatewayError> {
    let url = format!("{}/embeddings", up.base());
    let mut body = json!({"model": model, "input": req.inputs});
    if let Some(d) = req.dimensions {
        body["dimensions"] = json!(d);
    }
    let rb = http.post(url).json(&body);
    Ok(apply_bearer_auth(rb, up))
}

pub fn parse_embeddings(body: &[u8]) -> Result<EmbeddingsResponse, GatewayError> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| GatewayError::Transport(format!("invalid upstream JSON: {e}")))?;
    let mut embeddings = Vec::new();
    for item in v.get("data").and_then(Value::as_array).unwrap_or(&vec![]) {
        let vec: Vec<f32> = item
            .get("embedding")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_f64)
                    .map(|f| f as f32)
                    .collect()
            })
            .unwrap_or_default();
        embeddings.push(vec);
    }
    Ok(EmbeddingsResponse {
        embeddings,
        usage: parse_usage(v.get("usage")),
        model: v
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// The Jina request shape, which is the one llama-server documents for
/// `/v1/rerank`, and a de-facto extension on OpenAI-protocol rows: vLLM, TEI,
/// Infinity and Jina serve it on generic rows too (llama-egress design Q1).
/// `documents` rather than TEI's `texts`: the child auto-detects both, and
/// picking one keeps the response shape predictable (TEI's is a bare array).
pub fn build_rerank(
    http: &reqwest::Client,
    up: &Upstream,
    model: &str,
    req: &RerankRequest,
) -> Result<reqwest::RequestBuilder, GatewayError> {
    let mut body = json!({
        "model": model,
        "query": req.query,
        "documents": req.documents,
    });
    if let Some(n) = req.top_n {
        body["top_n"] = json!(n);
    }
    let rb = http.post(format!("{}/rerank", up.base())).json(&body);
    Ok(apply_bearer_auth(rb, up))
}

/// Accepts both response shapes a rerank backend may answer with: Jina's
/// `{"results": [{"index", "relevance_score"}]}` and TEI's bare
/// `[{"index", "score"}]`. Only the *request* shape is ours to choose; the
/// response is whatever the configured upstream returns.
pub fn parse_rerank(body: &[u8]) -> Result<RerankResponse, GatewayError> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| GatewayError::Transport(format!("invalid upstream JSON: {e}")))?;
    let rows = match v.get("results").and_then(Value::as_array) {
        Some(rows) => rows.clone(),
        None => v.as_array().cloned().ok_or_else(|| {
            GatewayError::Transport(
                "rerank response has neither a 'results' array nor a top-level array".into(),
            )
        })?,
    };
    let results = rows
        .iter()
        .map(|r| {
            let score = r
                .get("relevance_score")
                .or_else(|| r.get("score"))
                .and_then(Value::as_f64)
                .unwrap_or_default() as f32;
            RerankScore {
                index: r.get("index").and_then(Value::as_u64).unwrap_or_default() as usize,
                score,
            }
        })
        .collect();
    Ok(RerankResponse {
        results,
        usage: parse_usage(v.get("usage")),
        model: v
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// The count in a `/tokenize` answer: the number of token ids it returned.
/// llama.cpp's shape, which vLLM and SGLang answer in too; what both
/// OpenAI-shaped egresses' `parse_count` read.
pub fn parse_tokenize_count(body: &[u8]) -> Result<u64, GatewayError> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| GatewayError::Transport(format!("invalid upstream JSON: {e}")))?;
    v.get("tokens")
        .and_then(Value::as_array)
        .map(|a| a.len() as u64)
        .ok_or_else(|| GatewayError::Transport("tokenize response without 'tokens'".into()))
}
