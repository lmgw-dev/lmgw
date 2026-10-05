//! `POST /v1/embeddings` (OpenAI shape) and `POST /v1/rerank` (Jina shape),
//! plus the in-process call paths quickdoc uses for both.

use std::time::Instant;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::config::Route;
use crate::egress::{for_protocol, with_timeout};
use crate::error::GatewayError;
use crate::gate::{FallbackReason, GateHeaders};
use crate::ingress::ClientProto;
use crate::ir::{EmbeddingsRequest, EmbeddingsResponse, RerankRequest, RerankResponse, Usage};
use crate::state::SharedState;
use crate::telemetry::RequestClass;

use super::*;

/// `POST /v1/embeddings` (OpenAI shape in/out, §6).
pub async fn handle_embeddings(state: SharedState, ctx: RequestCtx, body: Value) -> Response {
    let started = Instant::now();
    state.telemetry.request_started();
    let proto = ClientProto::OpenaiChat;

    let alias = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();

    if let Some(r) = policy_or_refuse(&state, proto, &ctx, &alias, started, RequestClass::Aux).await
    {
        return r;
    }

    let result = embeddings_inner(&state, &body).await;
    match result {
        Ok((route, headers, response_body, usage)) => {
            record(
                LogParams {
                    state: &state,
                    proto,
                    ctx: &ctx,
                    alias,
                    route: Some(&route),
                    started,
                    streamed: false,
                    class: RequestClass::Aux,
                    timings: None,
                    max_tokens_clamped: None,
                    fallback: headers.fallback_reason(),
                    rung: None,
                },
                200,
                None,
                usage,
                None,
            )
            .await;
            headers.stamp(axum::Json(response_body).into_response())
        }
        Err((route, headers, e)) => {
            let resp = headers.stamp(error_response(proto, &e));
            record(
                LogParams {
                    state: &state,
                    proto,
                    ctx: &ctx,
                    alias,
                    route: route.as_deref(),
                    started,
                    streamed: false,
                    class: RequestClass::Aux,
                    timings: None,
                    max_tokens_clamped: None,
                    fallback: headers.fallback_reason(),
                    rung: None,
                },
                e.http_status().as_u16(),
                None,
                Usage::default(),
                Some((e.kind(), e.to_string())),
            )
            .await;
            resp
        }
    }
}

async fn embeddings_inner(state: &SharedState, body: &Value) -> Result<Served<Value>, Failed> {
    let alias = body.get("model").and_then(Value::as_str).ok_or((
        None,
        GateHeaders::default(),
        GatewayError::BadRequest("missing 'model'".into()),
    ))?;
    let (inputs, dimensions, base64) =
        embeddings_fields(body).map_err(|e| (None, GateHeaders::default(), e))?;

    let (route, headers, parsed) =
        embed_in_process(state, alias, inputs, dimensions, false).await?;
    let data: Vec<Value> = parsed
        .embeddings
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let embedding = if base64 {
                json!(base64_f32(e))
            } else {
                json!(e)
            };
            json!({"object": "embedding", "index": i, "embedding": embedding})
        })
        .collect();
    let response_body = json!({
        "object": "list",
        "data": data,
        "model": alias,
        "usage": {
            "prompt_tokens": parsed.usage.prompt_tokens.unwrap_or(0),
            "total_tokens": parsed.usage.prompt_tokens.unwrap_or(0),
        },
    });
    Ok((route, headers, response_body, parsed.usage))
}

