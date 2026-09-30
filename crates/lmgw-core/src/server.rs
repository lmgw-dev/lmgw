//! Axum router factory + server runner (§3). Used by the Tauri shell and by
//! the headless example; no Tauri dependency.

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::error::GatewayError;
use crate::ingress::ClientProto;
use crate::principal::{Cap, Principal, Refusal};
use crate::proxy::{self, RequestCtx};
use crate::state::SharedState;

/// Build the full application router (API + UI + events + assets).
pub fn build_router(state: SharedState) -> Router {
    // The JSON routes, bounded by the `max_body_mb` setting (§13). axum's
    // built-in 2 MiB `DefaultBodyLimit` is switched off here so that the
    // visible setting is the only ceiling in play — see `body_limit_mw`.
    let json_api = Router::new()
        .route("/chat/completions", post(chat_completions))
        // §21. Not a sibling of the two above: it may run several upstream
        // turns with tool calls between them, so it goes to its own handler
        // rather than through the 1:1 proxy.
        .route("/responses", post(responses))
        // The stateful half (§21 stage 2): `store` persists a response, and
        // these are how a client reads, continues from, and drops one.
        .route("/responses/{id}", get(response_get).delete(response_delete))
        .route("/responses/{id}/input_items", get(response_input_items))
        .route("/completions", post(legacy_completions))
        .route("/embeddings", post(embeddings))
        // quickdoc §9a: the cross-encoder half of the aux router. Jina and TEI
        // request shapes both land here.
        .route("/rerank", post(rerank))
        .route("/messages", post(anthropic_messages))
        // The Anthropic SDKs' `messages.count_tokens()` (api-docs design
        // §5.2): a thin adapter onto the universal counter below.
        .route("/messages/count_tokens", post(messages_count_tokens))
        .route("/count_tokens", post(count_tokens))
        .route("/models", get(list_models))
        // A single model's object. Wildcard capture, not `{id}`: a
        // passthrough id carries its upstream prefix and the provider's own
        // slashes (`kilo/anthropic/claude-sonnet-5`), so the id is everything
        // after `/models/` (model capabilities design §2.2).
        .route("/models/{*id}", get(model_by_id))
        // This document, filtered to the inference plane (api-docs design
        // §4.11): the same generated document as `/api/openapi.json`, minus
        // everything an inference-only credential cannot reach.
        .route(
            "/openapi.json",
            get(crate::openapi::v1_json).layer(tower_http::compression::CompressionLayer::new()),
        )
        .layer(middleware::from_fn_with_state(state.clone(), body_limit_mw))
        .layer(axum::extract::DefaultBodyLimit::disable());

    // Audio and generic task routes carry whole audio files; they are not
    // bounded by `max_body_mb` and keep the unconditional `disable()` they
    // have always had. The image routes join them for the same reason
    // (image-generation design §6): an edit carries images in and every
    // answer comes back base64-encoded.
    let media_api = Router::new()
        .route("/audio/speech", post(audio_speech))
        .route("/audio/transcriptions", post(audio_transcriptions))
        // The same upload, answered with the word timings, segments and
        // speaker turns the plain route drops.
        .route(
            "/audio/transcriptions/details",
            post(audio_transcription_details),
        )
        // Forced alignment of an uploaded clip against a known transcript —
        // what a `task: "align"` row is for, without the client having to put
        // its audio on the container's filesystem first.
        .route("/audio/alignments", post(audio_alignments))
        .route("/audio/voices", get(audio_voices))
        .route("/images/generations", post(image_generations))
        .route("/images/edits", post(image_edits))
        // audio.cpp's generic task routes: everything that has no OpenAI shape
        // (separation, VAD, diarization, alignment, VC/SVC/S2S, music
        // generation, …). Source audio and long prompts are as big as they are.
        .route("/tasks/run", post(task_run))
        .route("/tasks/stream", post(task_stream))
        .layer(axum::extract::DefaultBodyLimit::disable());

    // Both `/v1` groups are `Inference` (principals §3.2), so one
    // `route_layer` over the merged pair is the whole declaration. `route_layer`
    // rather than `layer`: it runs **after** routing, so `/v1/nonsense` is
    // still a 404 rather than a 401 that implies the route exists.
    let api = json_api
        .merge(media_api)
        .route_layer(require(&state, Cap::Inference));

    // MCP gateway (northbound). Two capabilities on one path — see
    // `ingress::routes` — so it declares them itself.
    let mcp = crate::mcp::ingress::routes(&state);

    Router::new()
        .nest("/v1", api)
        .merge(mcp)
        // llama.cpp's `/tokenize` (api-docs design §5.3), at the root where
        // llama.cpp clients look for it; it declares its own capability.
        .merge(crate::proxy::tokenize::routes(&state))
        .merge(crate::web::routes(&state))
        // Innermost of the three, outermost of the application (§3.11): every
        // route below has already declared its capability, and this is what
        // gives them a principal to check it against.
        .layer(middleware::from_fn_with_state(state.clone(), principal_mw))
        // On the main router only, as today: on `/v1` and `/mcp` it is what
        // lets a browser-side client call the gateway with a bearer, and on the
        // dashboard plane it is harmless once the cookie is the gate — a
        // cross-site page never has the cookie sent (`SameSite=Strict`), a
        // same-site page on another port is refused by §3.6, and a
        // credential-less request reaches only `Public` routes.
        .layer(tower_http::cors::CorsLayer::permissive())
        // Second from the outside (§3.11, origins §4.2): a request whose `Host`
        // is an agent origin is answered by that agent's container here, so it
        // is **inside** the trace — both planes are in the log — and **outside**
        // CORS, which is what leaves an agent-origin response with no
        // `Access-Control-Allow-Origin` for a foreign page to read it with.
        //
        // `from_fn_with_state` rather than a hand-written `tower::Layer`: what
        // it needs is the state and the ability to run the rest of the stack
        // when the host is not an agent's, which is exactly `Next` — and it
        // reads beside `principal_mw`, the other root middleware, instead of as
        // the only `Service` impl in the crate.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::web::agent_proxy::host_dispatch,
        ))
        // Outermost, so the trace covers the CORS preflights and the refusals
        // too (§3.11). `make_span_with` because the default span records the
        // whole URI, and a URI carries the query — see [`http_span`].
        .layer(tower_http::trace::TraceLayer::new_for_http().make_span_with(http_span))
        .with_state(state)
}

/// The span every request is traced under (§3.4).
///
/// Method and **path**, never the query. `DefaultMakeSpan` records
/// `uri`, which is path *and* query, and the login link puts a one-shot
/// credential in a query string — so the default would write it into the
/// process log, which is exactly where the owner is told to read the link
/// from. The path alone is what a trace is actually read for.
fn http_span(req: &axum::http::Request<axum::body::Body>) -> tracing::Span {
    tracing::debug_span!(
        "request",
        method = %req.method(),
        path = %req.uri().path(),
    )
}

// ---------------------------------------------------------------------------
// The capability table (principals §3.2)
// ---------------------------------------------------------------------------

