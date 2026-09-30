//! JSON + SSE admin plane (`/api/*`) for the Leptos UI (crates/lmgw-ui).
//!
//! Design (docs/design/ui-rebuild/plan.md, core-notes.md §12):
//! - Mutations wrap `crate::ops` 1:1 — validation, secret redaction and
//!   `reload_snapshot()` all live there; this layer only translates HTTP.
//! - Mounted like the rest of the dashboard plane: every route here needs
//!   [`Cap::Admin`](crate::principal::Cap::Admin) — an enabled owner key, in
//!   the session cookie or as a bearer (principals §3.2) — except
//!   `GET /api/version`, which a page may read before it has a session. The
//!   trust boundary is the principal, not `bind_addr`. `self_admin` is
//!   untouched by that: it stays the switch for the *agent-facing* tool
//!   plane, never for the human at the dashboard.
//! - Errors: every non-2xx body is an `ApiError`. `ops` returns bare
//!   `String` errors with no status semantics, so they map to 400 wholesale;
//!   unknown routes are 404.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseFrame, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::stream::{self, Stream, StreamExt};
use lmgw_api_types as dto;
use serde::Deserialize;
use serde_json::Value;

use crate::config::HoldFallbackMode;
use crate::runtime::{lifecycle, Class};
use crate::state::SharedState;
use crate::telemetry::Event;

use super::audio;

pub fn routes(state: &SharedState) -> Router<SharedState> {
    use crate::principal::Cap;
    use crate::server::require;

    Router::new()
        // The one read a page may do before it has a session (principals
        // §3.2): the footer prints the version, and so does the login view.
        // Registered in its own router so the `Admin` layer below cannot
        // reach it.
        .merge(
            Router::new()
                .route("/api/version", get(version))
                .route_layer(require(state, Cap::Public)),
        )
        .merge(admin_routes().route_layer(require(state, Cap::Admin)))
}

/// Everything on this plane that needs [`Cap::Admin`], which is everything
/// else: `/api/*` reads, the op dispatcher, the SSE feed.
fn admin_routes() -> Router<SharedState> {
    Router::new()
        // This document, generated (api-docs design §4.11) rather than
        // hand-maintained: `Admin`, so its `/v1/openapi.json` sibling below in
        // `server::build_router` can carry only the inference plane.
        // Compressed: the only route that is (`openapi/serve.rs`).
        .route(
            "/api/openapi.json",
            get(crate::openapi::admin_json).layer(tower_http::compression::CompressionLayer::new()),
        )
        .route("/api/status", get(status))
        .route("/api/connect", get(connect))
        .route("/api/logs", get(logs))
        .route("/api/events", get(events))
        .route("/api/models/full", get(models_full))
        .route("/api/local-model", get(local_model))
        .route("/api/local-model-check", get(local_model_check))
        .route("/api/gguf-files", get(gguf_files))
        .route("/api/model-inspect", get(model_inspect))
        .route("/api/local-model-plan", get(local_model_plan))
        .route("/api/ladder-rung-plan", get(ladder_rung_plan))
        .route("/api/llama-flags", get(llama_flags))
        .route("/api/upstreams", get(upstreams))
        .route("/api/wiring", get(wiring))
        .route("/api/mcp-servers", get(mcp_servers))
        .route("/api/mcp-servers/{id}/tools", get(mcp_server_tools))
        .route("/api/tools", get(tools))
        .route("/api/upstream-models", get(upstream_models))
        .route("/api/hf/repo", get(hf_repo))
        .route("/api/hf/downloads", get(hf_downloads))
        .route("/api/jobs", get(jobs))
        .route("/api/vram", get(vram))
        .route("/api/audio/catalog", get(audio_catalog))
        .route(
            "/api/settings-full",
            get(super::api_settings::settings_full),
        )
        .route("/api/responses", get(responses_index))
        .route("/api/responses/chain", get(responses_chain))
        // Usage analytics, cost & policy (usage-analytics design §5) — one
        // query surface, so a chart cannot invent its own arithmetic.
        .route("/api/usage/series", get(super::api_usage::series))
        .route("/api/usage/top", get(super::api_usage::top))
        .route("/api/usage/heat", get(super::api_usage::heat))
        .route("/api/usage/errors", get(super::api_usage::errors))
        .route("/api/usage/local", get(super::api_usage::local))
        .route("/api/usage/keys", get(super::api_usage::keys))
        .route("/api/usage/prices", get(super::api_usage::prices))
        .route("/api/usage/export.csv", get(super::api_usage::export_csv))
        .route("/api/op/{name}", post(op))
}

/// `Err(String)` from `ops` → 400 with a structured body. Shared with the
/// per-domain modules of this plane (`api_settings`, `api_agents`) so every
/// `/api` failure has one shape.
pub(super) fn ops_result(res: Result<Value, String>) -> Response {
    match res {
        Ok(v) => Json(v).into_response(),
        Err(message) => (
            StatusCode::BAD_REQUEST,
            Json(dto::ApiError {
                code: "op_failed".into(),
                message,
            }),
        )
            .into_response(),
    }
}

async fn version() -> Json<dto::VersionInfo> {
    Json(dto::VersionInfo {
        version: env!("CARGO_PKG_VERSION").into(),
    })
}

async fn status(State(st): State<SharedState>) -> Response {
    ops_result(crate::ops::status(&st).await)
}

/// `GET /api/vram` — the GPU ledger and the admission queue (quickdoc §9b),
/// freshly measured. The same object the `vram` SSE frame carries, so a page
/// can fetch once and then follow the stream.
async fn vram(State(st): State<SharedState>) -> Response {
    Json(st.vram.view(&st).await).into_response()
}