/// The request fields beyond `model`: the texts, `dimensions`, and whether
/// the answer is base64-encoded. Anything lmgw cannot honour is a 400 here,
/// before the gate starts a model for it.
fn embeddings_fields(body: &Value) -> Result<(Vec<String>, Option<u32>, bool), GatewayError> {
    let inputs = match body.get("input") {
        Some(Value::String(s)) => vec![s.clone()],
        // Every element or none: a token-id array (OpenAI's other `input`
        // shape) names ids of one tokenizer, and an alias can route to a
        // model with another — so it is refused by position rather than
        // skipped, which would shift every later `index` onto the wrong text.
        Some(Value::Array(a)) => a
            .iter()
            .enumerate()
            .map(|(i, v)| {
                v.as_str().map(String::from).ok_or_else(|| {
                    GatewayError::BadRequest(format!(
                        "'input[{i}]' is not a string — lmgw embeds text only, not token ids"
                    ))
                })
            })
            .collect::<Result<_, _>>()?,
        _ => {
            return Err(GatewayError::BadRequest(
                "missing or invalid 'input'".into(),
            ))
        }
    };
    let dimensions = match body.get("dimensions") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|&d| d > 0)
                .and_then(|d| u32::try_from(d).ok())
                .ok_or_else(|| {
                    GatewayError::BadRequest(format!(
                        "'dimensions' must be a positive integer, got {v}"
                    ))
                })?,
        ),
    };
    // A wire format, so it is ours to honour, not the upstream's: the vectors
    // are parsed as floats from every backend and encoded here.
    let base64 = match body.get("encoding_format") {
        None | Some(Value::Null) => false,
        Some(v) => match v.as_str() {
            Some("float") => false,
            Some("base64") => true,
            _ => {
                return Err(GatewayError::BadRequest(format!(
                    "'encoding_format' must be \"float\" or \"base64\", got {v}"
                )))
            }
        },
    };
    Ok((inputs, dimensions, base64))
}

/// `encoding_format: "base64"` as OpenAI serves it: the vector's float32
/// values, little-endian, base64-encoded — what the OpenAI SDKs decode (and
/// what the Python one asks for by default).
fn base64_f32(v: &[f32]) -> String {
    use base64::Engine as _;
    let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Resolve, gate, call, parse — everything `/v1/embeddings` does except wrap the
/// result in OpenAI's JSON envelope (§6).
///
/// Split out so quickdoc's in-process embedder rides the *same* path instead of
/// looping back through HTTP: same alias resolution, same egress adapter, and —
/// the part that matters — the same model-kind gate below, so no ingestion path
/// can reach a reranker section behind lmgw's back.
///
/// `pinned`: the caller is bound to the model it named (quickdoc: a corpus
/// pins the model its vectors come from), so admission never swaps it to a
/// fallback when VRAM outside lmgw's control is short — it waits for room
/// (candidate-aliases design §4.7, [`crate::gate::Routed::admit_pinned`]).
/// The hold's swap still happens; the caller refuses that one itself.
///
/// `dimensions`: [`EmbeddingsRequest::dimensions`]. Every vector that comes
/// back has to have that length, or the call is refused as unsupported by
/// the route — a backend that ignores the field would otherwise hand the
/// client full-length vectors it did not ask for.
pub(crate) async fn embed_in_process(
    state: &SharedState,
    alias: &str,
    inputs: Vec<String>,
    dimensions: Option<u32>,
    pinned: bool,
) -> Result<(Route, GateHeaders, EmbeddingsResponse), Failed> {
    let req = EmbeddingsRequest {
        model_alias: alias.to_string(),
        inputs,
        dimensions,
    };

    // The gate's per-request half: the hold swap, the media check and the
    // reranker-section guard ([`crate::gate::RouteCheck::Embeddings`]), then
    // admission. The motivating case for §9b: a bulk ingest embedding into an
    // aux router whose GPU the chat model already fills. Admission makes the
    // room (or refuses by name) before the call that would otherwise OOM the
    // container, and a local route comes back on the port its container
    // answers on (§5).
    let check = crate::gate::RouteCheck::Embeddings;
    let opened = if pinned {
        crate::gate::open_pinned(state, alias, check).await
    } else {
        crate::gate::open(state, alias, check).await
    };
    let crate::gate::Opened {
        route,
        hold,
        headers,
    } = opened.map_err(|f| (f.route, f.headers, f.error))?;

    let egress = for_protocol(route.upstream.protocol);

    let timeout = route.upstream.request_timeout();
    let resp = crate::vram::send_local(hold.as_ref(), &route, None, |r| {
        Ok(with_timeout(
            egress.build_embeddings(&state.http, &r.upstream, &r.upstream_model, &req)?,
            timeout,
        ))
    })
    .await
    .map_err(|e| (Some(Box::new(route.clone())), headers.clone(), e))?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| {
        (
            Some(Box::new(route.clone())),
            headers.clone(),
            GatewayError::from(e),
        )
    })?;
    if !status.is_success() {
        // `map_error`'s backstop can return `GatewayError::ContextExceeded`
        // with an empty `model` — it has no route to name one. Fill it in
        // the same way `gate::fit::attribute` does at every chat site
        // (review finding 6): an embedding request can overflow a guarded
        // row's context exactly like a chat one can.
        return Err((
            Some(Box::new(route.clone())),
            headers,
            crate::gate::attribute(egress.map_error(status.as_u16(), &bytes), &route),
        ));
    }
    let parsed = egress
        .parse_embeddings(&bytes)
        .map_err(|e| (Some(Box::new(route.clone())), headers.clone(), e))?;
    if let Some(want) = dimensions {
        if let Some(got) = parsed
            .embeddings
            .iter()
            .map(Vec::len)
            .find(|&n| n != want as usize)
        {
            let why = if route.upstream.kind == crate::config::UpstreamKind::LlamaServer {
                " — llama-server has no such parameter; omit it for the model's own size"
            } else {
                ""
            };
            return Err((
                Some(Box::new(route.clone())),
                headers,
                GatewayError::Unsupported(format!(
                    "'dimensions' on '{}' (upstream '{}'): asked for {want}, it returned \
                     {got}-dimensional vectors{why}",
                    route.upstream_model, route.upstream.name
                )),
            ));
        }
    }
    Ok((route, headers, parsed))
}