/// Every registered route and method, with the one capability it needs.
///
/// **The table is the specification, in code.** §3.2 is a table in a document
/// and documents drift; this is the same table where the compiler and the test
/// suite can see it. `tests/it/route_walk.rs` reads it in both directions — a
/// route registered anywhere in `src/` that is missing a row here fails, and a
/// row here that no route answers fails — so `Admin` really is "the default a
/// new route gets by being listed", and there is no route that is unlisted.
///
/// The method is the HTTP verb the route answers; `*` is a route registered
/// with `any(..)`, which answers all of them. The path is the one the router
/// matches, axum's `{id}` / `{*rest}` captures included.
pub const CAPABILITY_TABLE: &[(&str, &str, Cap)] = &[
    // -- /v1: the inference plane ------------------------------------------
    ("POST", "/v1/chat/completions", Cap::Inference),
    ("POST", "/v1/responses", Cap::Inference),
    ("GET", "/v1/responses/{id}", Cap::Inference),
    ("DELETE", "/v1/responses/{id}", Cap::Inference),
    ("GET", "/v1/responses/{id}/input_items", Cap::Inference),
    ("POST", "/v1/completions", Cap::Inference),
    ("POST", "/v1/embeddings", Cap::Inference),
    ("POST", "/v1/rerank", Cap::Inference),
    ("POST", "/v1/messages", Cap::Inference),
    ("POST", "/v1/messages/count_tokens", Cap::Inference),
    ("POST", "/v1/count_tokens", Cap::Inference),
    // llama.cpp's own route, at the root (api-docs design §5.3) — registered
    // in `proxy/tokenize.rs`, not with the `/v1` groups.
    ("POST", "/tokenize", Cap::Inference),
    ("GET", "/v1/models", Cap::Inference),
    ("GET", "/v1/models/{*id}", Cap::Inference),
    ("GET", "/v1/openapi.json", Cap::Inference),
    ("POST", "/v1/audio/speech", Cap::Inference),
    ("POST", "/v1/audio/transcriptions", Cap::Inference),
    ("POST", "/v1/audio/transcriptions/details", Cap::Inference),
    ("POST", "/v1/audio/alignments", Cap::Inference),
    ("GET", "/v1/audio/voices", Cap::Inference),
    ("POST", "/v1/images/generations", Cap::Inference),
    ("POST", "/v1/images/edits", Cap::Inference),
    ("POST", "/v1/tasks/run", Cap::Inference),
    ("POST", "/v1/tasks/stream", Cap::Inference),
    // -- MCP: the aggregate plane, and the self-admin plane on one path ----
    ("POST", "/mcp", Cap::Inference),
    ("GET", "/mcp", Cap::Inference),
    ("DELETE", "/mcp", Cap::Inference),
    // The one path with two capabilities: the POST is the self-admin plane
    // (§3.7), the GET and DELETE are the aggregate plane's session handlers.
    ("POST", "/mcp/admin", Cap::Admin),
    ("GET", "/mcp/admin", Cap::Inference),
    ("DELETE", "/mcp/admin", Cap::Inference),
    // -- The dashboard plane -----------------------------------------------
    // The one `/api` read a page may do before it has a session: the SPA
    // prints the version in its footer and on the login view.
    ("GET", "/api/version", Cap::Public),
    // The login (§3.4). `Public` and it has to be: a browser with no session
    // is the only caller these four have, and each verifies the credential it
    // was handed — the key in the query, the key in the body, or the cookie
    // the request already carries.
    ("GET", "/api/session/login", Cap::Public),
    ("GET", "/api/session", Cap::Public),
    ("POST", "/api/session", Cap::Public),
    ("DELETE", "/api/session", Cap::Public),
    ("GET", "/api/openapi.json", Cap::Admin),
    ("GET", "/api/status", Cap::Admin),
    ("GET", "/api/connect", Cap::Admin),
    ("GET", "/api/logs", Cap::Admin),
    ("GET", "/api/events", Cap::Admin),
    ("GET", "/api/models/full", Cap::Admin),
    ("GET", "/api/local-model", Cap::Admin),
    ("GET", "/api/local-model-check", Cap::Admin),
    ("GET", "/api/gguf-files", Cap::Admin),
    ("GET", "/api/model-inspect", Cap::Admin),
    ("GET", "/api/local-model-plan", Cap::Admin),
    ("GET", "/api/ladder-rung-plan", Cap::Admin),
    ("GET", "/api/llama-flags", Cap::Admin),
    ("GET", "/api/upstreams", Cap::Admin),
    ("GET", "/api/wiring", Cap::Admin),
    ("GET", "/api/mcp-servers", Cap::Admin),
    ("GET", "/api/mcp-servers/{id}/tools", Cap::Admin),
    ("GET", "/api/tools", Cap::Admin),
    ("GET", "/api/upstream-models", Cap::Admin),
    ("GET", "/api/hf/repo", Cap::Admin),
    ("GET", "/api/hf/downloads", Cap::Admin),
    ("GET", "/api/jobs", Cap::Admin),
    ("GET", "/api/vram", Cap::Admin),
    ("GET", "/api/audio/catalog", Cap::Admin),
    ("GET", "/api/settings-full", Cap::Admin),
    ("GET", "/api/responses", Cap::Admin),
    ("GET", "/api/responses/chain", Cap::Admin),
    ("GET", "/api/usage/series", Cap::Admin),
    ("GET", "/api/usage/top", Cap::Admin),
    ("GET", "/api/usage/heat", Cap::Admin),
    ("GET", "/api/usage/errors", Cap::Admin),
    ("GET", "/api/usage/local", Cap::Admin),
    ("GET", "/api/usage/keys", Cap::Admin),
    ("GET", "/api/usage/prices", Cap::Admin),
    ("GET", "/api/usage/export.csv", Cap::Admin),
    ("POST", "/api/op/{name}", Cap::Admin),
    // -- The agent catalog, where three capabilities meet ------------------
    ("GET", "/api/agents", Cap::Admin),
    ("POST", "/api/agents/import", Cap::Admin),
    ("GET", "/api/agents/runs/{job_id}", Cap::AgentSelf),
    ("POST", "/api/agents/runs/{job_id}/events", Cap::Ledger),
    ("POST", "/api/agents/runs/{job_id}/close", Cap::Ledger),
    ("GET", "/api/agents/{id}", Cap::AgentSelf),
    ("GET", "/api/agents/{id}/export", Cap::Admin),
    // The second path with two capabilities: a container reads its own runs,
    // and opens one.
    ("GET", "/api/agents/{id}/runs", Cap::AgentSelf),
    ("POST", "/api/agents/{id}/runs", Cap::Ledger),
    // -- The docs plane ----------------------------------------------------
    ("GET", "/api/docs/corpora", Cap::Admin),
    ("POST", "/api/docs/corpora", Cap::Admin),
    ("GET", "/api/docs/corpora/{id}", Cap::Admin),
    ("POST", "/api/docs/corpora/{id}/delete", Cap::Admin),
    ("POST", "/api/docs/corpora/{id}/ingest", Cap::Admin),
    ("POST", "/api/docs/corpora/{id}/re-embed", Cap::Admin),
    ("GET", "/api/docs/corpora/{id}/documents", Cap::Admin),
    ("GET", "/api/docs/chunks", Cap::Admin),
    ("POST", "/api/docs/search", Cap::Admin),
    ("GET", "/api/docs/eval", Cap::Admin),
    ("POST", "/api/docs/eval", Cap::Admin),
    ("GET", "/api/docs/golden", Cap::Admin),
    ("POST", "/api/docs/golden", Cap::Admin),
    ("POST", "/api/docs/golden/generate", Cap::Admin),
    ("GET", "/api/docs/golden/candidates", Cap::Admin),
    (
        "POST",
        "/api/docs/golden/candidates/{id}/accept",
        Cap::Admin,
    ),
    (
        "POST",
        "/api/docs/golden/candidates/{id}/reject",
        Cap::Admin,
    ),
    ("POST", "/api/docs/golden/{id}/delete", Cap::Admin),
    ("GET", "/api/docs/requests", Cap::Admin),
    ("POST", "/api/docs/requests/{id}/status", Cap::Admin),
    ("GET", "/api/docs/export", Cap::Admin),
    ("GET", "/api/docs/export/manifest", Cap::Admin),
    ("POST", "/api/docs/import", Cap::Admin),
    // -- Knowledge bases (chat-complete §9.5) ------------------------------
    ("GET", "/api/knowledge/bases", Cap::Admin),
    ("POST", "/api/knowledge/bases", Cap::Admin),
    ("GET", "/api/knowledge/bases/{id}", Cap::Admin),
    ("POST", "/api/knowledge/bases/{id}/settings", Cap::Admin),
    ("POST", "/api/knowledge/bases/{id}/delete", Cap::Admin),
    ("POST", "/api/knowledge/bases/{id}/resume", Cap::Admin),
    ("POST", "/api/knowledge/bases/{id}/cancel", Cap::Admin),
    ("GET", "/api/knowledge/bases/{id}/files", Cap::Admin),
    ("POST", "/api/knowledge/bases/{id}/files", Cap::Admin),
    ("POST", "/api/knowledge/files/{id}/delete", Cap::Admin),
    ("POST", "/api/knowledge/files/{id}/reingest", Cap::Admin),
    ("GET", "/api/knowledge/files/{id}/text", Cap::Admin),
    ("GET", "/api/knowledge/files/{id}/original", Cap::Admin),
    ("POST", "/api/knowledge/search", Cap::Admin),
    // -- The three user-facing mini-APIs -----------------------------------
    ("GET", "/chat/api/threads", Cap::Admin),
    ("POST", "/chat/api/threads", Cap::Admin),
    ("GET", "/chat/api/threads/{id}", Cap::Admin),
    ("POST", "/chat/api/threads/{id}/settings", Cap::Admin),
    ("POST", "/chat/api/threads/{id}/delete", Cap::Admin),
    ("POST", "/chat/api/threads/{id}/send", Cap::Admin),
    ("POST", "/chat/api/threads/{id}/pin", Cap::Admin),
    ("POST", "/chat/api/threads/{id}/move", Cap::Admin),
    ("GET", "/chat/api/search", Cap::Admin),
    ("GET", "/chat/api/threads/{id}/export", Cap::Admin),
    ("GET", "/chat/api/folders/{id}/export", Cap::Admin),
    ("GET", "/chat/api/export", Cap::Admin),
    ("GET", "/chat/api/folders", Cap::Admin),
    ("POST", "/chat/api/folders", Cap::Admin),
    ("POST", "/chat/api/folders/{id}", Cap::Admin),
    ("POST", "/chat/api/folders/{id}/delete", Cap::Admin),
    ("POST", "/chat/api/threads/{id}/archive", Cap::Admin),
    ("POST", "/chat/api/threads/{id}/persist", Cap::Admin),
    ("POST", "/chat/api/threads/{id}/continue", Cap::Admin),
    (
        "POST",
        "/chat/api/threads/{id}/messages/{mid}/delete",
        Cap::Admin,
    ),
    (
        "POST",
        "/chat/api/threads/{id}/messages/{mid}/edit",
        Cap::Admin,
    ),
    (
        "POST",
        "/chat/api/threads/{id}/messages/{mid}/regenerate",
        Cap::Admin,
    ),
    ("POST", "/chat/api/threads/{id}/attachments", Cap::Admin),
    ("POST", "/chat/api/attachments/{id}/delete", Cap::Admin),
    ("GET", "/chat/api/attachments/{id}", Cap::Admin),
    ("POST", "/chat/api/attachments/{id}/mode", Cap::Admin),
    ("POST", "/chat/api/attachments/{id}/transcribe", Cap::Admin),
    ("GET", "/chat/api/attachments/{id}/text", Cap::Admin),
    ("GET", "/audio-lab/api/models", Cap::Admin),
    ("GET", "/audio-lab/api/voices", Cap::Admin),
    ("GET", "/audio-lab/api/refs", Cap::Admin),
    ("POST", "/audio-lab/api/refs", Cap::Admin),
    ("GET", "/audio-lab/api/refs/{name}", Cap::Admin),
    ("POST", "/audio-lab/api/refs/{name}/delete", Cap::Admin),
    ("POST", "/audio-lab/api/refs/{name}/text", Cap::Admin),
    ("POST", "/audio-lab/api/speech", Cap::Admin),
    ("POST", "/audio-lab/api/transcriptions", Cap::Admin),
    ("POST", "/audio-lab/api/alignments", Cap::Admin),
    ("POST", "/audio-lab/api/tasks/run", Cap::Admin),
    ("GET", "/image-lab/api/models", Cap::Admin),
    ("POST", "/image-lab/api/generate", Cap::Admin),
    ("POST", "/image-lab/api/edit", Cap::Admin),
    // -- Service mode's reverse proxy --------------------------------------
    // The UI face is not here: it is an origin now (origins §4.2), dispatched
    // on `Host` above the router, with no principal resolved and no capability
    // asked for. What is left of it on this origin is the old mount answering
    // where the app went — `Public`, because an address is not a secret and a
    // stale bookmark should be redirected, not interrogated. The MCP face is
    // `Admin`: lmgw's own client dials it with the owner's bearer (§4.8).
    ("*", "/agents/{id}/app", Cap::Public),
    ("*", "/agents/{id}/app/", Cap::Public),
    ("*", "/agents/{id}/app/{*rest}", Cap::Public),
    ("*", "/agents/{id}/mcp", Cap::Admin),
    ("*", "/agents/{id}/mcp/{*rest}", Cap::Admin),
    // -- The SPA -----------------------------------------------------------
    // Public, and it has to be: a browser with no session must still be
    // served the page that asks for one.
    ("GET", "/", Cap::Public),
    ("GET", "/ui", Cap::Public),
    ("GET", "/ui/", Cap::Public),
    ("GET", "/ui/{*path}", Cap::Public),
    ("GET", "/{*path}", Cap::Public),
];