async fn connect(State(st): State<SharedState>) -> Json<dto::ConnectInfo> {
    let snap = st.snapshot();
    let bases = crate::net::reachable_urls(&snap.settings.bind_addr)
        .into_iter()
        .map(|r| dto::BaseUrl {
            url: r.url,
            label: r.label,
        })
        .collect();
    let models = snap
        .exposed_models()
        .into_iter()
        .map(|m| dto::ConnectModel {
            name: m.name,
            source: m.source.to_string(),
        })
        .collect();
    let catalog_prefixes = snap
        .upstreams
        .values()
        .filter(|u| u.enabled && u.expose_all)
        .map(|u| {
            if u.expose_prefix.is_empty() {
                u.name.clone()
            } else {
                u.expose_prefix.clone()
            }
        })
        .collect();
    Json(dto::ConnectInfo {
        bases,
        auth_enabled: snap.settings.auth_enabled,
        models,
        catalog_prefixes,
    })
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct LogsQuery {
    limit: Option<i64>,
    /// Deserialized as a string, not `Option<bool>`: serde's bool rejects
    /// anything but `true`/`false`, so a hand-typed (or older) `?errors_only=1`
    /// answered **400 Failed to deserialize query string** — a link that looks
    /// right and returns nothing. See [`truthy`].
    errors_only: Option<String>,
    alias: Option<String>,
    upstream: Option<String>,
    before_id: Option<i64>,
    /// Identity, not name: a key filter has to survive a rename.
    key_id: Option<i64>,
    /// One refusal or error kind — what the Usage page's errors chart links a
    /// segment to. Without it the link would silently return the unfiltered
    /// head, which looks like an answer.
    error_kind: Option<String>,
    class: Option<String>,
    /// Substring matches (case-insensitive) for what a Traffic filter box
    /// types; `alias`/`upstream` stay exact, which is what a link carries.
    alias_q: Option<String>,
    upstream_q: Option<String>,
}

/// The spellings a query-string boolean is allowed to arrive in.
fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Same row shape as the `request` SSE frame ([`dto::RequestRow`]), so the
/// live feed and pagination share one type. Goes to `store` directly rather
/// than `ops::logs`, whose JSON uses different key names (tool-plane legacy).
async fn logs(State(st): State<SharedState>, Query(q): Query<LogsQuery>) -> Response {
    let filter = crate::store::LogFilter {
        alias: q.alias.filter(|s| !s.is_empty()),
        upstream_name: q.upstream.filter(|s| !s.is_empty()),
        errors_only: q.errors_only.as_deref().is_some_and(truthy),
        limit: q.limit.unwrap_or(50),
        before_id: q.before_id,
        key_id: q.key_id,
        error_kind: q.error_kind.filter(|s| !s.is_empty()),
        class: q.class.filter(|s| !s.is_empty()),
        alias_q: q.alias_q.filter(|s| !s.trim().is_empty()),
        upstream_q: q.upstream_q.filter(|s| !s.trim().is_empty()),
    };
    match crate::store::query_logs(&st.db, &filter).await {
        Ok(rows) => {
            let logs = rows
                .into_iter()
                .map(|r| dto::RequestRow {
                    log_id: r.id,
                    ts: r.ts,
                    client_key: r.client_key,
                    ingress_proto: r.ingress_proto,
                    requested_alias: r.requested_alias,
                    upstream_name: r.upstream_name,
                    upstream_model: r.upstream_model,
                    egress_proto: r.egress_proto,
                    status: r.status as u16,
                    ttfb_ms: r.ttfb_ms,
                    total_ms: r.total_ms,
                    prompt_tokens: r.prompt_tokens,
                    completion_tokens: r.completion_tokens,
                    cached_in_tokens: r.cached_in_tokens,
                    cache_write_tokens: r.cache_write_tokens,
                    streamed: r.streamed,
                    error_kind: r.error_kind,
                    error_msg: r.error_msg,
                    mcp_tool: r.mcp_tool,
                    cost_micro: r.cost_micro,
                    class: r.class.unwrap_or_default(),
                    key_id: r.key_id,
                    max_tokens_clamped: r.max_tokens_clamped,
                    fallback_reason: r.fallback_reason,
                    rung: r.rung,
                })
                .collect();
            Json(dto::LogsResponse { logs }).into_response()
        }
        Err(e) => ops_result(Err(e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Models domain
// ---------------------------------------------------------------------------

async fn models_full(State(st): State<SharedState>) -> Response {
    let snap = st.snapshot();
    let aliases = snap
        .aliases
        .values()
        .map(|a| dto::AliasView {
            id: a.id,
            alias: a.alias.clone(),
            upstream_id: a.upstream_id,
            upstream_name: snap.upstreams.get(&a.upstream_id).map(|u| u.name.clone()),
            upstream_model_id: a.upstream_model_id.clone(),
            param_overrides: serde_json::from_value(
                serde_json::to_value(&a.param_overrides).unwrap_or_default(),
            )
            .unwrap_or_default(),
            enabled: a.enabled,
            capabilities_override: a.capabilities_override.clone(),
        })
        .collect();
    let local = snap
        .local_models
        .iter()
        .map(|m| {
            serde_json::to_value(m).map(|v| {
                serde_json::json!({
                    "public_name": snap.local_public_name(&m.model_id),
                    "model": v,
                })
            })
        })
        .collect::<Result<Vec<_>, _>>();
    let audio = crate::store::list_audio_models(&st.db).await;
    let (Ok(local), Ok(audio)) = (local, audio) else {
        return ops_result(Err("failed to load model lists".into()));
    };
    // One `candidates::derive` per row (candidate-aliases design §4.6) — a
    // cached GGUF read per candidate, warm calls free (see that function's
    // own cost note). A plain loop, not `join_all`: this is a desktop app for
    // one user, and a handful of aliases at most.
    //
    // The deferral count (§6) is one grouped query for every alias, read
    // once before the loop rather than once per alias
    // (`candidates::deferrals::deferrals_24h`'s own doc comment).
    let deferral_counts = crate::candidates::deferrals::deferrals_24h(&st.db)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("deferrals_24h query failed, showing zero for every alias: {e}");
            Default::default()
        });
    let mut candidate_aliases: Vec<dto::CandidateAliasView> = Vec::new();
    for c in snap.candidate_aliases.values() {
        let d = crate::candidates::derive::derive(&st, &snap, c).await;
        candidate_aliases.push(dto::CandidateAliasView {
            id: c.id,
            alias: c.alias.clone(),
            candidates: c.candidates.clone(),
            background: c.background,
            fallback_mode: c.fallback_mode.as_str().to_string(),
            fallback: c.fallback.clone(),
            capabilities_disabled: c.capabilities_disabled.clone(),
            enabled: c.enabled,
            notes: c.notes.clone(),
            enabled_facets: d.enabled.names(),
            common_facets: d.common.names(),
            unsupported_by: d.unsupported_by,
            routable: d.routable,
            problems: d.problems,
            advisories: d.advisories,
            fallback_usable: d.fallback_usable,
            context_length: d.context_length,
            max_output_tokens: d.max_output_tokens,
            deferrals_24h: crate::candidates::deferrals::for_alias(&deferral_counts, &c.alias),
        });
    }
    candidate_aliases.sort_by(|a, b| a.alias.cmp(&b.alias));
    let audio_prefix = snap.settings.audio.public_prefix.clone();
    let prefixed = |prefix: &str, id: &str| match prefix.trim_matches('/') {
        "" => id.to_string(),
        p => format!("{p}/{id}"),
    };
    let out = serde_json::json!({
        "aliases": aliases_sorted(aliases),
        "candidate_aliases": candidate_aliases,
        "local": local,
        "aux": snap.aux_models
            .iter()
            .map(|m| serde_json::json!({
                "public_name": snap.aux_public_name(&m.model_id),
                "model": m,
            }))
            .collect::<Vec<_>>(),
        "audio": audio
            .iter()
            .map(|m| serde_json::json!({
                "public_name": prefixed(&audio_prefix, &m.model_id),
                "model": m,
            }))
            .collect::<Vec<_>>(),
        "image": snap.image_models
            .iter()
            .map(|m| serde_json::json!({
                "public_name": snap.image_public_name(&m.model_id),
                "model": m,
            }))
            .collect::<Vec<_>>(),
        "passthrough": snap
            .upstreams
            .values()
            .filter(|u| u.enabled && u.expose_all)
            .map(|u| serde_json::json!({
                "upstream": u.name,
                "id": u.id,
                "prefix": u.prefix(),
                "hidden": snap
                    .hidden_passthrough
                    .iter()
                    .filter(|(uid, _)| *uid == u.id)
                    .map(|(_, model_id)| model_id.clone())
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
    });
    Json(out).into_response()
}

fn aliases_sorted(mut aliases: Vec<dto::AliasView>) -> Vec<dto::AliasView> {
    aliases.sort_by(|a, b| a.alias.cmp(&b.alias));
    aliases
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct LocalModelQuery {
    id: Option<i64>,
    model_id: Option<String>,
    /// `chat` | `aux`; absent looks in chat first, then aux.
    target: Option<String>,
}

async fn local_model(State(st): State<SharedState>, Query(q): Query<LocalModelQuery>) -> Response {
    ops_result(
        crate::ops::local_model_get(&st, q.id, q.model_id.as_deref(), q.target.as_deref()).await,
    )
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct ModelIdQuery {
    model_id: Option<String>,
    target: Option<String>,
}

async fn local_model_check(
    State(st): State<SharedState>,
    Query(q): Query<ModelIdQuery>,
) -> Response {
    ops_result(crate::ops::local_model_check(&st, q.model_id.as_deref(), q.target.as_deref()).await)
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct SearchQuery {
    search: Option<String>,
    /// `chat` (default) | `aux` | `audio` — which class's models dir to list.
    target: Option<String>,
}

async fn gguf_files(State(st): State<SharedState>, Query(q): Query<SearchQuery>) -> Response {
    ops_result(crate::modelinfo::gguf_files(&st, q.search.as_deref(), q.target.as_deref()).await)
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct FlagsQuery {
    search: Option<String>,
    /// A configured model id (any class) or an image reference; absent reads
    /// the chat class image (§3.6 — the vocabulary is per image now).
    model: Option<String>,
}

async fn llama_flags(State(st): State<SharedState>, Query(q): Query<FlagsQuery>) -> Response {
    ops_result(crate::ops::llama_flags(&st, q.search.as_deref(), q.model.as_deref()).await)
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct InspectQuery {
    path: String,
    #[serde(default)]
    probe: bool,
    /// `chat` (default) | `aux` — which class's models dir `path` is under.
    target: Option<String>,
}

async fn model_inspect(State(st): State<SharedState>, Query(q): Query<InspectQuery>) -> Response {
    ops_result(crate::modelinfo::model_inspect(&st, &q.path, q.probe, q.target.as_deref()).await)
}

async fn local_model_plan(
    State(st): State<SharedState>,
    Query(q): Query<InspectQuery>,
) -> Response {
    ops_result(crate::modelinfo::local_model_plan(&st, &q.path, q.probe, q.target.as_deref()).await)
}

/// The editor's ladder table (ladder design §6): every rung shares cache
/// types, projector and drafter with the base row (§4.1), so these mirror
/// `LlamaParams`' own field names rather than nesting a whole row.
#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct RungPlanQuery {
    gguf_path: String,
    ctx_size: i64,
    cache_type_k: Option<String>,
    cache_type_v: Option<String>,
    mmproj_path: Option<String>,
    draft_gguf_path: Option<String>,
    n_gpu_layers: Option<i64>,
}

async fn ladder_rung_plan(
    State(st): State<SharedState>,
    Query(q): Query<RungPlanQuery>,
) -> Response {
    ops_result(
        crate::ops::ladder_rung_plan(
            &st,
            crate::ops::RungPlanInput {
                gguf_path: &q.gguf_path,
                ctx_size: q.ctx_size,
                cache_type_k: q.cache_type_k.as_deref(),
                cache_type_v: q.cache_type_v.as_deref(),
                mmproj_path: q.mmproj_path.as_deref(),
                draft_gguf_path: q.draft_gguf_path.as_deref(),
                n_gpu_layers: q.n_gpu_layers,
            },
        )
        .await,
    )
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct HfRepoQuery {
    repo: String,
    search: Option<String>,
    /// Which class's file kinds and role vocabulary to list; `chat` when the
    /// caller says nothing, which is every client written before the image
    /// class existed.
    target: Option<String>,
}

async fn hf_repo(State(st): State<SharedState>, Query(q): Query<HfRepoQuery>) -> Response {
    ops_result(
        crate::ops::hf_repo(
            &st,
            &q.repo,
            q.search.as_deref(),
            q.target.as_deref().unwrap_or("chat"),
        )
        .await,
    )
}

async fn hf_downloads(State(st): State<SharedState>) -> Response {
    ops_result(crate::ops::hf_downloads(&st).await)
}

/// Poll half of the jobs feed (§9c); the SSE `jobs` frame on `/api/events` is
/// the push half. `limit` is a request parameter with no default ceiling — the
/// table is bounded by the visible retention settings, not by a second cap
/// hidden in a handler.
#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default)]
pub(crate) struct JobsQuery {
    kind: Option<String>,
    active: Option<bool>,
    limit: Option<i64>,
}

async fn jobs(State(st): State<SharedState>, Query(q): Query<JobsQuery>) -> Response {
    ops_result(
        crate::ops::jobs_list(
            &st,
            q.kind.as_deref(),
            q.active.unwrap_or(false),
            q.limit.unwrap_or(0),
        )
        .await,
    )
}

/// audio.cpp's spec catalog, from the cached snapshot only — an empty
/// `fetched_at` means "never fetched here", and the refresh op is the only
/// thing that goes to the network (the old page's rule: a page load never
/// waits on GitHub).
async fn audio_catalog(State(st): State<SharedState>) -> Json<dto::AudioCatalog> {
    Json(audio::catalog(&st).await)
}

async fn upstreams(State(st): State<SharedState>) -> Response {
    match crate::store::list_upstreams(&st.db).await {
        Ok(rows) => {
            let upstreams = rows
                .into_iter()
                .map(|u| dto::UpstreamView {
                    id: u.id,
                    name: u.name,
                    protocol: u.protocol.as_str().to_string(),
                    kind: u.kind.as_str().to_string(),
                    base_url: u.base_url,
                    has_api_key: u.api_key.is_some(),
                    extra_headers: u
                        .extra_headers
                        .into_iter()
                        .map(|(k, _)| (k, "<set>".to_string()))
                        .collect(),
                    timeout_ms: u.timeout_ms,
                    enabled: u.enabled,
                    expose_all: u.expose_all,
                    expose_prefix: u.expose_prefix,
                    supports_responses: u.supports_responses,
                })
                .collect();
            Json(dto::UpstreamsResponse { upstreams }).into_response()
        }
        Err(e) => ops_result(Err(e.to_string())),
    }
}

/// The signal path per model, composed once server-side (`web::wiring`) and
/// shared verbatim with the old page — the client does no joining.
async fn wiring(State(st): State<SharedState>) -> Json<dto::WiringView> {
    Json(super::wiring::compose(&st).await)
}

async fn mcp_servers(State(st): State<SharedState>) -> Response {
    ops_result(crate::ops::mcp_servers(&st).await)
}

/// One server's exposed tools — what a Chat thread's MCP picker offers.
async fn mcp_server_tools(State(st): State<SharedState>, Path(id): Path<i64>) -> Response {
    ops_result(crate::ops::mcp_server_tools(&st, id).await)
}

/// `GET /api/tools` — the whole northbound tool surface: the built-in toolsets
/// and every registered server's tools, each with its source and why it is or
/// is not offered. The inventory behind the MCP page's per-tool switches, and
/// the list the Chat picker reads to offer the built-in toolsets.
async fn tools(State(st): State<SharedState>) -> Response {
    ops_result(crate::ops::tools(&st).await)
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct IdQuery {
    id: i64,
}

/// Live model catalog of one upstream (cached ~TTL server-side). Needs a
/// round-trip to the upstream, so it is its own endpoint rather than part of
/// the upstreams list.
async fn upstream_models(State(st): State<SharedState>, Query(q): Query<IdQuery>) -> Response {
    let snap = st.snapshot();
    let Some(u) = snap.upstreams.get(&q.id) else {
        return ops_result(Err(format!("no upstream with id {}", q.id)));
    };
    match crate::catalog::upstream_models(&st, u).await {
        Ok(models) => Json(dto::UpstreamModelsResponse {
            models: models.iter().map(|m| m.id.clone()).collect(),
            entries: models.into_iter().map(upstream_model_entry).collect(),
        })
        .into_response(),
        Err(e) => ops_result(Err(e)),
    }
}

/// The catalog facts the dashboard's catalog table shows, as parsed — no
/// field is filled in that the upstream did not publish.
fn upstream_model_entry(m: crate::catalog::ModelInfo) -> dto::UpstreamModelEntry {
    let (price_prompt, price_completion) = match m.pricing {
        Some(p) => (Some(p.prompt), Some(p.completion)),
        None => (None, None),
    };
    dto::UpstreamModelEntry {
        id: m.id,
        context_length: m.context_length,
        max_output_tokens: m.max_output_tokens,
        price_prompt,
        price_completion,
        created: m.created,
        input_modalities: m.input_modalities,
        task: m.task,
        tools: m.tools,
        reasoning_enabled: m.reasoning.as_ref().and_then(|r| r.enabled),
        reasoning: m.reasoning.map(|r| r.kind),
    }
}

// ---------------------------------------------------------------------------
// Stored /v1/responses conversations
// ---------------------------------------------------------------------------

async fn responses_index(State(st): State<SharedState>) -> Response {
    let chains = crate::store::list_response_chains(&st.db, 200)
        .await
        .unwrap_or_default();
    let (total_responses, total_chains) = crate::store::count_responses(&st.db)
        .await
        .unwrap_or((0, 0));
    let s = st.snapshot().settings.clone();
    let rows: Vec<Value> = chains
        .iter()
        .map(|c| {
            serde_json::json!({
                "chain_id": c.chain_id,
                "head_id": c.head_id,
                "model": c.model,
                "status": c.status,
                "responses": c.responses,
                "first_at": c.first_at,
                "last_at": c.last_at,
                "input_tokens": c.input_tokens,
                "output_tokens": c.output_tokens,
                "size": super::responses::human_bytes(c.bytes),
                "awaiting_approval": c.awaiting_approval,
            })
        })
        .collect();
    Json(serde_json::json!({
        "chains": rows,
        "total_chains": total_chains,
        "total_responses": total_responses,
        "store_enabled": s.responses_store,
        "retention_hours": s.responses_retention_hours,
        "max_chains": s.responses_max_chains,
        "rules": super::responses::describe_rules(
            s.responses_retention_hours,
            s.responses_max_chains,
        ),
        "capped": total_chains > 200,
    }))
    .into_response()
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct ChainQuery {
    id: String,
}

async fn responses_chain(State(st): State<SharedState>, Query(q): Query<ChainQuery>) -> Response {
    let rows = crate::store::list_chain_responses(&st.db, &q.id)
        .await
        .unwrap_or_default();
    if rows.is_empty() {
        return ops_result(Err("no such conversation".into()));
    }
    let responses: Vec<Value> = rows
        .iter()
        .map(|r| {
            let body: Value = serde_json::from_str(&r.body).unwrap_or(Value::Null);
            serde_json::json!({
                "id": r.id,
                "created_at": r.created_at,
                "status": r.status,
                "input_tokens": r.input_tokens,
                "output_tokens": r.output_tokens,
                "outline": super::responses::outline(&body),
                "text": crate::ingress::responses::output_text(&body),
                "pending": super::responses::pending_names(r.pending.as_deref()),
                "body": serde_json::to_string_pretty(&body).unwrap_or_default(),
            })
        })
        .collect();
    Json(serde_json::json!({ "chain_id": q.id, "responses": responses })).into_response()
}

// ---------------------------------------------------------------------------
// Mutations — one dispatcher over the ops plane
// ---------------------------------------------------------------------------

type Args = serde_json::Map<String, Value>;

fn arg_str<'a>(args: &'a Args, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// `POST /api/op/{name}` with the op's arguments as a JSON object (no body at
/// all is `{}`, [`super::op_body`]) — the same
/// vocabulary as the `lmgw__*` tool plane, minus the `self_admin` gate (this
/// is the human dashboard). Long-running ops (`local_model_test`, `container
/// apply/restart`) run inline; the UI shows their pending state.
async fn op(
    State(st): State<SharedState>,
    Path(name): Path<String>,
    super::op_body::OpBody(args): super::op_body::OpBody,
) -> Response {
    use super::op_names;
    use crate::ops::{self, patch_from_args};
    // The op plane's vocabulary, made explicit (api-docs design §4.7): a name
    // not in one of the four lists is unreachable, whatever the arms below —
    // or the two sibling dispatchers' — might otherwise have answered. Checked
    // first so the doc's coverage claim ("`/api/op/{name}` answers only the
    // names in `web::op_names`") is also what the gate does.
    if let Some(refusal) = op_names::refuse_unlisted(&name) {
        return ops_result(Err(refusal));
    }
    // The agent catalog's ops (agent-catalog §5) live next to the reads they
    // share their shaping with; everything else is dispatched here.
    if op_names::AGENT_OPS.contains(&name.as_str()) {
        // Two of them answer with a code of their own — a manifest write that
        // would take a DNS label it cannot have, or the gateway's own address
        // (origins §4.1). The rest render as the flat `op_failed` `ops_result`
        // produces, which is what `Refusal`'s `From<String>` is.
        return match super::api_agents::op(&st, &name, args).await {
            Ok(v) => Json(v).into_response(),
            Err(refusal) => refusal.into_response(),
        };
    }
    // The credential ops (principals §3.12), next to the key list they write
    // through, the way the agent catalog's ops are. Dispatched here rather
    // than in the table below because they answer with their own status and
    // code: an owner row's refusals are `refuse_owner`, not `ops_result`'s
    // flat `400 op_failed` for a malformed argument.
    if op_names::KEY_OPS.contains(&name.as_str()) {
        return match super::api_settings::key_op(&st, &name, args).await {
            Ok(v) => Json(v).into_response(),
            Err(refusal) => refusal.into_response(),
        };
    }
    // The dashboard's settings save, for the same reason: `agent_origin_suffix`
    // has a refusal of its own (`origin_suffix_shadows_gateway`, origins §4.1),
    // and everything else it answers is still the flat `op_failed`.
    if name == "settings_set_full" {
        return match patch_from_args(Some(args)) {
            Ok(p) => match super::api_settings::settings_set_full(&st, p).await {
                Ok(v) => Json(v).into_response(),
                Err(refusal) => refusal.into_response(),
            },
            Err(e) => ops_result(Err(e)),
        };
    }
    let res = match name.as_str() {
        "upstream_set" => match patch_from_args(Some(args)) {
            Ok(p) => ops::upstream_set(&st, p).await,
            Err(e) => Err(e),
        },
        "model_set" => match patch_from_args(Some(args)) {
            Ok(p) => ops::model_set(&st, p).await,
            Err(e) => Err(e),
        },
        "local_model_set" => match patch_from_args(Some(args)) {
            Ok(p) => ops::local_model_set(&st, p).await,
            Err(e) => Err(e),
        },
        // The candidate-alias editor (candidate-aliases design §6): create,
        // update, delete, enable, disable and the UI-only `preview` dry run
        // all go through the one op, exactly like every other `*_set`.
        "candidate_alias_set" => match patch_from_args(Some(args)) {
            Ok(p) => ops::candidate_alias_set(&st, p).await,
            Err(e) => Err(e),
        },
        "mcp_server_set" => match patch_from_args(Some(args)) {
            Ok(p) => ops::mcp_server_set(&st, p).await,
            Err(e) => Err(e),
        },
        "settings_set" => match patch_from_args(Some(args)) {
            Ok(p) => ops::settings_set(&st, p).await,
            Err(e) => Err(e),
        },
        // `embed_model_set` is the pre-rename spelling, still accepted so a
        // browser holding an older SPA bundle keeps working.
        "aux_model_set" | "embed_model_set" => match patch_from_args(Some(args)) {
            Ok(p) => ops::aux_model_set(&st, p).await,
            Err(e) => Err(e),
        },
        "audio_model_set" => match patch_from_args(Some(args)) {
            Ok(p) => audio_model_set(&st, p).await,
            Err(e) => Err(e),
        },
        // The image class's CRUD is ops-level rather than web-only
        // (image-generation design §8), so the dashboard and
        // `lmgw__image_model_set` post the same patch at the same function.
        "image_model_set" => match patch_from_args(Some(args)) {
            Ok(p) => ops::image_model_set(&st, p).await,
            Err(e) => Err(e),
        },
        "model_visibility" => match patch_from_args(Some(args)) {
            Ok(p) => model_visibility_set(&st, p).await,
            Err(e) => Err(e),
        },
        "alias_set" => match patch_from_args(Some(args)) {
            Ok(p) => alias_set(&st, p).await,
            Err(e) => Err(e),
        },
        "upstream_set_full" => match patch_from_args(Some(args)) {
            Ok(p) => upstream_set_full(&st, p).await,
            Err(e) => Err(e),
        },

        // The usage plane's four writes (usage-analytics §5). Three of them
        // have had working `ops` implementations since that design landed and
        // were reachable only from the tool plane, so the dashboard's Keys and
        // Prices cards posted ops that did not exist and failed on every Save.
        // The fourth, `key_set`, is dispatched above with the other four
        // credential ops.
        "price_set" => {
            let f = |k: &str| args.get(k).and_then(Value::as_f64);
            let scope_kind = arg_str(&args, "scope_kind").unwrap_or("alias").to_string();
            let scope_key = arg_str(&args, "scope_key").map(str::to_string);
            let note = arg_str(&args, "note").map(str::to_string);
            ops::price_set(
                &st,
                &scope_kind,
                scope_key.as_deref(),
                f("price_in"),
                f("price_out"),
                f("price_cache_read"),
                f("price_cache_write"),
                note.as_deref(),
            )
            .await
        }
        "price_delete" => match args.get("id").and_then(Value::as_i64) {
            Some(id) => ops::price_delete(&st, id).await,
            None => Err("pass id".to_string()),
        },
        "prices_sync" => ops::prices_sync(&st).await,
        "tool_set" => match (
            arg_str(&args, "name"),
            args.get("enabled").and_then(Value::as_bool),
        ) {
            (Some(name), Some(enabled)) => ops::tool_set(&st, name, enabled).await,
            _ => Err("pass name and enabled".to_string()),
        },
        "update_check" => super::api_settings::update_check(&st).await,
        "job_cancel" => match args.get("id").and_then(Value::as_i64) {
            Some(id) => ops::job_cancel(&st, id).await,
            None => Err("pass id".to_string()),
        },
        "response_chain_delete" => match arg_str(&args, "chain_id") {
            Some(chain_id) => match crate::store::delete_response_chain(&st.db, chain_id).await {
                Ok(n) => Ok(serde_json::json!({
                    "ok": true,
                    "message": format!("deleted {n} response(s)"),
                })),
                Err(e) => Err(e.to_string()),
            },
            None => Err("pass chain_id".to_string()),
        },
        "responses_gc" => {
            let scope = arg_str(&args, "scope").unwrap_or("rules");
            if scope == "all" {
                let chains = crate::store::list_response_chains(&st.db, i64::MAX)
                    .await
                    .unwrap_or_default();
                let mut n = 0;
                for c in &chains {
                    n += crate::store::delete_response_chain(&st.db, &c.chain_id)
                        .await
                        .unwrap_or(0);
                }
                Ok(serde_json::json!({
                    "ok": true,
                    "message": format!("cleared {n} stored response(s)"),
                }))
            } else {
                let s = st.snapshot().settings.clone();
                match crate::store::gc_responses(
                    &st.db,
                    s.responses_retention_hours,
                    s.responses_max_chains,
                )
                .await
                {
                    Ok(0) => Ok(serde_json::json!({ "ok": true, "message": "nothing to evict" })),
                    Ok(n) => Ok(serde_json::json!({
                        "ok": true,
                        "message": format!("evicted {n} response(s)"),
                    })),
                    Err(e) => Err(e.to_string()),
                }
            }
        }
        "upstream_test" => match args.get("id").and_then(Value::as_i64) {
            None => Err("pass id".to_string()),
            Some(id) => match crate::store::get_upstream(&st.db, id).await {
                Err(e) => Err(e.to_string()),
                Ok(None) => Err(format!("no upstream with id {id}")),
                Ok(Some(u)) => match super::admin::test_upstream(&st, &u).await {
                    Ok(detail) => Ok(serde_json::json!({ "ok": true, "detail": detail })),
                    Err(e) => Err(format!("connection test failed — {e}")),
                },
            },
        },
        "hf_add" => {
            let repo = arg_str(&args, "repo").unwrap_or_default().to_string();
            let target = arg_str(&args, "target").unwrap_or("chat").to_string();
            let file = arg_str(&args, "file").map(str::to_string);
            let quant = arg_str(&args, "quant").map(str::to_string);
            let companions = args
                .get("companions")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            ops::hf_add(
                &st,
                &repo,
                file.as_deref(),
                quant.as_deref(),
                &target,
                companions,
            )
            .await
        }
        "hf_set" => {
            let action = arg_str(&args, "action").unwrap_or_default().to_string();
            let target = arg_str(&args, "target").unwrap_or("chat").to_string();
            let id = args.get("id").and_then(Value::as_i64);
            ops::hf_set(&st, &action, id, &target).await
        }
        "container" => {
            let target = arg_str(&args, "target").map(str::to_string);
            let model = arg_str(&args, "model").map(str::to_string);
            let action = arg_str(&args, "action").unwrap_or_default().to_string();
            let force = args
                .get("override")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let tail = args.get("tail").and_then(Value::as_i64);
            ops::container(
                &st,
                target.as_deref(),
                model.as_deref(),
                &action,
                force,
                tail,
            )
            .await
        }
        // The GPU hold (gpu-hold design §6). An op, not a settings patch:
        // engaging it stops containers, which a generic settings save must not
        // start doing.
        "hold_set" => match args.get("active").and_then(Value::as_bool) {
            Some(active) => ops::hold_set(&st, active).await,
            None => Err("pass active: true or false".into()),
        },
        "audio_catalog" => audio_catalog_op(&st, &args).await,
        // "Add from recipe" for the image class (image-generation design
        // §7.2), the audio catalog's sibling with a compiled-in source.
        // Shared with `lmgw__image_recipes` / `lmgw__image_recipe_add`.
        "image_recipes" => ops::image_recipes(&st).await,
        "image_recipe_add" => {
            let key = arg_str(&args, "key").unwrap_or_default();
            ops::image_recipe_add(&st, key, arg_str(&args, "diffusion_file")).await
        }
        "local_model_test" => {
            let model_id = arg_str(&args, "model_id").unwrap_or_default().to_string();
            let target = arg_str(&args, "target").map(String::from);
            crate::modelinfo::local_model_test(&st, &model_id, target.as_deref()).await
        }
        // The Backends page (container-builds §15): each op takes its §15
        // argument type and answers with its §15 response type. Cancel is
        // `job_cancel` on the run's job, above.
        "builds" => ops::backends::to_json(ops::backends::builds(&st).await),
        "build_get" => typed(args, |a| ops::backends::build_get(&st, a)).await,
        "build_set" => typed(args, |a| ops::backends::build_set(&st, a)).await,
        "build_resolve" => typed(args, |a| ops::backends::build_resolve(&st, a)).await,
        "build_check_merge" => typed(args, |a| ops::backends::build_check_merge(&st, a)).await,
        "build_run" => {
            typed(args, |a| {
                ops::backends::build_run(&st, a, lmgw_api_types::builds::BuildTrigger::Manual)
            })
            .await
        }
        "build_run_log" => typed(args, |a| ops::backends::build_run_log(&st, a)).await,
        "build_promote" => typed(args, |a| ops::backends::build_promote(&st, a)).await,
        "build_verify" => typed(args, |a| ops::backends::build_verify(&st, a)).await,
        "build_env" => ops::backends::to_json(ops::backends::build_env(&st).await),
        "forge_refs" => typed(args, |a| ops::backends::forge_refs(&st, a)).await,
        "forge_prs" => typed(args, |a| ops::backends::forge_prs(&st, a)).await,
        "forge_pr" => typed(args, |a| ops::backends::forge_pr(&st, a)).await,
        "container_images" => typed(args, |a| ops::backends::container_images(&st, a)).await,
        "container_image_delete" => {
            typed(args, |a| ops::backends::container_image_delete(&st, a)).await
        }
        "container_image_tag" => typed(args, |a| ops::backends::container_image_tag(&st, a)).await,
        // Update detection (container-builds §8): "Check now", and "Pull
        // update" for a registry image. Neither recreates a container.
        "build_updates_check" => typed(args, |a| ops::backends::build_updates_check(&st, a)).await,
        "container_image_pull" => {
            typed(args, |a| ops::backends::container_image_pull(&st, a)).await
        }
        "container_image_pull_status" => {
            typed(args, |a| ops::backends::container_image_pull_status(&st, a)).await
        }
        // Benchmarks (benchmark design §8.1): typed like the Backends ops.
        // Cancel is its own op (not `job_cancel`), so a caller never needs
        // the run's job id.
        "bench_plan" => typed(args, |a| ops::bench::bench_plan(&st, a)).await,
        "bench_start" => typed(args, |a| ops::bench::bench_start(&st, a)).await,
        "bench_runs" => typed(args, |a| ops::bench::bench_runs(&st, a)).await,
        "bench_run" => typed(args, |a| ops::bench::bench_run(&st, a)).await,
        "bench_cancel" => typed(args, |a| ops::bench::bench_cancel(&st, a)).await,
        "bench_run_set" => typed(args, |a| ops::bench::bench_run_set(&st, a)).await,
        "bench_delete" => typed(args, |a| ops::bench::bench_delete(&st, a)).await,
        other => Err(format!("unknown op '{other}'")),
    };
    ops_result(res)
}

/// One typed op: its arguments deserialized into `A` (a malformed body is the
/// usual `op_failed` with serde's sentence), its answer `T` serialized back.
async fn typed<A, T, F, Fut>(args: Args, f: F) -> Result<Value, String>
where
    A: serde::de::DeserializeOwned,
    T: serde::Serialize,
    F: FnOnce(A) -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let a: A = crate::ops::patch_from_args(Some(args))?;
    crate::ops::backends::to_json(f(a).await)
}

/// The two catalog actions of the old Audio page, as one op:
/// - `refresh` — live-fetch `model_specs/*.json` and persist the snapshot
///   (memory + kv). The only network call in this domain.
/// - `download` — queue one package's files (`family` + `package`) through the
///   shared HF download queue, target `audio`. Fire-and-forget: progress is
///   read back from `GET /api/hf/downloads`, and the audio model row stays a
///   deliberate second step (the editor, prefilled from the package).
async fn audio_catalog_op(st: &SharedState, args: &Args) -> Result<Value, String> {
    match arg_str(args, "action").unwrap_or_default() {
        "refresh" => {
            let snapshot = audio::catalog_refresh(st).await?;
            Ok(serde_json::json!({
                "ok": true,
                "families": snapshot.specs.len(),
                "fetched_at": snapshot.fetched_at,
                "message": format!("catalog refreshed — {} families", snapshot.specs.len()),
            }))
        }
        "download" => {
            let family = arg_str(args, "family").unwrap_or_default().trim();
            let package = arg_str(args, "package").unwrap_or_default().trim();
            if family.is_empty() || package.is_empty() {
                return Err("pass family and package".into());
            }
            let install = audio::catalog_download(st, family, package).await?;
            serde_json::to_value(install).map_err(|e| e.to_string())
        }
        other => Err(format!("unknown action '{other}' (refresh, download)")),
    }
}

/// Upstream create/update including `extra_headers`, which the tool-plane
/// patch omits. Secrets follow the house convention: an empty api_key keeps
/// the stored one, and a header value of "<set>" round-trips to the stored
/// value for that header name.
#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct UpstreamFullPatch {
    action: String,
    id: Option<i64>,
    name: Option<String>,
    protocol: Option<String>,
    kind: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    /// (name, value) pairs, replacing the stored set. A secret as a whole:
    /// these are typically the provider's own auth headers.
    #[schemars(transform = lmgw_api_types::openapi_ext::secret)]
    extra_headers: Option<Vec<(String, String)>>,
    timeout_ms: Option<u64>,
    enabled: Option<bool>,
    expose_all: Option<bool>,
    expose_prefix: Option<String>,
    supports_responses: Option<bool>,
}

async fn upstream_set_full(st: &SharedState, p: UpstreamFullPatch) -> Result<Value, String> {
    use crate::config::{Protocol, UpstreamKind};
    use crate::store;
    let protocol = |s: &Option<String>, cur: Protocol| -> Result<Protocol, String> {
        match s.as_deref() {
            None | Some("") => Ok(cur),
            Some(v) => Protocol::parse(v).ok_or_else(|| format!("unknown protocol '{v}'")),
        }
    };
    let kind = |s: &Option<String>, cur: UpstreamKind| -> Result<UpstreamKind, String> {
        match s.as_deref() {
            None | Some("") => Ok(cur),
            Some(v) => UpstreamKind::parse(v).ok_or_else(|| format!("unknown kind '{v}'")),
        }
    };
    match p.action.as_str() {
        "create" => {
            let name = p
                .name
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or("create requires name")?;
            let base_url = p
                .base_url
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or("create requires base_url")?;
            let new = store::NewUpstream {
                name: name.clone(),
                protocol: protocol(&p.protocol, Protocol::Openai)?,
                kind: kind(&p.kind, UpstreamKind::Generic)?,
                base_url,
                api_key: p.api_key.clone().filter(|k| !k.is_empty()),
                extra_headers: p.extra_headers.clone().unwrap_or_default(),
                timeout_ms: p.timeout_ms.unwrap_or(120_000),
                enabled: p.enabled.unwrap_or(true),
                expose_all: p.expose_all.unwrap_or(false),
                expose_prefix: p
                    .expose_prefix
                    .clone()
                    .unwrap_or_default()
                    .trim_matches('/')
                    .to_string(),
                supports_responses: p.supports_responses.unwrap_or(false),
            };
            let id = store::insert_upstream(&st.db, &new)
                .await
                .map_err(|e| e.to_string())?;
            st.reload_snapshot().await.map_err(|e| e.to_string())?;
            st.catalog.invalidate(id).await;
            Ok(
                serde_json::json!({ "ok": true, "id": id, "message": format!("upstream '{name}' created") }),
            )
        }
        "update" => {
            let id = p.id.ok_or("pass id")?;
            let cur = store::get_upstream(&st.db, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no upstream with id {id}"))?;
            let new_key = p.api_key.clone().filter(|k| !k.is_empty());
            let update_key = new_key.is_some();
            let headers = match p.extra_headers.clone() {
                None => cur.extra_headers.clone(),
                Some(hs) => hs
                    .into_iter()
                    .map(|(k, v)| {
                        if v == "<set>" {
                            let stored = cur
                                .extra_headers
                                .iter()
                                .find(|(ck, _)| *ck == k)
                                .map(|(_, cv)| cv.clone())
                                .unwrap_or_default();
                            (k, stored)
                        } else {
                            (k, v)
                        }
                    })
                    .collect(),
            };
            let upd = store::NewUpstream {
                name: p
                    .name
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(cur.name),
                protocol: protocol(&p.protocol, cur.protocol)?,
                kind: kind(&p.kind, cur.kind)?,
                base_url: p
                    .base_url
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(cur.base_url),
                api_key: new_key,
                extra_headers: headers,
                timeout_ms: p.timeout_ms.unwrap_or(cur.timeout_ms),
                enabled: p.enabled.unwrap_or(cur.enabled),
                expose_all: p.expose_all.unwrap_or(cur.expose_all),
                expose_prefix: p
                    .expose_prefix
                    .clone()
                    .map(|s| s.trim_matches('/').to_string())
                    .unwrap_or(cur.expose_prefix),
                supports_responses: p.supports_responses.unwrap_or(cur.supports_responses),
            };
            store::update_upstream(&st.db, id, &upd, update_key)
                .await
                .map_err(|e| e.to_string())?;
            st.reload_snapshot().await.map_err(|e| e.to_string())?;
            st.catalog.invalidate(id).await;
            Ok(serde_json::json!({ "ok": true, "id": id, "message": "upstream updated" }))
        }
        other => Err(format!("unknown action '{other}' (create, update)")),
    }
}

/// Alias create/update including param overrides, which `ops::model_set`
/// does not carry (its tool-plane schema stays scalar-only). Overrides are
/// replaced wholesale — the form always sends the complete set.
#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AliasFullPatch {
    action: String,
    id: Option<i64>,
    alias: Option<String>,
    upstream_id: Option<i64>,
    upstream_model: Option<String>,
    enabled: Option<bool>,
    overrides: Option<serde_json::Value>,
    /// Owner override of the derived `/v1/models` capability facts
    /// (model-capabilities design §7) — see
    /// `ops::parse_capabilities_override`, which this field is handed to
    /// verbatim. A JSON `null` is indistinguishable from an absent field
    /// once serde has parsed it (both deserialise to `None`), so — like
    /// `chat_template_kwargs` on the local model form — the form sends an
    /// empty *string* to clear, never `null`; `parse_capabilities_override`
    /// reads that string as "clear". `None` here (the field truly absent)
    /// leaves the stored value as is.
    capabilities_override: Option<serde_json::Value>,
}

async fn alias_set(st: &SharedState, p: AliasFullPatch) -> Result<Value, String> {
    use crate::store;
    let overrides: crate::ir::Params = match &p.overrides {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("bad overrides: {e}"))?,
        None => Default::default(),
    };
    // `crate::ops::parse_capabilities_override` accepts an object or a JSON
    // string containing one and shape-checks it; an empty string / `null`
    // clears (design §7).
    let capabilities_override_patch = match &p.capabilities_override {
        Some(v) => Some(crate::ops::parse_capabilities_override(v)?),
        None => None,
    };
    let upstream_ok = |id: i64| async move {
        store::get_upstream(&st.db, id)
            .await
            .map_err(|e| e.to_string())?
            .map(|_| id)
            .ok_or_else(|| format!("no upstream with id {id}"))
    };
    match p.action.as_str() {
        "create" => {
            let alias = p
                .alias
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or("create requires alias")?;
            let upstream_id =
                upstream_ok(p.upstream_id.ok_or("create requires upstream_id")?).await?;
            let new = store::NewAlias {
                alias: alias.clone(),
                upstream_id,
                upstream_model_id: p.upstream_model.clone().unwrap_or_default(),
                param_overrides: overrides,
                enabled: p.enabled.unwrap_or(true),
                capabilities_override: capabilities_override_patch.flatten(),
            };
            let id = store::insert_alias(&st.db, &new)
                .await
                .map_err(|e| e.to_string())?;
            st.reload_snapshot().await.map_err(|e| e.to_string())?;
            Ok(
                serde_json::json!({ "ok": true, "id": id, "message": format!("alias '{alias}' created") }),
            )
        }
        "update" => {
            let id = p.id.ok_or("pass id")?;
            let all = store::list_aliases(&st.db)
                .await
                .map_err(|e| e.to_string())?;
            let cur = all
                .into_iter()
                .find(|a| a.id == id)
                .ok_or_else(|| format!("no alias with id {id}"))?;
            let upstream_id = match p.upstream_id {
                Some(uid) => upstream_ok(uid).await?,
                None => cur.upstream_id,
            };
            let upd = store::NewAlias {
                alias: p
                    .alias
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(cur.alias),
                upstream_id,
                upstream_model_id: p.upstream_model.clone().unwrap_or(cur.upstream_model_id),
                param_overrides: if p.overrides.is_some() {
                    overrides
                } else {
                    cur.param_overrides
                },
                enabled: p.enabled.unwrap_or(cur.enabled),
                capabilities_override: match capabilities_override_patch {
                    Some(v) => v,
                    None => cur.capabilities_override,
                },
            };
            store::update_alias(&st.db, id, &upd)
                .await
                .map_err(|e| e.to_string())?;
            st.reload_snapshot().await.map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "ok": true, "id": id, "message": "alias updated" }))
        }
        other => Err(format!("unknown action '{other}' (create, update)")),
    }
}

/// Audio model CRUD — same shape as `ops::aux_model_set` (sparse patch,
/// unknown fields rejected, `reload_snapshot()` after every write, apply stays
/// separate), adapted to the wider audio.cpp field set. Validation lives in
/// `crate::web::audio`, next to the container it protects: a task from the
/// fixed 13-value list, every voice preset naming
/// a `voice_id`/`voice_ref`, a bare `default_voice_preset` resolving to a
/// declared preset, and every `voice_ref` under `/models/...` existing on
/// disk — audiocpp_server opens reference clips at startup and exits if one
/// is missing.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AudioPatch {
    action: String,
    id: Option<i64>,
    model_id: Option<String>,
    family: Option<String>,
    path: Option<String>,
    task: Option<String>,
    mode: Option<String>,
    load_options: Option<serde_json::Map<String, Value>>,
    session_options: Option<serde_json::Map<String, Value>>,
    /// Request-option defaults for every call to this model (audio.cpp's
    /// `default_request_options`); a request that names one still wins.
    default_request_options: Option<serde_json::Map<String, Value>>,
    /// Per-model override of the class's `lazy_load`. `None` keeps the current
    /// value; name `lazy` in `clear` to go back to inheriting the class.
    lazy: Option<bool>,
    /// Per-model `busy_timeout_ms` (and the ceiling a request's own value is
    /// clamped to), in ms; `0` means "wait forever". Same clear convention as
    /// `lazy`.
    busy_timeout_ms: Option<i64>,
    /// A `<family>.json` spec, or a directory of them, relative to the audio
    /// models dir — empty clears back to the image's own catalog.
    model_spec_override: Option<String>,
    /// Named config/weights asset ids for a model directory holding several;
    /// empty clears.
    config_id: Option<String>,
    weight_id: Option<String>,
    voice_presets: Option<serde_json::Map<String, Value>>,
    /// `None` = not supplied (update keeps the current value); an explicit
    /// blank string clears it back to `None`, the same "empty clears"
    /// convention `AuxPatch::pooling` uses for a plain string field.
    default_voice_preset: Option<Value>,
    enabled: Option<bool>,
    /// Per-model container image override (per-model-containers design §3.1);
    /// empty clears back to inheriting the audio class settings' image.
    image: Option<String>,
    /// `podman run` args override (§3.1); replaces the current value
    /// wholesale when supplied. `None` keeps the current value; to revert to
    /// inheriting the class settings, name `extra_run_args` in `clear` (same
    /// convention as `AuxPatch::clear`).
    extra_run_args: Option<Vec<String>>,
    /// Start this model's own container at app launch (§3.4).
    warm_start: Option<bool>,
    /// GPU-hold fallback mode (gpu-hold design §2/§3.2): `inherit` (default,
    /// means "no fallback" for audio) | `none` | `alias`.
    hold_fallback_mode: Option<String>,
    /// The alias `hold_fallback_mode = "alias"` routes a held request to.
    hold_fallback: Option<String>,
    /// Field names to reset to unset, comma- or space-separated — see
    /// `AuxPatch::clear`.
    clear: Option<String>,
}

impl Default for AudioPatch {
    fn default() -> Self {
        AudioPatch {
            action: String::new(),
            id: None,
            model_id: None,
            family: None,
            path: None,
            task: None,
            mode: None,
            load_options: None,
            session_options: None,
            default_request_options: None,
            lazy: None,
            busy_timeout_ms: None,
            model_spec_override: None,
            config_id: None,
            weight_id: None,
            voice_presets: None,
            default_voice_preset: None,
            enabled: None,
            image: None,
            extra_run_args: None,
            warm_start: None,
            hold_fallback_mode: None,
            hold_fallback: None,
            clear: None,
        }
    }
}

fn check_task(task: &str) -> Result<(), String> {
    if audio::AUDIO_TASKS.contains(&task) {
        Ok(())
    } else {
        Err(format!(
            "unknown task '{task}' (expected one of: {})",
            audio::AUDIO_TASKS.join(", ")
        ))
    }
}

/// A negative `busy_timeout_ms` is rejected by audiocpp_server **at startup**,
/// so saving one would trade an error message here for a container that never
/// comes up. `0` is legal and means "wait forever" — the engine's own way of
/// disabling the guard.
fn check_busy_timeout(ms: Option<i64>) -> Result<Option<i64>, String> {
    match ms {
        Some(v) if v < 0 => {
            Err("busy timeout: must be 0 or more milliseconds (0 = wait forever, no guard)".into())
        }
        other => Ok(other),
    }
}

/// The spec override is a path under the audio models dir, checked the same
/// way the model path is: it is mounted into the container, and `..` out of
/// the mount would name a file the container cannot read anyway.
fn check_spec_override(path: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = path
        .map(|p| p.trim().trim_matches('/'))
        .filter(|p| !p.is_empty())
    else {
        return Ok(None);
    };
    crate::ops::check_rel_path(raw, "audio models dir")?;
    Ok(Some(raw.to_string()))
}

/// Every preset value needs a `voice_id`/`voice_ref`; a `default_voice_preset`
/// naming a bare preset must resolve to one of them — the rules themselves are
/// `crate::web::audio`'s, applied here over the already-typed JSON map.
fn check_presets(
    presets: &serde_json::Map<String, Value>,
    default: Option<&Value>,
) -> Result<(), String> {
    for (name, p) in presets {
        let obj = p
            .as_object()
            .ok_or_else(|| format!("voice preset '{name}': must be a JSON object"))?;
        audio::check_voice_preset(&format!("voice preset '{name}'"), obj)?;
    }
    if let Some(v) = default {
        audio::check_default_voice_preset(v, presets)?;
    }
    Ok(())
}

/// `Some("")` clears `default_voice_preset` back to `None` on update; `None`
/// (the field absent from the patch) keeps the current value.
fn overlay_default_preset(p: &Option<Value>, cur: Option<Value>) -> Option<Value> {
    match p {
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(v) => Some(v.clone()),
        None => cur,
    }
}

async fn audio_model_set(st: &SharedState, p: AudioPatch) -> Result<Value, String> {
    use crate::store;
    let snap = st.snapshot();
    let reload = || async { st.reload_snapshot().await.map_err(|e| e.to_string()) };
    match p.action.as_str() {
        "create" => {
            let model_id = p
                .model_id
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or("create requires model_id")?;
            crate::ops::refuse_if_candidate_alias_name(&snap, &snap.audio_public_name(&model_id))?;
            let family = p
                .family
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or("create requires family")?;
            let path = p
                .path
                .as_deref()
                .map(|s| s.trim().trim_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .ok_or("create requires path")?;
            crate::ops::check_rel_path(&path, "audio models dir")?;
            let task = p
                .task
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or("create requires task")?;
            check_task(&task)?;
            let voice_presets = p.voice_presets.clone().unwrap_or_default();
            // Same "blank clears" convention `overlay_default_preset` applies on
            // update: the Leptos form always supplies this field, so an explicit
            // `Some("")` (no default chosen) must not be validated as a literal
            // empty-named preset — there is no "current" value to fall back to on
            // create, so the overlay's `cur` is unconditionally `None`.
            let default_voice_preset = overlay_default_preset(&p.default_voice_preset, None);
            check_presets(&voice_presets, default_voice_preset.as_ref())?;
            let (hold_fallback_mode, hold_fallback) = crate::ops::resolve_hold_fallback_text(
                &st.snapshot(),
                p.hold_fallback_mode.as_deref(),
                p.hold_fallback.as_deref(),
                p.clear.as_deref(),
                (HoldFallbackMode::Inherit, None),
            )?;
            let new = store::NewAudioModel {
                model_id: model_id.clone(),
                family,
                path,
                task,
                mode: match p.mode.as_deref() {
                    Some("streaming") => "streaming".to_string(),
                    _ => "offline".to_string(),
                },
                lazy: p.lazy,
                busy_timeout_ms: check_busy_timeout(p.busy_timeout_ms)?,
                load_options: p.load_options.clone().unwrap_or_default(),
                session_options: p.session_options.clone().unwrap_or_default(),
                default_request_options: p.default_request_options.clone().unwrap_or_default(),
                model_spec_override: check_spec_override(p.model_spec_override.as_deref())?,
                config_id: p.config_id.clone().filter(|s| !s.trim().is_empty()),
                weight_id: p.weight_id.clone().filter(|s| !s.trim().is_empty()),
                voice_presets,
                default_voice_preset,
                enabled: p.enabled.unwrap_or(true),
                image: p.image.clone().filter(|s| !s.is_empty()),
                extra_run_args: p.extra_run_args.clone(),
                warm_start: p.warm_start.unwrap_or(false),
                hold_fallback_mode,
                hold_fallback,
            };
            audio::check_voice_refs(st, &new)?;
            let id = store::insert_audio_model(&st.db, &new)
                .await
                .map_err(|e| e.to_string())?;
            reload().await?;
            Ok(serde_json::json!({
                "ok": true, "id": id,
                "message": format!("audio model '{model_id}' created — start it to serve it"),
            }))
        }
        "update" | "enable" | "disable" => {
            let id = p.id.ok_or("pass id")?;
            let cur = store::get_audio_model(&st.db, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no audio model with id {id}"))?;
            // See `aux_model_set`: the running container is named after the
            // pre-patch id.
            let cur_model_id = cur.model_id.clone();
            let path = match p.path.as_deref() {
                Some(s) => s.trim().trim_matches('/').to_string(),
                None => cur.path,
            };
            crate::ops::check_rel_path(&path, "audio models dir")?;
            let task = p.task.clone().unwrap_or(cur.task);
            check_task(&task)?;
            let voice_presets = p.voice_presets.clone().unwrap_or(cur.voice_presets);
            let default_voice_preset =
                overlay_default_preset(&p.default_voice_preset, cur.default_voice_preset);
            check_presets(&voice_presets, default_voice_preset.as_ref())?;
            let enabled = match p.action.as_str() {
                "enable" => true,
                "disable" => false,
                _ => p.enabled.unwrap_or(cur.enabled),
            };
            let (hold_fallback_mode, hold_fallback) = crate::ops::resolve_hold_fallback_text(
                &st.snapshot(),
                p.hold_fallback_mode.as_deref(),
                p.hold_fallback.as_deref(),
                p.clear.as_deref(),
                (cur.hold_fallback_mode, cur.hold_fallback.clone()),
            )?;
            let new_audio_model_id = p
                .model_id
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(cur.model_id);
            crate::ops::refuse_if_candidate_alias_name(
                &snap,
                &snap.audio_public_name(&new_audio_model_id),
            )?;
            let upd = store::NewAudioModel {
                model_id: new_audio_model_id,
                family: p
                    .family
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(cur.family),
                path,
                task,
                mode: match p.mode.as_deref() {
                    Some("streaming") => "streaming".to_string(),
                    Some(_) => "offline".to_string(),
                    None => cur.mode,
                },
                lazy: if crate::ops::clear_has(p.clear.as_deref(), "lazy") {
                    None
                } else {
                    p.lazy.or(cur.lazy)
                },
                busy_timeout_ms: if crate::ops::clear_has(p.clear.as_deref(), "busy_timeout_ms") {
                    None
                } else {
                    check_busy_timeout(p.busy_timeout_ms)?.or(cur.busy_timeout_ms)
                },
                load_options: p.load_options.clone().unwrap_or(cur.load_options),
                session_options: p.session_options.clone().unwrap_or(cur.session_options),
                default_request_options: p
                    .default_request_options
                    .clone()
                    .unwrap_or(cur.default_request_options),
                model_spec_override: match p.model_spec_override.as_deref() {
                    Some(s) if s.trim().is_empty() => None,
                    Some(_) => check_spec_override(p.model_spec_override.as_deref())?,
                    None => cur.model_spec_override,
                },
                config_id: match p.config_id.clone() {
                    Some(s) if s.trim().is_empty() => None,
                    Some(s) => Some(s),
                    None => cur.config_id,
                },
                weight_id: match p.weight_id.clone() {
                    Some(s) if s.trim().is_empty() => None,
                    Some(s) => Some(s),
                    None => cur.weight_id,
                },
                voice_presets,
                default_voice_preset,
                enabled,
                image: match p.image.clone() {
                    Some(s) if s.is_empty() => None,
                    Some(s) => Some(s),
                    None => cur.image,
                },
                extra_run_args: if crate::ops::clear_has(p.clear.as_deref(), "extra_run_args") {
                    None
                } else {
                    p.extra_run_args.clone().or(cur.extra_run_args)
                },
                warm_start: p.warm_start.unwrap_or(cur.warm_start),
                hold_fallback_mode,
                hold_fallback,
            };
            audio::check_voice_refs(st, &upd)?;
            store::update_audio_model(&st.db, id, &upd)
                .await
                .map_err(|e| e.to_string())?;
            reload().await?;
            // Container hygiene (per-model-containers §3.4/§3.6). Audio's
            // per-model `server.json` is rendered fresh at every start, so
            // stopping the container *is* the apply. A rename is the exception
            // (see the chat path in `ops::local_model_set`): the old container
            // — and the old per-model config dir it mounts, which is keyed on
            // the model id too — belong to a model that no longer exists.
            let note = if upd.model_id != cur_model_id {
                lifecycle::drop_model(st, Class::Audio, &cur_model_id).await;
                Some(format!(
                    "it was renamed from '{cur_model_id}', so that container was stopped and \
                     removed — the next request starts '{}' fresh",
                    upd.model_id
                ))
            } else if enabled {
                lifecycle::stop_for_apply(st, Class::Audio, &cur_model_id).await
            } else {
                lifecycle::drop_model(st, Class::Audio, &cur_model_id).await;
                Some("it is disabled — its container was stopped and removed".to_string())
            };
            Ok(serde_json::json!({
                "ok": true, "id": id,
                "message": match note {
                    Some(note) => format!("audio model updated — {note}"),
                    None => "audio model updated — it is not running, so the next request starts                              it with the new configuration"
                        .to_string(),
                },
            }))
        }
        "delete" => {
            let id = p.id.ok_or("pass id")?;
            let cur = store::get_audio_model(&st.db, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no audio model with id {id}"))?;
            store::delete_audio_model(&st.db, id)
                .await
                .map_err(|e| e.to_string())?;
            reload().await?;
            // §3.4: immediately, not at the next boot.
            lifecycle::drop_model(st, Class::Audio, &cur.model_id).await;
            Ok(serde_json::json!({
                "ok": true,
                "message": "audio model deleted — its container was stopped and removed",
            }))
        }
        other => Err(format!(
            "unknown action '{other}' (create, update, delete, enable, disable)"
        )),
    }
}

/// Hide/unhide one model of an `expose_all` upstream's live catalog from
/// `/v1/models` and the client-facing model list (`server::exposed_model_names`
/// filters on the same `hidden_passthrough` set this writes). The model itself
/// is never configured here — it is whatever the upstream's catalog reports —
/// so there is no `id`, only the `(upstream_id, model_id)` pair that names one
/// entry of a live catalog.
#[derive(Deserialize, Default, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ModelVisibilityPatch {
    action: String,
    upstream_id: Option<i64>,
    model_id: Option<String>,
    /// Several entries of the same catalog at once ("hide the 17 shown"):
    /// one transaction and one snapshot reload instead of 17 of each.
    /// Combines with `model_id`.
    model_ids: Option<Vec<String>>,
}

async fn model_visibility_set(st: &SharedState, p: ModelVisibilityPatch) -> Result<Value, String> {
    use crate::store;
    let upstream_id = p.upstream_id.ok_or("pass upstream_id")?;
    let mut ids: Vec<String> = Vec::new();
    for id in p
        .model_id
        .into_iter()
        .chain(p.model_ids.into_iter().flatten())
    {
        let id = id.trim().to_string();
        if !id.is_empty() && !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err("pass model_id or model_ids".into());
    }
    let hide = match p.action.as_str() {
        "hide" => true,
        "unhide" => false,
        other => return Err(format!("unknown action '{other}' (hide, unhide)")),
    };
    store::set_passthrough_hidden(&st.db, upstream_id, &ids, hide)
        .await
        .map_err(|e| e.to_string())?;
    st.reload_snapshot().await.map_err(|e| e.to_string())?;
    let what = match ids.as_slice() {
        [one] => format!("'{one}'"),
        many => format!("{} models", many.len()),
    };
    let message = if hide {
        format!("{what} hidden from the client-facing model list")
    } else {
        format!("{what} restored to the client-facing model list")
    };
    Ok(serde_json::json!({ "ok": true, "message": message, "count": ids.len() }))
}

/// How often a `stats` frame is refreshed *while a request is in flight*, so
/// the titlebar's live tok/s readout moves at a readable pace. At idle nothing
/// is sent at all — this app sits open all day, and a quiet gateway must mean a
/// quiet stream.
const RATE_TICK: std::time::Duration = std::time::Duration::from_millis(500);

/// The dashboard's live event stream. Frames:
/// - `stats` — `StatsView`; sent on connect, after every finished request, and
///   (while `active_requests > 0`) every [`RATE_TICK`] so `tok_s` stays live.
/// - `request` — `RequestSummary`; one per finished request/tool call.
/// - `runtime` — every per-model container lmgw believes is up (§3.2); on
///   connect and whenever the registry changes.
/// - `mcp` — full `McpStatusView` list; on connect and on any MCP status
///   change.
/// - `vram` — the GPU ledger + admission queue (§9b); on connect, whenever a
///   request starts or stops waiting for memory, and on the same 5 s tick
///   (free memory moves without lmgw doing anything).
async fn events(
    State(st): State<SharedState>,
) -> Sse<impl Stream<Item = Result<SseFrame, String>>> {
    fn frame<T: serde::Serialize>(event: &str, data: &T) -> Result<SseFrame, String> {
        Ok(SseFrame::default()
            .event(event)
            .data(serde_json::to_string(data).unwrap_or_default()))
    }

    async fn vram_frame(st: &SharedState) -> Result<SseFrame, String> {
        frame("vram", &st.vram.view(st).await)
    }

    let rx = st.telemetry.subscribe();

    let first_stats = st.telemetry.stats();
    let initial = stream::iter(vec![
        frame("stats", &first_stats),
        frame("jobs", &st.jobs.live()),
        // The registry publishes on change (server.rs's status tick), so a
        // dashboard opened onto a steady-state gateway would otherwise see no
        // `runtime` frame until something started or stopped.
        frame("runtime", &st.runtime().list()),
    ])
    .chain(stream::once({
        let st = st.clone();
        async move { vram_frame(&st).await }
    }))
    // MCP statuses are published on change too, and the sidebar's error badge
    // reads this frame: a server that went into error before the dashboard
    // opened would otherwise stay unflagged until its next transition.
    .chain(stream::once({
        let st = st.clone();
        async move { frame("mcp", &st.mcp.status_views(&st.snapshot()).await) }
    }))
    // The Backends badge (container-builds §8) changes on a schedule of its
    // own, so a dashboard opened between two checks needs the current count
    // up front, like the MCP error badge above.
    .chain(stream::once({
        let st = st.clone();
        async move { frame("updates", &crate::backends::updates::summary(&st).await) }
    }));

    let live = {
        let st = st.clone();
        stream::unfold(rx, |mut rx| async move {
            let ev = rx.recv().await;
            Some((ev, rx))
        })
        .flat_map(move |ev| {
            let st = st.clone();
            stream::once(async move {
                match ev {
                    Ok(Event::Request(summary)) => vec![
                        frame("request", &summary),
                        frame("stats", &st.telemetry.stats()),
                    ],
                    Ok(Event::Mcp(views)) => vec![frame("mcp", &views)],
                    Ok(Event::Jobs(views)) => vec![frame("jobs", &views)],
                    Ok(Event::Vram(view)) => vec![frame("vram", &view)],
                    Ok(Event::Runtime(views)) => vec![frame("runtime", &views)],
                    Ok(Event::Updates(summary)) => vec![frame("updates", &summary)],
                    Err(_) => vec![], // lagged; drop
                }
            })
            .flat_map(stream::iter)
        })
    };

    // Free VRAM moves without lmgw doing anything, so the ledger is polled
    // rather than waited on.
    let tick = {
        let st = st.clone();
        stream::unfold(
            tokio::time::interval(std::time::Duration::from_secs(5)),
            |mut i| async move {
                i.tick().await;
                Some(((), i))
            },
        )
        .flat_map(move |_| {
            let st = st.clone();
            stream::once(async move { vram_frame(&st).await })
        })
    };

    // Live token rate. `Event::Request` only fires when a request *finishes*,
    // so a stream in progress would otherwise never refresh `tok_s`; this polls
    // the (purely in-memory, no DB) counters instead. It emits only while
    // something is actually in flight, and only when the numbers changed, so an
    // idle dashboard sees nothing but SSE keep-alives and a stalled request
    // doesn't republish the same frame twice a second.
    let rate = {
        let st = st.clone();
        stream::unfold(
            (tokio::time::interval(RATE_TICK), first_stats),
            move |(mut ticker, last)| {
                let st = st.clone();
                async move {
                    loop {
                        ticker.tick().await;
                        let now = st.telemetry.stats();
                        if now.active_requests > 0 && now != last {
                            // `now` becomes the next comparison baseline.
                            return Some((frame("stats", &now), (ticker, now)));
                        }
                    }
                }
            },
        )
    };

    let stream = initial.chain(stream::select(live, stream::select(tick, rate)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use crate::store;

    /// Args as `audio_model_set` receives them from `POST /api/op/audio_model_set`.
    fn patch(args: serde_json::Value) -> AudioPatch {
        crate::ops::patch_from_args(Some(args.as_object().unwrap().clone())).unwrap()
    }

    /// A fresh in-memory state with `settings.audio.models_dir` pointed at a
    /// scratch dir, so `check_voice_refs` has somewhere real to look.
    async fn state_with_models_dir() -> (SharedState, tempfile::TempDir) {
        let state = AppState::init_for_tests().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut settings = state.snapshot().settings.clone();
        settings.audio.models_dir = dir.path().display().to_string();
        store::save_settings(&state.db, &settings).await.unwrap();
        state.reload_snapshot().await.unwrap();
        (state, dir)
    }

    fn minimal_new_model() -> store::NewAudioModel {
        store::NewAudioModel {
            model_id: "fish".into(),
            family: "fish_audio".into(),
            path: "Fish-Audio-S2-Pro-GGUF".into(),
            task: "tts".into(),
            mode: "offline".into(),
            lazy: None,
            busy_timeout_ms: None,
            load_options: Default::default(),
            session_options: Default::default(),
            default_request_options: Default::default(),
            model_spec_override: None,
            config_id: None,
            weight_id: None,
            voice_presets: Default::default(),
            default_voice_preset: None,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: HoldFallbackMode::Inherit,
            hold_fallback: None,
        }
    }

    #[tokio::test]
    async fn create_happy_path_inserts_and_reloads() {
        let (state, _dir) = state_with_models_dir().await;
        let res = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create",
                "model_id": "fish",
                "family": "fish_audio",
                "path": "Fish-Audio-S2-Pro-GGUF",
                "task": "tts",
            })),
        )
        .await
        .unwrap();
        assert_eq!(res["ok"], true);
        let id = res["id"].as_i64().unwrap();
        let saved = store::get_audio_model(&state.db, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.model_id, "fish");
        assert_eq!(saved.family, "fish_audio");
        assert_eq!(saved.path, "Fish-Audio-S2-Pro-GGUF");
        assert_eq!(saved.task, "tts");
        assert_eq!(saved.mode, "offline");
        assert!(saved.enabled);
    }

    /// The Leptos editor always sends `default_voice_preset` — a blank one
    /// means "no default", not a preset literally named "".
    #[tokio::test]
    async fn create_treats_a_blank_default_preset_as_none() {
        let (state, _dir) = state_with_models_dir().await;
        let res = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create",
                "model_id": "pocket",
                "family": "pocket_tts",
                "path": "PocketTTS-GGUF/english",
                "task": "tts",
                "voice_presets": {"me": {"voice_id": "narrator"}},
                "default_voice_preset": "",
            })),
        )
        .await
        .unwrap();
        assert_eq!(res["ok"], true);
        let saved = store::get_audio_model(&state.db, res["id"].as_i64().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(saved.default_voice_preset.is_none());
    }

    /// The engine keys added with audio.cpp's own: they round-trip through the
    /// row, and a blank one on update means *inherit the class* — a reset
    /// named in `clear`, not a zero written into the column (0 is a real
    /// value there: "wait forever").
    #[tokio::test]
    async fn per_model_engine_keys_round_trip_and_clear_back_to_inherit() {
        let (state, _dir) = state_with_models_dir().await;
        let res = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create",
                "model_id": "yue2",
                "family": "yue2",
                "path": "YuE2-GGUF",
                "task": "gen",
                "lazy": false,
                "busy_timeout_ms": 900_000,
                "default_request_options": {"cot": "full"},
                "model_spec_override": "specs/yue2.json",
                "config_id": "base",
                "weight_id": "q8_0",
            })),
        )
        .await
        .unwrap();
        let id = res["id"].as_i64().unwrap();
        let saved = store::get_audio_model(&state.db, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.lazy, Some(false));
        assert_eq!(saved.busy_timeout_ms, Some(900_000));
        assert_eq!(saved.default_request_options["cot"], "full");
        assert_eq!(
            saved.model_spec_override.as_deref(),
            Some("specs/yue2.json")
        );
        assert_eq!(saved.config_id.as_deref(), Some("base"));
        assert_eq!(saved.weight_id.as_deref(), Some("q8_0"));

        audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "update",
                "id": id,
                "clear": "lazy busy_timeout_ms",
                "model_spec_override": "",
                "config_id": "",
                "weight_id": "",
            })),
        )
        .await
        .unwrap();
        let saved = store::get_audio_model(&state.db, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.lazy, None, "back to the class's lazy_load");
        assert_eq!(saved.busy_timeout_ms, None);
        assert!(saved.model_spec_override.is_none());
        assert!(saved.config_id.is_none() && saved.weight_id.is_none());
        assert_eq!(
            saved.default_request_options["cot"], "full",
            "a field the patch left out keeps its value"
        );
    }

    /// audiocpp_server refuses a negative `busy_timeout_ms` at startup, so a
    /// saved one would cost a container that never comes up.
    #[tokio::test]
    async fn create_rejects_a_negative_busy_timeout() {
        let (state, _dir) = state_with_models_dir().await;
        let err = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p", "task": "tts",
                "busy_timeout_ms": -1,
            })),
        )
        .await
        .unwrap_err();
        assert!(err.contains("busy timeout"), "{err}");
    }

    /// A spec override is mounted into the container like the model path, so
    /// it is bounded the same way.
    #[tokio::test]
    async fn create_rejects_a_spec_override_outside_the_models_dir() {
        let (state, _dir) = state_with_models_dir().await;
        let err = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p", "task": "tts",
                "model_spec_override": "../../etc/passwd",
            })),
        )
        .await
        .unwrap_err();
        assert!(!err.is_empty(), "a traversing path must not be saved");
    }

    #[tokio::test]
    async fn create_rejects_unknown_task() {
        let (state, _dir) = state_with_models_dir().await;
        let err = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p",
                "task": "bogus",
            })),
        )
        .await
        .unwrap_err();
        assert!(err.contains("unknown task"), "{err}");
    }

    #[tokio::test]
    async fn create_rejects_a_voice_preset_without_a_voice() {
        let (state, _dir) = state_with_models_dir().await;
        let err = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p", "task": "tts",
                "voice_presets": {"narrator": {"reference_text": "hi"}},
            })),
        )
        .await
        .unwrap_err();
        assert!(err.contains("voice_id"), "{err}");
    }

    #[tokio::test]
    async fn create_rejects_a_default_naming_an_undeclared_preset() {
        let (state, _dir) = state_with_models_dir().await;
        let err = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p", "task": "tts",
                "voice_presets": {"narrator": {"voice_id": "alba"}},
                "default_voice_preset": "narrater",
            })),
        )
        .await
        .unwrap_err();
        assert!(err.contains("narrater"), "{err}");
    }

    /// audiocpp_server opens every `voice_ref` at startup — a missing file
    /// there must fail the save, not surface as a dead container later.
    #[tokio::test]
    async fn create_rejects_a_voice_ref_to_a_missing_file() {
        let (state, _dir) = state_with_models_dir().await;
        let err = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p", "task": "tts",
                "voice_presets": {"narrator": {"voice_ref": "/models/voices/missing.wav"}},
            })),
        )
        .await
        .unwrap_err();
        assert!(err.contains("no such file"), "{err}");
    }

    #[tokio::test]
    async fn create_accepts_a_voice_ref_that_exists_on_disk() {
        let (state, dir) = state_with_models_dir().await;
        std::fs::create_dir_all(dir.path().join("voices")).unwrap();
        std::fs::write(dir.path().join("voices").join("n.wav"), b"fake wav").unwrap();
        let res = audio_model_set(
            &state,
            patch(serde_json::json!({
                "action": "create", "model_id": "x", "family": "f", "path": "p", "task": "tts",
                "voice_presets": {"narrator": {"voice_ref": "/models/voices/n.wav"}},
                "default_voice_preset": "narrator",
            })),
        )
        .await
        .unwrap();
        assert_eq!(res["ok"], true);
    }

    /// A sparse update patch (only `enabled`) must not clobber the fields it
    /// omits — the whole point of overlaying onto the current row instead of
    /// requiring a full replace.
    #[tokio::test]
    async fn update_overlays_only_supplied_fields() {
        let (state, _dir) = state_with_models_dir().await;
        let id = store::insert_audio_model(&state.db, &minimal_new_model())
            .await
            .unwrap();

        let res = audio_model_set(
            &state,
            patch(serde_json::json!({ "action": "update", "id": id, "enabled": false })),
        )
        .await
        .unwrap();
        assert_eq!(res["ok"], true);

        let saved = store::get_audio_model(&state.db, id)
            .await
            .unwrap()
            .unwrap();
        assert!(!saved.enabled);
        assert_eq!(saved.model_id, "fish");
        assert_eq!(saved.family, "fish_audio");
        assert_eq!(saved.path, "Fish-Audio-S2-Pro-GGUF");
        assert_eq!(saved.task, "tts");
        assert_eq!(saved.mode, "offline");
    }

    /// The merged result is re-validated on update too: overlaying an unknown
    /// task onto an otherwise-valid row must still be rejected.
    #[tokio::test]
    async fn update_revalidates_the_merged_result() {
        let (state, _dir) = state_with_models_dir().await;
        let id = store::insert_audio_model(&state.db, &minimal_new_model())
            .await
            .unwrap();

        let err = audio_model_set(
            &state,
            patch(serde_json::json!({ "action": "update", "id": id, "task": "bogus" })),
        )
        .await
        .unwrap_err();
        assert!(err.contains("unknown task"), "{err}");
    }

    #[tokio::test]
    async fn delete_removes_the_row() {
        let (state, _dir) = state_with_models_dir().await;
        let id = store::insert_audio_model(&state.db, &minimal_new_model())
            .await
            .unwrap();

        let res = audio_model_set(
            &state,
            patch(serde_json::json!({ "action": "delete", "id": id })),
        )
        .await
        .unwrap();
        assert_eq!(res["ok"], true);
        assert!(store::get_audio_model(&state.db, id)
            .await
            .unwrap()
            .is_none());
    }
}