/// [`embed_in_process`] plus the `request_logs` row — the embedding counterpart
/// of [`sample_once`], and what every in-process caller should use.
///
/// The split exists because `/v1/embeddings` logs in its own handler (it has a
/// [`RequestCtx`] with the caller's key, which an in-process call has no
/// equivalent of); routing the HTTP path through here too would write the row
/// twice. Everything else — quickdoc's ingest batches, re-embed, corpus probe
/// and query embedding — goes through this one, so "every request produces a
/// row" holds for them as it does for public traffic.
///
/// A failure that never resolved an alias writes no row: [`record_in_process`]
/// keys a row to the upstream that answered, and there is none. The caller's
/// own error is where that surfaces — it is a configuration fault, not traffic.
///
/// Pinned ([`embed_in_process`]'s `pinned`): every caller is quickdoc, whose
/// corpus must never take vectors from a fallback.
pub(crate) async fn embed_once(
    state: &SharedState,
    alias: &str,
    inputs: Vec<String>,
    ingress_proto: &str,
) -> Result<(Route, GateHeaders, EmbeddingsResponse), Failed> {
    let started = Instant::now();
    state.telemetry.request_started();
    let result = embed_in_process(state, alias, inputs, None, true).await;
    log_in_process_aux(
        state,
        alias,
        ingress_proto,
        started,
        match &result {
            Ok((_, headers, _)) | Err((_, headers, _)) => headers.fallback_reason(),
        },
        match &result {
            Ok((route, _, resp)) => Ok((route, resp.usage)),
            Err((route, _, e)) => Err((route.as_deref(), e)),
        },
    )
    .await;
    result
}