/// Serve until `shutdown` resolves.
pub async fn run(
    state: SharedState,
    addr: std::net::SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("lmgw listening on http://{addr}");
    // The headless owner's whole login procedure: click it (§3.4). One line
    // per start, beside the listening line, at the same level and target —
    // and a deliberate disclosure of a secret into the owner's own process
    // log, the only one lmgw makes.
    let base = crate::net::primary_base_url(&addr.to_string());
    match state
        .snapshot()
        .owner_key(crate::agents::token::OWNER_DASHBOARD)
    {
        Some(key) => tracing::info!("dashboard login: {base}/api/session/login?token={key}"),
        // Seeded by `AppState::init` and undeletable (§3.1), so this is the
        // unreachable arm — and it logs rather than panics because a gateway
        // that cannot print its login link still serves every other caller.
        None => tracing::info!(
            "dashboard login: unavailable — the '{}' key is missing or disabled; \
             rotate it on Usage → Keys from a browser that still has a session",
            crate::agents::token::OWNER_DASHBOARD
        ),
    }
    spawn_background_tasks(state);
    // With the peer address on every request: the agent proxy tells an app
    // who reached lmgw in `X-Forwarded-For` (origins §4.6), and that address
    // is the TCP peer's, never a header a client wrote.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    Ok(())
}

