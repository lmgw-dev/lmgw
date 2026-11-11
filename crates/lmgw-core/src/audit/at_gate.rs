//! The gate's writer (§5.8): an entry for every non-`GET` request to a
//! route `CAPABILITY_TABLE` marks `Admin`, once the gate has let it in and
//! its handler has answered. One place, so a route added later is covered
//! by its table row; `audit::route_walk` sends a request to every such row.
//!
//! What it records: the caller ([`super::Actor`]), the op's name for
//! `/api/op/{name}` and the method with the route's template for every
//! other route, the row the request names (an op's [`super::redact::
//! identity`] arguments, another route's path), the settings fields a
//! settings op asked for ([`super::redact::patch_fields`]) or, for any
//! other request, the ones it changed, and the answer's status.
//!
//! Left to other writers: `/mcp/admin` (one route for reads and writes, so
//! the self-admin dispatcher writes its writing calls with the tool's name)
//! and an agent's own MCP endpoint (its calls act inside the agent's
//! server, as `/mcp`'s do, which is `Inference` and not logged). Left out:
//! the reading ops (an op neither writing nor revealing a secret by the op
//! table's marks, D84), the `POST`s that only read ([`READS`]) and every
//! `GET`.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, MatchedPath, Request};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

use super::{redact, Actor};
use crate::config::Snapshot;
use crate::proxy::RequestCtx;
use crate::state::SharedState;

/// The op plane's route.
pub const OP_ROUTE: &str = "/api/op/{name}";

/// The `Admin` write routes whose entries another writer makes (or none),
/// with why: `audit::route_walk` holds every other one to this writer.
pub const BY_OTHERS: &[(&str, &str, &str)] = &[
    (
        "POST",
        "/mcp/admin",
        "the self-admin dispatcher writes each writing lmgw__* call, with the tool's name",
    ),
    (
        "*",
        "/agents/{id}/mcp",
        "an agent's own tool server: its calls act inside the agent, as /mcp's do",
    ),
    (
        "*",
        "/agents/{id}/mcp/{*rest}",
        "an agent's own tool server: its calls act inside the agent, as /mcp's do",
    ),
];

/// The `Admin` `POST` routes that only read or compute an answer, as the
/// API document marks them (`x-lmgw-writes: false`, which
/// `audit::route_walk` holds this list to), each with why: no entry, as no
/// reading op has one (D84). A request that starts a model for its answer
/// is a request log row, as `/v1`'s are.
pub const READS: &[(&str, &str)] = &[
    (
        "/api/docs/search",
        "searches a corpus: a POST only for its body, it stores nothing",
    ),
    (
        "/api/knowledge/search",
        "searches a knowledge base: a POST only for its body, it stores nothing",
    ),
    (
        "/decision-lab/api/count",
        "counts an Ask's tokens against the model's limits and stores nothing",
    ),
    (
        "/decision-lab/api/batches/preview",
        "previews what a batch would run and stores nothing",
    ),
    (
        "/audio-lab/api/speech",
        "generates speech for the lab and answers it, as /v1/audio/speech does; the lab stores \
         a group only through its own groups route",
    ),
    (
        "/audio-lab/api/transcriptions",
        "transcribes a clip and answers the text, as /v1/audio/transcriptions does",
    ),
    (
        "/audio-lab/api/alignments",
        "aligns a clip with its transcript and answers the timings, as /v1/audio/alignments does",
    ),
    (
        "/audio-lab/api/tasks/run",
        "runs an audio task and answers its result, as /v1/tasks/run does",
    ),
    (
        "/voice-lab/api/clone-check",
        "speaks a check line with a saved sample and answers it, storing nothing",
    ),
];

/// Whether the gate writes an entry for `method` on route `template` at
/// `path` (`/api/op/<name>` for the op plane).
pub fn audits(method: &Method, template: &str, path: &str) -> bool {
    if *method == Method::GET || *method == Method::HEAD {
        return false;
    }
    if BY_OTHERS.iter().any(|(_, p, _)| *p == template)
        || (*method == Method::POST && READS.iter().any(|(p, _)| *p == template))
    {
        return false;
    }
    if template == OP_ROUTE {
        return op_name(path).is_some_and(op_audited);
    }
    true
}

/// Whether op `name` gets an entry: it writes, or it hands out a secret.
pub fn op_audited(name: &str) -> bool {
    crate::openapi::op_marks(name).is_some_and(|(writes, reveals)| writes || reveals)
}

fn op_name(path: &str) -> Option<&str> {
    path.strip_prefix("/api/op/").filter(|n| !n.is_empty())
}

/// The route template the router matched for `req`, else its path.
pub fn template_of(req: &Request, path: &str) -> String {
    req.extensions()
        .get::<MatchedPath>()
        .map_or_else(|| path.to_string(), |m| m.as_str().to_string())
}

/// Run the request and write its entry, when accounts are on before it or
/// after it (the switch's own entry in both directions, §5.11).
pub(crate) async fn run(
    state: &SharedState,
    before: Arc<Snapshot>,
    ctx: &RequestCtx,
    template: String,
    path: String,
    req: Request,
    next: Next,
) -> Response {
    let op = (template == OP_ROUTE)
        .then(|| op_name(&path).map(str::to_string))
        .flatten();
    let may_switch = op.as_deref() == Some("accounts_switch");
    if !before.accounts_on && !may_switch {
        return next.run(req).await;
    }
    let actor = Actor::of(&before, ctx, req.headers());
    let method = req.method().clone();
    let (req, mut target) = match &op {
        // The op's arguments, read here under the route's own body limit
        // (its `413` is the answer) and handed on whole: the handler reads
        // the same bytes.
        Some(name) => {
            let (parts, body) = req.into_parts();
            let bytes =
                match Bytes::from_request(Request::from_parts(parts.clone(), body), &()).await {
                    Ok(bytes) => bytes,
                    Err(rejection) => return rejection.into_response(),
                };
            let args: Map<String, Value> = serde_json::from_slice(&bytes).unwrap_or_default();
            let target = if redact::PATCH_OPS.contains(&name.as_str()) {
                let fields = redact::patch_fields(&args);
                super::target("fields", Value::Object(fields))
            } else {
                redact::identity(&args)
            };
            (Request::from_parts(parts, Body::from(bytes)), target)
        }
        None if path != template => (req, super::target("path", path.clone())),
        None => (req, Map::new()),
    };
    let resp = next.run(req).await;
    let after = state.snapshot();
    if !before.accounts_on && !after.accounts_on {
        return resp;
    }
    // A write that changed the settings as a side effect names what changed
    // between the two snapshots, where nothing tells its own change from
    // another request's that landed meanwhile (D85); a settings op names its
    // patch above.
    let patched = op
        .as_deref()
        .is_some_and(|n| redact::PATCH_OPS.contains(&n));
    if !patched && !Arc::ptr_eq(&before, &after) {
        let fields = redact::settings_fields(&before.settings, &after.settings);
        if !fields.is_empty() {
            target.insert("fields".into(), Value::Object(fields));
        }
    }
    let action = op.unwrap_or_else(|| format!("{method} {template}"));
    super::record(state, actor.entry(action, target, resp.status().as_u16())).await;
    resp
}