/// `POST /v1/rerank` (Jina shape in/out — quickdoc §9a).
pub async fn handle_rerank(state: SharedState, ctx: RequestCtx, body: Value) -> Response {
    let started = Instant::now();
    state.telemetry.request_started();
    let proto = ClientProto::OpenaiChat;

    let alias = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();

    if let Some(r) = policy_or_refuse(&state, proto, &ctx, &alias, started, RequestClass::Aux).await
    {
        return r;
    }

    match rerank_inner(&state, &body).await {
        Ok((route, headers, response_body, usage)) => {
            record(
                LogParams {
                    state: &state,
                    proto,
                    ctx: &ctx,
                    alias,
                    route: Some(&route),
                    started,
                    streamed: false,
                    class: RequestClass::Aux,
                    timings: None,
                    max_tokens_clamped: None,
                    fallback: headers.fallback_reason(),
                    rung: None,
                },
                200,
                None,
                usage,
                None,
            )
            .await;
            headers.stamp(axum::Json(response_body).into_response())
        }
        Err((route, headers, e)) => {
            let resp = headers.stamp(error_response(proto, &e));
            record(
                LogParams {
                    state: &state,
                    proto,
                    ctx: &ctx,
                    alias,
                    route: route.as_deref(),
                    started,
                    streamed: false,
                    class: RequestClass::Aux,
                    timings: None,
                    max_tokens_clamped: None,
                    fallback: headers.fallback_reason(),
                    rung: None,
                },
                e.http_status().as_u16(),
                None,
                Usage::default(),
                Some((e.kind(), e.to_string())),
            )
            .await;
            resp
        }
    }
}

async fn rerank_inner(state: &SharedState, body: &Value) -> Result<Served<Value>, Failed> {
    let alias = body.get("model").and_then(Value::as_str).ok_or((
        None,
        GateHeaders::default(),
        GatewayError::BadRequest("missing 'model'".into()),
    ))?;
    let query = body.get("query").and_then(Value::as_str).ok_or((
        None,
        GateHeaders::default(),
        GatewayError::BadRequest("missing 'query'".into()),
    ))?;
    // Jina says `documents`, TEI says `texts`. Both are accepted here for the
    // same reason llama-server accepts both: a client that already speaks one
    // of them should not have to learn the other to reach lmgw.
    let documents: Vec<String> = match body.get("documents").or_else(|| body.get("texts")) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect(),
        _ => {
            return Err((
                None,
                GateHeaders::default(),
                GatewayError::BadRequest(
                    "missing or invalid 'documents' (an array of strings; 'texts' is accepted \
                     as the TEI spelling)"
                        .into(),
                ),
            ))
        }
    };
    let top_n = body
        .get("top_n")
        .and_then(Value::as_u64)
        .map(|n| n as usize);

    let (route, headers, parsed) =
        rerank_in_process(state, alias, query, documents, top_n, false).await?;
    let results: Vec<Value> = parsed
        .results
        .iter()
        .map(|r| json!({"index": r.index, "relevance_score": r.score}))
        .collect();
    let response_body = json!({
        "object": "list",
        "model": alias,
        "results": results,
        "usage": {
            "prompt_tokens": parsed.usage.prompt_tokens.unwrap_or(0),
            "total_tokens": parsed.usage.prompt_tokens.unwrap_or(0),
        },
    });
    Ok((route, headers, response_body, parsed.usage))
}