/// Periodic log pruning + llama-server status polling (§8, §10) and
/// restarting HF downloads interrupted by the last shutdown.
pub fn spawn_background_tasks(state: SharedState) {
    // First start after the usage-analytics upgrade: build the rollups from
    // whatever request logs are already there, so the Usage page opens on the
    // month that actually happened rather than on an empty chart that reads as
    // "the gateway did nothing in September". A no-op on every later boot.
    let st = state.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::store::backfill_usage_if_empty(&st.db).await {
            tracing::warn!("usage rollup backfill failed: {e}");
        }
    });

    let st = state.clone();
    tokio::spawn(async move {
        let rows = crate::store::list_hf_models(&st.db)
            .await
            .unwrap_or_default();
        for row in rows
            .into_iter()
            .filter(|r| r.status == "queued" || r.status == "downloading")
        {
            tracing::info!("resuming hf download {}/{}", row.repo, row.file);
            if let Err(e) = crate::jobs::hf_download::start(&st, &row).await {
                tracing::warn!("resuming hf download {}/{}: {e}", row.repo, row.file);
            }
        }
    });

    // Reconcile MCP southbound connections once on boot (§9): connect enabled
    // autostart servers; leave lazy ones stopped until first use (Milestone 3).
    let st = state.clone();
    tokio::spawn(async move {
        let snap = st.snapshot();
        st.mcp.reconcile(&snap).await;
        st.telemetry.mcp_status(st.mcp.status_views(&snap).await);
    });

    // Per-model container runtime, boot half (§3.4): reconcile against what
    // podman is already running, sweep the router-mode containers of a
    // pre-upgrade install, then start the `warm_start` models. First, before
    // anything else touches the runtime — every other consumer (`acquire`, the
    // reaper, the status tick) reads a map this is responsible for filling.
    //
    // Spawned rather than awaited, deliberately: `podman ps` + one `/health`
    // probe per container is unbounded wall clock on a box where podman is
    // slow or absent, and refusing every request until it finishes would make
    // a crash-recovery detail an availability problem. The cost is that an
    // `acquire` can race an adoption for the same model, which is why the
    // adoption's insert is identity-checked and yields
    // (`Registry::adopt`) instead of overwriting a live entry.
    let st = state.clone();
    tokio::spawn(async move { crate::runtime::lifecycle::boot(&st).await });

    // The idle reaper (§3.7). Its own task rather than a rider on the 5 s
    // status tick: it is the one background job that *stops containers*, and
    // pinning it to a slower, explicit cadence keeps it from inheriting the
    // status poll's timing as policy.
    let st = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(crate::runtime::lifecycle::REAP_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            crate::runtime::lifecycle::reap_idle(&st).await;
        }
    });

    // The VRAM peak sampler (image-generation §9). Beside the reaper rather
    // than inside it: the reaper's question is "has this been idle long
    // enough", asked every 15 s, and this one is "how much did that generation
    // take", which has to be asked four times a second while one is running.
    // Its own cadence is its own business — see `vram::peak`, which returns the
    // interval it wants after every pass and asks for the slow one whenever no
    // image container is up.
    let st = state.clone();
    tokio::spawn(async move { crate::vram::peak::run(st).await });

    // The build and registry-image update check (container-builds §8): every
    // `build_update_check_hours` (0 = off, re-read each cycle), a few minutes
    // after boot at the earliest — and not before the persisted last check is
    // that old, so a restart does not re-ask every remote.
    let st = state.clone();
    tokio::spawn(async move {
        crate::backends::updates::run_scheduler(st, crate::backends::updates::Cadence::default())
            .await
    });

    let st = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let settings = st.snapshot().settings.clone();
            match crate::store::prune_logs(
                &st.db,
                settings.retention_days,
                settings.retention_max_rows,
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => tracing::info!("pruned {n} request log rows"),
                Err(e) => tracing::warn!("log pruning failed: {e}"),
            }
            // The rollups are pruned by their *own* setting, which defaults to
            // keeping them forever (usage-analytics §3.3): the raw rows above
            // are the detail you can still open, these are the months you can
            // still see, and they cost a few hundred rows a day.
            match crate::store::prune_usage(&st.db, settings.usage_retention_months).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("pruned {n} usage rollup rows"),
                Err(e) => tracing::warn!("usage rollup pruning failed: {e}"),
            }
            // Stored `/v1/responses` conversations (§21). Chain-aware, and
            // driven by the same two visible Settings the Responses tab shows —
            // both `0` means keep everything, which is a choice the owner can
            // make and see, not a silent unbounded default.
            match crate::store::gc_responses(
                &st.db,
                settings.responses_retention_hours,
                settings.responses_max_chains,
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => tracing::info!("evicted {n} stored responses"),
                Err(e) => tracing::warn!("response GC failed: {e}"),
            }
            // Finished background-job rows (§9c). Same two visible knobs as the
            // request log above; both `0` keeps every job ever run.
            match crate::store::prune_jobs(
                &st.db,
                settings.jobs_retention_days,
                settings.jobs_max_rows,
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => tracing::info!("pruned {n} finished job rows"),
                Err(e) => tracing::warn!("job pruning failed: {e}"),
            }
            // Chat auto-archive + purge (chat-archive-pin-attachments design
            // §1). Two visible knobs, same "0 disables that step" convention
            // as every other retention rule above; pinned threads are exempt
            // from both, enforced in the query itself.
            match crate::store::sweep_chat_threads(
                &st.db,
                settings.chat_archive_days,
                settings.chat_purge_days,
            )
            .await
            {
                Ok((0, 0)) => {}
                Ok((archived, purged)) => {
                    tracing::info!("chat sweep: archived {archived}, purged {purged} threads")
                }
                Err(e) => tracing::warn!("chat sweep failed: {e}"),
            }
        }
    });

    let st = state;
    tokio::spawn(async move {
        // Seeded from the live registry for the same reason the MCP views are:
        // boot reconciliation may already have adopted containers, and the
        // first tick should compare against that rather than republish it.
        let mut last_runtime = st.runtime().list();
        // Seed from the current statuses (the boot reconcile already broadcast
        // them) so the first tick compares like-for-like instead of against an
        // empty vec and spuriously rebroadcasting.
        let mut last_mcp = st.mcp.status_views(&st.snapshot()).await;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let snap = st.snapshot();
            // Idle-reap first (§9): stop Ready conns idle past their
            // `idle_seconds` (0 = never, the warm-keep default). It marks them
            // Stopped and nudges `tools/list_changed`; it rides this tick rather
            // than a dedicated interval. Crucially it runs *before* poll_statuses
            // so a reaped Stopped conn isn't a candidate for the Error-retry path
            // (which never touches Stopped anyway) — the two don't fight (§9).
            st.mcp.reap_idle(&snap).await;

            // And the agent service containers, on the same tick and by the
            // same rule (container-runtime §3.3): `service.idle_seconds`, `0`
            // = never, and a request in flight is skipped however stale
            // `last_used` looks. Riding this tick rather than a cadence
            // invented for it is deliberate — the two idle windows mean the
            // same thing and should not drift into being checked differently.
            // **Spawned, not awaited**: a stop runs `podman stop -t <grace>`,
            // and blocking this tick on it would hold up `mcp.reap_idle` and
            // the status broadcast for as long as a container takes to go.
            let sweep = st.clone();
            tokio::spawn(async move { crate::agents::service::sweep_idle(&sweep).await });

            // Refresh MCP connection statuses (retries failed autostart conns
            // subject to backoff) and broadcast on change to the MCP tab (§9).
            // poll_statuses returns the post-reap views, so a reap surfaces as a
            // `stopped` badge here without a separate broadcast.
            let mcp = st.mcp.poll_statuses(&snap).await;
            if mcp != last_mcp {
                last_mcp = mcp.clone();
                st.telemetry.mcp_status(mcp);
            }

            // The per-model container registry (§3.2) — the one container
            // view there is, now that §7 has deleted the shared router-mode
            // statuses this used to sit beside. No podman call: `list()` is
            // the in-memory map, so this tick stays as cheap as it was.
            let runtime = st.runtime().list();
            if runtime != last_runtime {
                last_runtime = runtime.clone();
                st.telemetry.runtime(runtime);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Auth (§13)
// ---------------------------------------------------------------------------

/// Turn reasoning on or off for this one request: `on` | `off`.
pub const REASONING_HEADER: &str = "x-lmgw-reasoning";
/// Effort level for this one request, verbatim (`low`, `high`, `xhigh`, …).
pub const REASONING_EFFORT_HEADER: &str = "x-lmgw-reasoning-effort";
/// Thinking-token budget for this one request (a non-negative integer).
pub const REASONING_BUDGET_HEADER: &str = "x-lmgw-reasoning-budget";

/// Read the three reasoning-control headers (§5.2) — the tier above the body,
/// so a client can steer a model it is talking to through someone else's SDK,
/// which has no field for any of this.
///
/// Refused, by name, rather than guessed at — silently ignoring
/// `x-lmgw-reasoning-budget: lots` would leave a caller believing a budget
/// applied when none did:
///
/// - a value that is not `on`/`off`, not a non-negative integer, or an empty
///   effort;
/// - a contradiction **within this tier**, in either direction:
///   `x-lmgw-reasoning: on` with an effort of `none` or a budget of `0`, and
///   `x-lmgw-reasoning: off` with a real effort or a positive budget. Across
///   tiers precedence settles it; here there is nothing lower to defer to, and
///   picking one half of a contradiction would be a coin flip.
///
/// Whether that refusal becomes a 400 is the *route's* call, not this
/// function's — see [`RequestCtx::reasoning`](crate::proxy::RequestCtx).
fn reasoning_from_headers(
    h: &axum::http::HeaderMap,
) -> Result<Option<crate::ir::ReasoningControl>, GatewayError> {
    fn value<'a>(
        h: &'a axum::http::HeaderMap,
        name: &str,
    ) -> Result<Option<&'a str>, GatewayError> {
        match h.get(name) {
            None => Ok(None),
            Some(v) => v.to_str().map(Some).map_err(|_| {
                GatewayError::BadRequest(format!("{name}: value must be printable ASCII"))
            }),
        }
    }

    let mut c = crate::ir::ReasoningControl::default();
    if let Some(v) = value(h, REASONING_HEADER)? {
        c.enabled = match v.trim().to_ascii_lowercase().as_str() {
            "on" => Some(true),
            "off" => Some(false),
            other => {
                return Err(GatewayError::BadRequest(format!(
                    "{REASONING_HEADER}: expected 'on' or 'off', got '{other}'"
                )))
            }
        };
    }
    if let Some(v) = value(h, REASONING_EFFORT_HEADER)? {
        let v = v.trim();
        if v.is_empty() {
            return Err(GatewayError::BadRequest(format!(
                "{REASONING_EFFORT_HEADER}: expected a non-empty effort level"
            )));
        }
        c.effort = Some(v.to_string());
    }
    if let Some(v) = value(h, REASONING_BUDGET_HEADER)? {
        let n: i64 = v.trim().parse().map_err(|_| {
            GatewayError::BadRequest(format!(
                "{REASONING_BUDGET_HEADER}: expected a non-negative integer, got '{}'",
                v.trim()
            ))
        })?;
        if n < 0 {
            return Err(GatewayError::BadRequest(format!(
                "{REASONING_BUDGET_HEADER}: expected a non-negative integer, got '{n}'"
            )));
        }
        c.budget_tokens = Some(n);
    }

    // Within one tier the halves must agree, in both directions.
    let effort_is_none = c
        .effort
        .as_deref()
        .is_some_and(|e| e.eq_ignore_ascii_case("none"));
    match c.enabled {
        Some(true) => {
            if effort_is_none {
                return Err(GatewayError::BadRequest(format!(
                    "{REASONING_HEADER}: 'on' contradicts {REASONING_EFFORT_HEADER}: 'none'"
                )));
            }
            if c.budget_tokens == Some(0) {
                return Err(GatewayError::BadRequest(format!(
                    "{REASONING_HEADER}: 'on' contradicts {REASONING_BUDGET_HEADER}: 0"
                )));
            }
        }
        Some(false) => {
            if let Some(level) = c.effort.as_deref().filter(|_| !effort_is_none) {
                return Err(GatewayError::BadRequest(format!(
                    "{REASONING_HEADER}: 'off' contradicts {REASONING_EFFORT_HEADER}: '{level}'"
                )));
            }
            if let Some(b) = c.budget_tokens.filter(|b| *b > 0) {
                return Err(GatewayError::BadRequest(format!(
                    "{REASONING_HEADER}: 'off' contradicts {REASONING_BUDGET_HEADER}: {b}"
                )));
            }
        }
        None => {}
    }

    Ok((!c.is_empty()).then_some(c))
}

/// The 400 a *reasoning-consuming* route owes a client whose control headers
/// did not parse (§5.2), in that route's own dialect — the same
/// `to_openai_json` / `to_anthropic_json` choice each handler already makes for
/// a malformed body.
///
/// The other `/v1` routes never call this, which is the whole point: the same
/// header is inert on `GET /v1/models`, `/v1/embeddings`, `/v1/audio/*` and
/// `/mcp`, because none of them has a reasoning control to get wrong.
fn reasoning_or_400(ctx: &RequestCtx, dialect: fn(&GatewayError) -> Value) -> Option<Response> {
    let e = GatewayError::BadRequest(ctx.reasoning_error()?.to_string());
    Some((e.http_status(), Json(dialect(&e))).into_response())
}

/// Resolve the principal, once, at the router root (principals §3.5).
///
/// **Resolution only.** It parses the reasoning headers, looks the presented
/// credential up in the snapshot, reads `X-Lmgw-Run`, fills the derived fields
/// telemetry and pricing have always read, and inserts the [`RequestCtx`]. It
/// decides nothing: which capability a route needs is declared beside the
/// route, and [`require`] is what asks whether this principal holds it.
///
/// The one thing it *refuses* is a disabled row (§3.3) — and even that it does
/// not **render**, because the dialect belongs to the route: the refusal rides
/// along in the extensions and `require` answers it in `/v1`'s dialect or the
/// dashboard's, whichever this route speaks. Every registered route carries a
/// `require` (the §3.2 table has no gaps), so nothing slips past a pending one.
async fn principal_mw(State(state): State<SharedState>, mut req: Request, next: Next) -> Response {
    let snap = state.snapshot();
    // Parsed for every route in one place, but **enforced by none of them
    // here**: only the three chat-shaped routes consume a reasoning control,
    // and they turn a parse failure into a 400 themselves (`reasoning_or_400`).
    // A typo'd header on `GET /v1/models` or `/v1/embeddings` is not a reason
    // to refuse a request that never looks at it.
    let mut ctx = RequestCtx {
        reasoning: reasoning_from_headers(req.headers())
            .map(|c| c.map(Ok))
            .unwrap_or_else(|e| {
                // The bare message: `reasoning_or_400` wraps it in a fresh
                // `BadRequest`, whose Display adds the "bad request: " prefix
                // exactly once.
                Some(Err(match e {
                    GatewayError::BadRequest(m) => m,
                    other => other.to_string(),
                }))
            }),
        anthropic_beta: crate::egress::anthropic::client_betas(req.headers()),
        ..Default::default()
    };

    // §3.6, and only here: a cookie is the one credential a foreign page can
    // make a browser attach on its own. A bearer principal skips the rule —
    // `curl` sends no `Origin`, and a page that holds a bearer did not get it
    // from the browser.
    let by_cookie = crate::agents::token::presented(req.headers()).is_none()
        && crate::principal::session_cookie(req.headers()).is_some();
    let resolved = crate::principal::resolve(req.headers(), &snap);
    // A cookie that matched nothing authenticated nothing, and the rule is
    // about cookies that authenticate — so it is a cookie which *resolved to
    // something*, refusal included, that the origin is judged for. And judged
    // **first**: what a page on another port hears must not depend on the row
    // behind the cookie it replayed, or `owner_key_disabled` tells it which
    // of the owner's keys it is holding.
    let resolved = if by_cookie && !matches!(resolved, Ok(Principal::Anonymous)) {
        crate::principal::same_origin(req.headers()).and(resolved)
    } else {
        resolved
    };

    let who = match resolved {
        Ok(who) => who,
        Err(refusal) => {
            // Nothing authenticated, so the derived fields stay empty and the
            // request travels on as `Anonymous` carrying its refusal.
            req.extensions_mut().insert(refusal);
            req.extensions_mut().insert(ctx);
            return next.run(req).await;
        }
    };

    // The two derived fields, unchanged in meaning (§3.5): `client_key` is the
    // label Logs prints, and `agent` the capability the `/mcp` allow list and
    // the run meter read. They follow the principal now instead of being filled
    // by three separate branches — including with **Require API key** off,
    // where a presented key used to be resolved for agents only and dropped on
    // the floor for everyone else.
    if let Principal::Key {
        id,
        name,
        kind,
        agent_id,
    } = &who
    {
        ctx.client_key = Some(name.clone());
        if let (crate::config::ApiKeyKind::Agent, Some(agent_id)) = (kind, agent_id) {
            ctx.agent = Some(crate::agents::AgentIdentity {
                agent_id: agent_id.clone(),
                key_id: *id,
                key_name: name.clone(),
            });
        }
    }
    ctx.run = run_header(&state, req.headers(), ctx.agent.as_ref());
    ctx.principal = who;
    req.extensions_mut().insert(ctx);
    next.run(req).await
}

// ---------------------------------------------------------------------------
// The gate (principals §3.5)
// ---------------------------------------------------------------------------

/// What one `require` layer carries: the capability the route declared, and
/// whether it is the single route that also accepts `x-lmgw-admin-token`.
type GateState = (SharedState, Cap, bool);

/// [`gate`] as a plain function pointer, which is what makes [`GateLayer`]
/// nameable: an `async fn` returns an opaque future, so its own type cannot be
/// written down and a `fn require(..) -> ..` would have no return type.
type GateFn = fn(State<GateState>, Request, Next) -> futures::future::BoxFuture<'static, Response>;

pub(crate) type GateLayer = middleware::FromFnLayer<GateFn, GateState, (State<GateState>, Request)>;

/// The layer a route declares its capability with (§3.5).
///
/// Group-level `route_layer` where a whole router shares one capability,
/// `Handler::layer` where one path carries two (`/api/agents/{id}/runs`,
/// `/mcp/admin`). Both run **after** routing, so an unknown path is still a
/// 404 rather than a 401 that would tell a stranger which paths exist.
pub(crate) fn require(state: &SharedState, cap: Cap) -> GateLayer {
    middleware::from_fn_with_state((state.clone(), cap, false), gate as GateFn)
}

/// `require(Admin)` plus the third spelling of the bearer that
/// `POST /mcp/admin` — and nothing else — accepts (§3.3).
///
/// Read here rather than in `token::presented` precisely so that it widens
/// nothing: the MCP client configs that send `x-lmgw-admin-token` today keep
/// working, and the header stays inert on `/v1`, on the ledger and on `/api`.
pub(crate) fn require_admin_token(state: &SharedState) -> GateLayer {
    middleware::from_fn_with_state((state.clone(), Cap::Admin, true), gate as GateFn)
}

fn gate(
    State((state, cap, admin_token)): State<GateState>,
    mut req: Request,
    next: Next,
) -> futures::future::BoxFuture<'static, Response> {
    Box::pin(async move {
        let snap = state.snapshot();
        let mut ctx = req
            .extensions()
            .get::<RequestCtx>()
            .cloned()
            .unwrap_or_default();
        let mut pending = req.extensions().get::<Refusal>().cloned();

        if admin_token {
            if let Some(spelled) = req
                .headers()
                .get("x-lmgw-admin-token")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                match crate::principal::from_bearer(&snap, spelled) {
                    // A value that matches no row leaves whatever the root
                    // resolved standing: the header is an extra door, not a
                    // way to *lose* a session the request already carried.
                    Ok(Principal::Anonymous) => {}
                    Ok(who) => {
                        ctx.client_key = who.key_id().and_then(|id| {
                            snap.api_keys
                                .iter()
                                .find(|k| k.id == id)
                                .map(|k| k.name.clone())
                        });
                        ctx.principal = who;
                        // The **whole** ctx describes the owner now. An agent
                        // bearer that came in alongside the header had filled
                        // `agent` and `run` at the root, and leaving those
                        // standing would attribute this Admin-plane call to
                        // that agent and meter it against its run (§3.5) —
                        // the header is a different principal, not a second
                        // hat on the same one.
                        ctx.agent = None;
                        ctx.run = None;
                        pending = None;
                        req.extensions_mut().insert(ctx.clone());
                    }
                    Err(refusal) => pending = Some(refusal),
                }
            }
        }

        // A `Public` route needs no credential, so a **bad** one is not its
        // business: the pending refusal (§3.5 — a disabled row, resolved at
        // the root and rendered by whichever `require` the route carries) is
        // dropped here and rendered everywhere else. Without this, a browser
        // holding the cookie of a second owner key the owner has just
        // disabled is answered `401 owner_key_disabled` by `GET /`, never
        // sees the login card, and has no way back in but the process log.
        // It stays `Anonymous` for the handler, which is what it is.
        if let Some(refusal) = pending.filter(|_| cap != Cap::Public) {
            return refuse(&state, &ctx, cap, refusal, req.headers()).await;
        }
        if !ctx.principal.holds(cap, &snap) {
            // `/v1` with **Require API key** on and nothing presented: the
            // wording every OpenAI-shaped client has read since the first
            // release, unchanged and in its own dialect (§3.9).
            if cap == Cap::Inference && matches!(ctx.principal, Principal::Anonymous) {
                let e = GatewayError::Unauthorized("missing or invalid gateway API key");
                return policy_refusal(&state, &ctx, &e, req.headers()).await;
            }
            let refusal = match &ctx.principal {
                // Nothing was presented at all, so there is no row to name —
                // the owner needs the login link, not a forbidden.
                Principal::Anonymous => Refusal::session_required(),
                other => Refusal::forbidden(cap, other),
            };
            return refuse(&state, &ctx, cap, refusal, req.headers()).await;
        }

        // Expiry, requests/minute, tokens/minute and concurrency
        // (usage-analytics §4.2) — on `Inference` and nowhere else (§3.5). An
        // agent flushing a hundred ledger events a minute is not spending its
        // request budget, and a dashboard read never was. The two checks that
        // need the alias, scope and budget, stay post-body in
        // `policy::check_alias`; the alias is in the body.
        if cap != Cap::Inference {
            return next.run(req).await;
        }
        let Some(key) = ctx
            .principal
            .key_id()
            .and_then(|id| snap.api_keys.iter().find(|k| k.id == id))
        else {
            return next.run(req).await;
        };
        let guard = match state.policy.admit(key, chrono::Utc::now()) {
            Ok(g) => g,
            Err(e) => return policy_refusal(&state, &ctx, &e, req.headers()).await,
        };
        let resp = next.run(req).await;
        // The guard must outlive the *body*, not the handler: a streamed
        // response is built in microseconds and then drains for minutes on a
        // spawned task, so dropping here would let a `concurrency_limit` of 1
        // admit fifty concurrent streams. `hold_until_body_end` moves it into
        // the body's own lifetime, which is the thing the limit is about.
        hold_until_body_end(resp, guard)
    })
}

/// Render a gate refusal in the dialect the route speaks (§3.5).
///
/// `Inference` is `/v1` and `/mcp`, whose clients are OpenAI- and
/// Anthropic-shaped SDKs that parse `{"error": {…}}` and nothing else — and a
/// refusal there is traffic worth a log row, which is the other half of what
/// `policy_refusal` does. Every other capability answers the flat
/// `{ "code", "message" }` the SPA's `decode` already reads.
async fn refuse(
    state: &SharedState,
    ctx: &RequestCtx,
    cap: Cap,
    refusal: Refusal,
    headers: &axum::http::HeaderMap,
) -> Response {
    if cap != Cap::Inference {
        return refusal.into_response();
    }
    let e = GatewayError::Refused {
        status: refusal.status.as_u16(),
        code: refusal.code,
        message: refusal.message,
    };
    policy_refusal(state, ctx, &e, headers).await
}

/// `X-Lmgw-Run: <job id>` — which run a request belongs to (container-runtime
/// §3.1).
///
/// A header rather than a token claim, because one token serves every run, and
/// a header works identically on the JSONL transport's sibling HTTP calls, in
/// service mode, and from a container lmgw did not start.
///
/// **Only an agent token may attribute, and only to its own runs.** The header
/// moves money: whatever it names has its cost, tokens and call count folded
/// into the job's `result`, which the Runs tab and the cost line read. Without
/// the identity check any local process — and `auth_enabled` is off by default,
/// so that means any process at all — could bill an arbitrary live run by
/// guessing a small integer, and the owner would read the inflated figure as
/// something their agent did. A caller who cannot own a run cannot stamp one.
///
/// An unknown or foreign id from a caller who *could* own one is still
/// **ignored with a log line, never a refusal**: a mis-stamped request is a
/// request the owner made, and refusing it would turn a bookkeeping slip into
/// an outage.
fn run_header(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    agent: Option<&crate::agents::AgentIdentity>,
) -> Option<i64> {
    let raw = headers.get("x-lmgw-run")?.to_str().ok()?.trim().to_string();
    let Some(agent) = agent else {
        tracing::debug!(
            "X-Lmgw-Run '{raw}' ignored: the caller presented no agent token, and only an agent \
             may bill its own run"
        );
        return None;
    };
    let Ok(id) = raw.parse::<i64>() else {
        tracing::info!("X-Lmgw-Run: '{raw}' is not a job id — the request is not attributed");
        return None;
    };
    let Some(job) = state.jobs.live_one(id) else {
        tracing::info!("X-Lmgw-Run: {id} names no run in flight — the request is not attributed");
        return None;
    };
    if job.kind != crate::agents::JOB_KIND {
        tracing::info!("X-Lmgw-Run: job {id} is a '{}', not an agent run", job.kind);
        return None;
    }
    let mine = crate::agents::job_key(&agent.agent_id);
    if job.key.as_deref() != Some(mine.as_str()) {
        tracing::info!(
            "X-Lmgw-Run: run {id} is not a run of '{}' — the request is not attributed",
            agent.agent_id
        );
        return None;
    }
    Some(id)
}