/// Resolve, gate, call, parse — `/v1/rerank` without the JSON envelope.
///
/// The mirror image of [`embed_in_process`]'s gate: `/v1/embeddings` refuses a
/// model lmgw knows to be a reranker, and this refuses one it knows to be an
/// embedder. A section without `reranking = true` answers a rerank request with
/// whatever its pooling produces, which is not a relevance score and would
/// silently reorder a corpus's answers. Models lmgw has no kind for (a remote
/// provider's) are forwarded — their kind is theirs to know.
///
/// `pinned`: as for [`embed_in_process`] — quickdoc's trace names the
/// reranker, so its calls never swap to a fallback at admission.
pub(crate) async fn rerank_in_process(
    state: &SharedState,
    alias: &str,
    query: &str,
    documents: Vec<String>,
    top_n: Option<usize>,
    pinned: bool,
) -> Result<(Route, GateHeaders, RerankResponse), Failed> {
    // The gate's per-request half, as in `embed_in_process`: the hold swap,
    // the media check and the embedding-section guard
    // ([`crate::gate::RouteCheck::Rerank`]), then admission.
    let check = crate::gate::RouteCheck::Rerank;
    let opened = if pinned {
        crate::gate::open_pinned(state, alias, check).await
    } else {
        crate::gate::open(state, alias, check).await
    };
    let crate::gate::Opened {
        route,
        hold,
        headers,
    } = opened.map_err(|f| (f.route, f.headers, f.error))?;

    let n = documents.len();
    let req = RerankRequest {
        model_alias: alias.to_string(),
        query: query.to_string(),
        documents,
        top_n,
    };
    let egress = for_protocol(route.upstream.protocol);
    let timeout = route.upstream.request_timeout();
    let resp = crate::vram::send_local(hold.as_ref(), &route, None, |r| {
        Ok(with_timeout(
            egress.build_rerank(&state.http, &r.upstream, &r.upstream_model, &req)?,
            timeout,
        ))
    })
    .await
    .map_err(|e| (Some(Box::new(route.clone())), headers.clone(), e))?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| {
        (
            Some(Box::new(route.clone())),
            headers.clone(),
            GatewayError::from(e),
        )
    })?;
    if !status.is_success() {
        // Same reason as `embed_in_process` above (review finding 6):
        // `map_error`'s backstop can hand back an unattributed
        // `ContextExceeded`, and a rerank request can overflow a guarded
        // row's context exactly like a chat one can.
        return Err((
            Some(Box::new(route.clone())),
            headers,
            crate::gate::attribute(egress.map_error(status.as_u16(), &bytes), &route),
        ));
    }
    let parsed = egress
        .parse_rerank(&bytes)
        .map_err(|e| (Some(Box::new(route.clone())), headers.clone(), e))?;
    if let Some(bad) = parsed.results.iter().find(|r| r.index >= n) {
        return Err((
            Some(Box::new(route.clone())),
            headers,
            GatewayError::Transport(format!(
                "rerank upstream scored document {} of {n} — the response does not match the \
                 request",
                bad.index
            )),
        ));
    }
    Ok((route, headers, parsed))
}

/// [`rerank_in_process`] plus the `request_logs` row — [`embed_once`] for the
/// rerank stage, and logged for the same reason: a query whose time went to the
/// reranker should say so in Logs rather than look like a slow search. Pinned,
/// like [`embed_once`]: every caller is quickdoc's retrieval.
pub(crate) async fn rerank_once(
    state: &SharedState,
    alias: &str,
    query: &str,
    documents: Vec<String>,
    top_n: Option<usize>,
    ingress_proto: &str,
) -> Result<(Route, GateHeaders, RerankResponse), Failed> {
    let started = Instant::now();
    state.telemetry.request_started();
    let result = rerank_in_process(state, alias, query, documents, top_n, true).await;
    log_in_process_aux(
        state,
        alias,
        ingress_proto,
        started,
        match &result {
            Ok((_, headers, _)) | Err((_, headers, _)) => headers.fallback_reason(),
        },
        match &result {
            Ok((route, _, resp)) => Ok((route, resp.usage)),
            Err((route, _, e)) => Err((route.as_deref(), e)),
        },
    )
    .await;
    result
}

/// The row-writing half [`embed_once`] and [`rerank_once`] share: an encoder
/// call has no completion tokens and never streams, so the only thing that
/// varies between them is which outcome carried which usage.
async fn log_in_process_aux(
    state: &SharedState,
    alias: &str,
    ingress_proto: &str,
    started: Instant,
    fallback: Option<FallbackReason>,
    outcome: Result<(&Route, Usage), (Option<&Route>, &GatewayError)>,
) {
    let (route, status, usage, error) = match outcome {
        Ok((route, usage)) => (Some(route), StatusCode::OK.as_u16(), usage, None),
        Err((route, e)) => (
            route,
            e.http_status().as_u16(),
            Usage::default(),
            Some((e.kind(), e.to_string())),
        ),
    };
    let Some(route) = route else {
        // Nothing resolved, so `request_started` above has to be undone by
        // hand: `record_in_process` is what normally closes it out.
        state.telemetry.request_abandoned();
        return;
    };
    record_in_process(
        InProcessLog {
            key: KeyRef::default(),
            ingress_proto,
            alias,
            route,
            started,
            streamed: false,
            class: RequestClass::Chat,
            timings: None,
            max_tokens_clamped: None,
            fallback,
            rung: None,
        },
        status,
        Some(started.elapsed().as_millis() as i64),
        usage,
        error,
        state,
    )
    .await;
}