/// Keep `guard` alive until the response body has been fully consumed — or
/// dropped, when the client goes away.
///
/// `next.run()` returns as soon as the handler has *built* the response, and a
/// streamed chat response is built in microseconds and then drained for minutes
/// by a spawned task. Dropping the concurrency slot there would let a
/// `concurrency_limit` of 1 admit fifty simultaneous streams while looking
/// perfectly correct in a non-streaming smoke test.
fn hold_until_body_end(resp: Response, guard: Option<crate::policy::ConcurrencyGuard>) -> Response {
    let Some(guard) = guard else { return resp };
    let (parts, body) = resp.into_parts();
    // `unfold` owns the guard for the life of the stream; the guard's Drop runs
    // when the last frame is read or the stream is dropped, whichever happens.
    let stream = futures::stream::unfold(
        (body.into_data_stream(), guard),
        |(mut inner, guard)| async move {
            use futures::StreamExt;
            inner.next().await.map(|item| (item, (inner, guard)))
        },
    );
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

async fn policy_refusal(
    state: &SharedState,
    ctx: &RequestCtx,
    e: &GatewayError,
    headers: &axum::http::HeaderMap,
) -> Response {
    // A refusal is traffic. Without a row, the one refusal class most likely to
    // be hit in a loop — a rate limit — is the one class Logs, Traffic and the
    // Usage page can never show, and "why did my agent stop working" is
    // answerable only from the client's side of the connection.
    crate::proxy::record_middleware_refusal(state, ctx, e).await;
    let body = if headers.contains_key("anthropic-version") {
        e.to_anthropic_json()
    } else {
        e.to_openai_json()
    };
    let mut resp = (e.http_status(), Json(body)).into_response();
    if let GatewayError::KeyRate { retry_after, .. } = e {
        if let Ok(v) = axum::http::HeaderValue::from_str(&retry_after.to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
    }
    resp
}

fn ctx_of(req_ext: Option<RequestCtx>) -> RequestCtx {
    req_ext.unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Request body limit (§13)
// ---------------------------------------------------------------------------

/// Bound the JSON `/v1` bodies by the `max_body_mb` setting; `0` means no bound.
///
/// This replaces axum's silent 2 MiB `DefaultBodyLimit`, which rejected e.g. a
/// base64 image a shade over the line with nothing telling the caller what it
/// had hit. The limit is read from the snapshot per request, so editing the
/// setting applies to the next request without a restart.
///
/// `pub(crate)` so `web::chat`'s attachment upload can share this exact
/// ceiling and refusal shape (chat-archive-pin-attachments design §2) rather
/// than growing a second body-limit story — see its route registration in
/// `web::labs_routes`.
pub(crate) async fn body_limit_mw(
    State(state): State<SharedState>,
    req: Request,
    next: Next,
) -> Response {
    let max_mb = state.snapshot().settings.max_body_mb;
    if max_mb == 0 {
        return next.run(req).await;
    }
    let max_bytes = max_mb as u64 * 1024 * 1024;

    // A declared oversize body is refused before a byte of it is read.
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max_bytes) {
        return body_limit_response(req.headers(), max_mb);
    }
    let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);

    // Chunked bodies declare no length, and a declared one may be a lie; this
    // wrapper is what actually enforces the bound. Tripping it surfaces to the
    // handler as a 413 body rejection, which `body_error` turns back into the
    // same named error.
    let (parts, body) = req.into_parts();
    let body = axum::body::Body::new(http_body_util::Limited::new(body, max));
    next.run(Request::from_parts(parts, body)).await
}

/// The 413, in the caller's protocol shape. Anthropic clients identify
/// themselves with `anthropic-version`, as in [`policy_refusal`].
fn body_limit_response(headers: &axum::http::HeaderMap, max_mb: u32) -> Response {
    let e = GatewayError::BodyTooLarge { max_mb };
    let body = if headers.contains_key("anthropic-version") {
        e.to_anthropic_json()
    } else {
        e.to_openai_json()
    };
    (e.http_status(), Json(body)).into_response()
}

/// Classify a body-extraction rejection.
///
/// A rejection carrying 413 is [`body_limit_mw`]'s wrapper tripping on a body
/// bigger than its headers admitted to; it is reported as the named limit
/// rather than as a generic malformed-JSON complaint.
fn body_error(state: &SharedState, e: axum::extract::rejection::JsonRejection) -> GatewayError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        GatewayError::BodyTooLarge {
            max_mb: state.snapshot().settings.max_body_mb,
        }
    } else {
        GatewayError::BadRequest(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// /v1 handlers
// ---------------------------------------------------------------------------

async fn responses(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            let err = body_error(&state, e);
            return (err.http_status(), Json(err.to_openai_json())).into_response();
        }
    };
    let ctx = ctx_of(Some(ctx));
    if let Some(r) = reasoning_or_400(&ctx, GatewayError::to_openai_json) {
        return r;
    }
    crate::responses::handle_responses(state, ctx, body).await
}

async fn response_get(
    State(state): State<SharedState>,
    id: axum::extract::Path<String>,
) -> Response {
    crate::responses::get_response(state, id).await
}

async fn response_delete(
    State(state): State<SharedState>,
    id: axum::extract::Path<String>,
) -> Response {
    crate::responses::delete_response(state, id).await
}

async fn response_input_items(
    State(state): State<SharedState>,
    id: axum::extract::Path<String>,
) -> Response {
    crate::responses::list_input_items(state, id).await
}

async fn chat_completions(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            let err = body_error(&state, e);
            return (err.http_status(), Json(err.to_openai_json())).into_response();
        }
    };
    let ctx = ctx_of(Some(ctx));
    if let Some(r) = reasoning_or_400(&ctx, GatewayError::to_openai_json) {
        return r;
    }
    proxy::handle_chat(state, ClientProto::OpenaiChat, ctx, body).await
}

async fn anthropic_messages(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            let err = body_error(&state, e);
            return (err.http_status(), Json(err.to_anthropic_json())).into_response();
        }
    };
    let ctx = ctx_of(Some(ctx));
    if let Some(r) = reasoning_or_400(&ctx, GatewayError::to_anthropic_json) {
        return r;
    }
    proxy::handle_chat(state, ClientProto::AnthropicMessages, ctx, body).await
}

async fn legacy_completions(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            let err = body_error(&state, e);
            return (err.http_status(), Json(err.to_openai_json())).into_response();
        }
    };
    proxy::handle_legacy_completions(state, ctx_of(Some(ctx)), body).await
}

async fn embeddings(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            let err = body_error(&state, e);
            return (err.http_status(), Json(err.to_openai_json())).into_response();
        }
    };
    proxy::handle_embeddings(state, ctx_of(Some(ctx)), body).await
}

async fn rerank(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            let err = body_error(&state, e);
            return (err.http_status(), Json(err.to_openai_json())).into_response();
        }
    };
    proxy::handle_rerank(state, ctx_of(Some(ctx)), body).await
}

/// `POST /v1/messages/count_tokens` — Anthropic's body and error dialect, the
/// reasoning headers judged as on `/v1/messages` (their tier shapes what is
/// counted), then [`proxy::handle_messages_count_tokens`].
async fn messages_count_tokens(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            let err = body_error(&state, e);
            return (err.http_status(), Json(err.to_anthropic_json())).into_response();
        }
    };
    let ctx = ctx_of(Some(ctx));
    if let Some(r) = reasoning_or_400(&ctx, GatewayError::to_anthropic_json) {
        return r;
    }
    proxy::handle_messages_count_tokens(state, ctx, body).await
}

async fn count_tokens(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            let err = body_error(&state, e);
            return (err.http_status(), Json(err.to_openai_json())).into_response();
        }
    };
    proxy::handle_count_tokens(state, ctx_of(Some(ctx)), body).await
}

/// `POST /v1/audio/speech` — OpenAI TTS shape; passthrough to the resolved
/// (audio.cpp) upstream by [`proxy::handle_audio_speech`].
async fn audio_speech(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            // Not bounded by `max_body_mb` — this route runs with the body
            // limit disabled outright, so a rejection here is malformed JSON.
            let err = GatewayError::BadRequest(e.to_string());
            return (err.http_status(), Json(err.to_openai_json())).into_response();
        }
    };
    proxy::handle_audio_speech(state, ctx_of(Some(ctx)), body).await
}

/// `POST /v1/audio/transcriptions` — JSON or multipart upload; passthrough by
/// [`proxy::handle_audio_upload`].
async fn audio_transcriptions(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    req: Request,
) -> Response {
    proxy::handle_audio_upload(
        state,
        ctx_of(Some(ctx)),
        req,
        proxy::AudioUpload::Transcription,
    )
    .await
}

/// `POST /v1/audio/transcriptions/details` — the same request as
/// `/v1/audio/transcriptions`, with the transcript detail arrays (words,
/// segments, speaker turns) the plain route leaves out.
async fn audio_transcription_details(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    req: Request,
) -> Response {
    proxy::handle_audio_upload(
        state,
        ctx_of(Some(ctx)),
        req,
        proxy::AudioUpload::TranscriptionDetails,
    )
    .await
}

/// `POST /v1/audio/alignments` — multipart forced alignment (`file`, `model`,
/// `text`, optional `language`) against a `task: "align"` model.
async fn audio_alignments(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    req: Request,
) -> Response {
    proxy::handle_audio_upload(state, ctx_of(Some(ctx)), req, proxy::AudioUpload::Alignment).await
}

/// `POST /v1/images/generations` — OpenAI image shape; passthrough to the
/// resolved (sd-server or cloud) upstream by [`proxy::handle_image_generation`].
async fn image_generations(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => {
            // Not bounded by `max_body_mb` — this route runs with the body
            // limit disabled outright, so a rejection here is malformed JSON.
            let err = GatewayError::BadRequest(e.to_string());
            return (err.http_status(), Json(err.to_openai_json())).into_response();
        }
    };
    proxy::handle_image_generation(state, ctx_of(Some(ctx)), body).await
}

/// `POST /v1/images/edits` — multipart upload; passthrough by
/// [`proxy::handle_image_edit`], which refuses a model whose row is not an
/// edit pipeline before anything reaches the container.
async fn image_edits(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    req: Request,
) -> Response {
    proxy::handle_image_edit(state, ctx_of(Some(ctx)), req).await
}

/// `pub(crate)` and `JsonSchema`-derived so `openapi::planes::inference`
/// (api-docs design §4.1 "`/api` query parameters ... the handler's `Query<T>`
/// struct") can turn it into `GET /v1/audio/voices`'s one query parameter
/// instead of hand-describing it a second time.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct VoicesQuery {
    #[serde(default)]
    model: String,
}

/// `GET /v1/audio/voices?model=<alias>` — voice ids/presets of a TTS model.
async fn audio_voices(
    State(state): State<SharedState>,
    axum::extract::Query(q): axum::extract::Query<VoicesQuery>,
) -> Response {
    if q.model.trim().is_empty() {
        let err = GatewayError::BadRequest("missing 'model' query parameter".into());
        return (err.http_status(), Json(err.to_openai_json())).into_response();
    }
    proxy::handle_audio_voices(state, q.model.trim()).await
}

/// Shared body decode for the two `/v1/tasks/*` routes.
fn task_body(
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Value, Response> {
    match body {
        Ok(Json(b)) => Ok(b),
        Err(e) => {
            let err = GatewayError::BadRequest(e.to_string());
            Err((err.http_status(), Json(err.to_openai_json())).into_response())
        }
    }
}

/// `POST /v1/tasks/run` — audio.cpp's generic task route; passthrough by
/// [`proxy::handle_task_run`].
async fn task_run(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    match task_body(body) {
        Ok(body) => proxy::handle_task_run(state, ctx_of(Some(ctx)), body).await,
        Err(resp) => resp,
    }
}

/// `POST /v1/tasks/stream` — the streaming-mode sibling of `/v1/tasks/run`.
async fn task_stream(
    State(state): State<SharedState>,
    axum::Extension(ctx): axum::Extension<RequestCtx>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    match task_body(body) {
        Ok(body) => proxy::handle_task_stream(state, ctx_of(Some(ctx)), body).await,
        Err(resp) => resp,
    }
}

/// The described-model builder lives in `capabilities::exposed` (model
/// capabilities design §8 item 8): it is the only place that reads GGUF
/// headers, template overrides and upstream catalogs, so `/v1/models`,
/// `/v1/models/{id}` and every future consumer of the same facts cannot drift
/// apart. Re-exported under its historical name because callers outside this
/// module (and the docs that name them) know it as `server::ExposedEntry`.
pub use crate::capabilities::exposed::ExposedEntry;

/// All client-routable models with their capabilities — see
/// [`crate::capabilities::exposed::exposed_entries`], which this is the
/// long-standing name for.
pub async fn exposed_model_names(state: &SharedState) -> Vec<ExposedEntry> {
    crate::capabilities::exposed::exposed_entries(state).await
}

// ---------------------------------------------------------------------------
// `GET /v1/models` and `GET /v1/models/{id}` (model capabilities design §2)
// ---------------------------------------------------------------------------

/// The OpenAI-shaped model object (design §2.1). Every optional field is
/// omitted when unknown rather than defaulted — "absent means unknown" is the
/// whole contract the consuming application reads.
fn openai_model_object(e: &ExposedEntry) -> Value {
    let mut obj = json!({
        "id": e.name,
        "object": "model",
        // Stable per process (design §2.1): a listing polled twice must not
        // claim every model was recreated in between.
        "created": e.created,
        "owned_by": e.owner,
    });
    if let Some(ctx) = e.context_length {
        obj["context_length"] = json!(ctx);
    }
    if let Some(n) = e.max_output_tokens {
        obj["max_output_tokens"] = json!(n);
    }
    if let Some(p) = &e.pricing {
        obj["pricing"] = json!({"prompt": p.prompt, "completion": p.completion});
    }
    if let Some(caps) = &e.capabilities {
        obj["capabilities"] = serde_json::to_value(caps).unwrap_or(Value::Null);
    }
    if !e.notes.is_empty() {
        obj["notes"] = json!(e.notes);
    }
    obj
}

/// The Anthropic-shaped model object (design §2.2): the Anthropic top level
/// plus that SDK's typed names as **aliases** over the same numbers —
/// `max_input_tokens` beside `context_window`, `max_tokens` beside
/// `max_output_tokens`. One capability schema, whichever SDK the caller holds.
fn anthropic_model_object(e: &ExposedEntry) -> Value {
    let created_at = chrono::DateTime::from_timestamp(e.created, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut obj = json!({
        "type": "model",
        "id": e.name,
        "display_name": e.name,
        "created_at": created_at,
    });
    if let Some(ctx) = e.context_length {
        obj["context_window"] = json!(ctx);
        obj["max_input_tokens"] = json!(ctx);
    }
    if let Some(n) = e.max_output_tokens {
        obj["max_output_tokens"] = json!(n);
        obj["max_tokens"] = json!(n);
    }
    if let Some(p) = &e.pricing {
        obj["pricing"] = json!({"prompt": p.prompt, "completion": p.completion});
    }
    if let Some(caps) = &e.capabilities {
        obj["capabilities"] = serde_json::to_value(caps).unwrap_or(Value::Null);
    }
    if !e.notes.is_empty() {
        obj["notes"] = json!(e.notes);
    }
    obj
}

/// The list-level `lmgw` block (design §2.1): gateway-wide facts — the routes,
/// the control headers, the hold/fallback semantics — so the per-model notes
/// can stay about that model.
fn lmgw_block() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        // Generated from the inference-plane route registry (api-docs design
        // §4.11, WP6) rather than hand-maintained here — the old hard-coded
        // list named `/v1/messages/count_tokens` and `/tokenize` before
        // either route existed (§0 finding 6); a route that is not really
        // registered now simply cannot appear.
        "endpoints": crate::openapi::lmgw_endpoints(),
        // The `{name: description}` map of every `Audience::Client` row in
        // `openapi::LMGW_HEADERS` (api-docs design §4.9) — what a client
        // reading this block needs to know; `x-lmgw-run`, `x-lmgw-admin-token`
        // and `x-lmgw-face` are the gateway's and an agent's own, not a
        // client's, so the table leaves them out of this map (WP2).
        "headers": crate::openapi::headers_block(),
        "notes": [
            "Model ids without a '/' are local llama.cpp models; 'embed/…' local embedders and \
             rerankers; 'audio/…' local audio.cpp models; 'image/…' local stable-diffusion.cpp \
             models; other prefixes are cloud upstreams passed through.",
            "A 503 whose error code is gpu_hold means local models are deliberately paused by \
             the owner; cloud aliases still work. Do not retry in a loop.",
            "A 503 whose error code is gpu_benchmark means a benchmark run has the GPU to \
             itself; local models come back when it finishes or is canceled, cloud aliases \
             still work. Do not retry in a loop.",
            "Local models start on first request; the first response after idle can take tens of \
             seconds (weights load).",
            "Local tool calling assumes llama-server's default --jinja; lmgw never passes \
             --no-jinja.",
            "capabilities.source says where the facts came from: gguf+config (read from the \
             model files' chat template and projector header — a text heuristic over the \
             template, cross-checked against llama-server /props by lmgw__local_model_test — \
             plus the owner's settings), catalog (what the provider publishes), config (owner \
             settings only), owner (set by hand). A missing capability field means unknown — do \
             not guess."
        ]
    })
}

/// Whether the caller is speaking the Anthropic dialect. The SDK stamps
/// `anthropic-version` on every request, which is what `/v1/models` has always
/// switched on; the single-model route and its 404 use the same test so a
/// client never gets one dialect's success and the other's error.
fn wants_anthropic(req: &Request) -> bool {
    req.headers().contains_key("anthropic-version")
}

/// `GET /v1/models` — aggregates enabled aliases, public local models, and
/// expose-all upstream catalogs (§6). Served OpenAI-shaped unless the caller
/// identifies as an Anthropic SDK via `anthropic-version`.
///
/// Each object carries `context_length`, per-token `pricing` (Kilo/OpenRouter
/// shape: `{prompt, completion}` USD/token, `"0"` for local models),
/// `max_output_tokens`, a `capabilities` object and plain-language `notes`
/// when they are known; the list carries the gateway-wide `lmgw` block (model
/// capabilities design §2).
async fn list_models(State(state): State<SharedState>, req: Request) -> Response {
    let models = crate::capabilities::exposed::exposed_entries(&state).await;
    if wants_anthropic(&req) {
        let data: Vec<Value> = models.iter().map(anthropic_model_object).collect();
        Json(json!({
            "data": data,
            "has_more": false,
            "first_id": null,
            "last_id": null,
            "lmgw": lmgw_block(),
        }))
        .into_response()
    } else {
        let data: Vec<Value> = models.iter().map(openai_model_object).collect();
        Json(json!({"object": "list", "data": data, "lmgw": lmgw_block()})).into_response()
    }
}

/// `GET /v1/models/{id}` — the same object as the list's, on its own (design
/// §2.1/§2.2). The id may contain `/` (`kilo/anthropic/claude-sonnet-5`),
/// hence the wildcard capture; an id that is not exposed is a 404 in the
/// caller's own error dialect.
async fn model_by_id(
    State(state): State<SharedState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    req: Request,
) -> Response {
    let anthropic = wants_anthropic(&req);
    // A single-model lookup (`exposed_entry`), not the whole catalog: this
    // used to build every alias/local/aux/audio/image row and fetch every
    // enabled expose-all upstream's live models just to keep one of them. It
    // is also what Chat's send-time vision check resolves through, so the two
    // cannot drift (model-capabilities design; chat-archive-pin-attachments
    // review finding 9).
    let model = crate::capabilities::exposed::exposed_entry(&state, &id).await;
    match &model {
        Some(e) if anthropic => Json(anthropic_model_object(e)).into_response(),
        Some(e) => Json(openai_model_object(e)).into_response(),
        None => {
            let err =
                GatewayError::NotFound(format!("model '{id}' is not exposed by this gateway"));
            let body = if anthropic {
                err.to_anthropic_json()
            } else {
                err.to_openai_json()
            };
            (err.http_status(), Json(body)).into_response()
        }
    }
}
