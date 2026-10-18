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
use crate::principal::Cap;
use crate::proxy::{self, RequestCtx};
use crate::state::SharedState;

/// The connect info both listeners serve with (device link design §4.5).
mod conn;
/// The principal on the device listener (device link design §4.7).
mod device_gate;
/// The device listener (device link design §4): its TLS accept, its serving
/// loop, and what runs it from the `devices` settings.
pub mod device_listener;
/// The device listener's face (device link design §4.6).
mod face;
/// `principal_mw` and the gate (principals §3.5, device link §4.7).
mod gate;
mod stop;
pub use conn::{canonical, Conn, DevicePeer, SocketHandle, Via};
pub use device_listener::{peer_key, DeviceListener};
pub use face::{serves_on, DEVICE_ROUTES};
pub(crate) use gate::{require, require_admin_token};
pub use stop::{
    quit_signal, signal_exit_code, until_stopped, InFlight, Running, ServedAt, Stops,
    CLOSE_GOING_AWAY, SESSIONS_END_WITHIN, STOPPING, STOP_WITHIN,
};

/// Build the plain listener's router (API + UI + events + assets).
pub fn build_router(state: SharedState) -> Router {
    build_router_for(state, Via::Plain)
}

/// Build the device listener's router (device link design §4.6): the same
/// routes with the device face — every `Inference` and `Chat` route and the
/// `/devices/*` routes, each other route a 404 — and neither CORS nor the
/// agent origins' dispatch: no browser holds a device certificate, and an
/// agent's app is the owner's, not a device's.
pub fn build_device_router(state: SharedState) -> Router {
    build_router_for(state, Via::Devices)
}

fn build_router_for(state: SharedState, via: Via) -> Router {
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
        // Decision models (decision-models design §2): TypeSafe's System One
        // wire, relayed. Its handler reads bytes, not `Json<Value>`: the body
        // is JSON whatever its `Content-Type` says, as on llama-server.
        .route("/systemone", post(crate::proxy::systemone::handle))
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
        // The MCP labels a caller may attach to a `/v1/realtime` or
        // `/v1/responses` tool (realtime-server-tools design §1.4): the list
        // connects nothing, the detail lists one label's tools.
        .route("/mcp/servers", get(crate::mcp::discovery::list))
        .route("/mcp/servers/{label}", get(crate::mcp::discovery::detail))
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

    // The realtime WebSocket (realtime design §10): a `GET` with no body, so
    // neither body limit above applies, and its frames have their own explicit
    // ceilings (`realtime.max_message_mb` / `max_frame_mb`, §10.4).
    let realtime_api = Router::new().route("/realtime", get(crate::realtime::upgrade));

    // WebRTC calls (realtime calls design §2): the offer, a few KB of SDP,
    // bounded by `max_body_mb` like the JSON group (§2.1), and the hangup,
    // which carries no body.
    let calls_api = Router::new()
        .route(
            "/realtime/calls",
            post(crate::realtime::calls::route::create),
        )
        .route(
            "/realtime/calls/{call_id}/hangup",
            post(crate::realtime::calls::route::hangup),
        )
        .layer(middleware::from_fn_with_state(state.clone(), body_limit_mw))
        .layer(axum::extract::DefaultBodyLimit::disable());

    // All four `/v1` groups are `Inference` (principals §3.2), so one
    // `route_layer` over the merged groups is the whole declaration. `route_layer`
    // rather than `layer`: it runs **after** routing, so `/v1/nonsense` is
    // still a 404 rather than a 401 that implies the route exists.
    let api = json_api
        .merge(media_api)
        .merge(realtime_api)
        .merge(calls_api)
        .route_layer(require(&state, Cap::Inference));

    // MCP gateway (northbound). Two capabilities on one path — see
    // `ingress::routes` — so it declares them itself.
    let mcp = crate::mcp::ingress::routes(&state)
        // A paired device's MCP host link (client-apps design §5.1): `Chat`,
        // declared by the route itself.
        .merge(crate::mcp::host::routes(&state));

    let routes = Router::new()
        .nest("/v1", api)
        .merge(mcp)
        // llama.cpp's `/tokenize` (api-docs design §5.3), at the root where
        // llama.cpp clients look for it; it declares its own capability.
        .merge(crate::proxy::tokenize::routes(&state))
        // The device listener's own routes (device link design §4.6): on
        // its face only, a 404 on the plain listener's (`gate`).
        .merge(crate::devices::routes(&state))
        .merge(crate::web::routes(&state))
        // Innermost of the three, outermost of the application (§3.11): every
        // route below has already declared its capability, and this is what
        // gives them a principal to check it against.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gate::principal_mw,
        ));
    let routes = match via {
        Via::Plain => plain_face(&state, routes),
        Via::Devices => face::device_face(routes),
    };
    routes
        // Outermost, so the trace covers the CORS preflights and the refusals
        // too (§3.11). `make_span_with` because the default span records the
        // whole URI, and a URI carries the query — see [`http_span`].
        .layer(tower_http::trace::TraceLayer::new_for_http().make_span_with(http_span))
        .with_state(state)
}

/// The plain listener's two outer layers: CORS, and the agent origins'
/// dispatch.
fn plain_face(state: &SharedState, routes: Router<SharedState>) -> Router<SharedState> {
    routes
        // On the main router only, as today: on `/v1` and `/mcp` it is what
        // lets a browser-side client call the gateway with a bearer, and on the
        // dashboard plane it is harmless once the cookie is the gate — a
        // cross-site page never has the cookie sent (`SameSite=Strict`), a
        // same-site page on another port is refused by §3.6, and a
        // credential-less request reaches only `Public` routes.
        //
        // The request headers are mirrored rather than answered with `*`: the
        // Fetch standard never lets the wildcard cover `Authorization`, so a
        // preflight answered `*` blocks exactly the browser client with a
        // bearer this layer is here for.
        .layer(
            tower_http::cors::CorsLayer::permissive()
                .allow_headers(tower_http::cors::AllowHeaders::mirror_request()),
        )
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
}

/// The span every request is traced under (§3.4).
///
/// Method and **path**, never the query. `DefaultMakeSpan` records
/// `uri`, which is path *and* query, and the login link puts a one-shot
/// credential in a query string — so the default would write it into the
/// process log, which is exactly where the owner is told to read the link
/// from. The path alone is what a trace is actually read for. And the
/// listener it came in on (device link design §4.5), so a log line says
/// which of the two a request used.
fn http_span(req: &axum::http::Request<axum::body::Body>) -> tracing::Span {
    let via = req
        .extensions()
        .get::<axum::extract::ConnectInfo<Conn>>()
        .map_or(Via::Plain, |c| c.0.via);
    tracing::debug_span!(
        "request",
        method = %req.method(),
        path = %req.uri().path(),
        via = %via,
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
    ("POST", "/v1/systemone", Cap::Inference),
    ("POST", "/v1/messages", Cap::Inference),
    ("POST", "/v1/messages/count_tokens", Cap::Inference),
    ("POST", "/v1/count_tokens", Cap::Inference),
    // llama.cpp's own route, at the root (api-docs design §5.3) — registered
    // in `proxy/tokenize.rs`, not with the `/v1` groups.
    ("POST", "/tokenize", Cap::Inference),
    ("GET", "/v1/models", Cap::Inference),
    ("GET", "/v1/models/{*id}", Cap::Inference),
    ("GET", "/v1/mcp/servers", Cap::Inference),
    ("GET", "/v1/mcp/servers/{label}", Cap::Inference),
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
    ("GET", "/v1/realtime", Cap::Inference),
    // WebRTC calls (realtime calls design §2): the hangup takes no slot and
    // counts no request (`gate`, C3).
    ("POST", "/v1/realtime/calls", Cap::Inference),
    (
        "POST",
        "/v1/realtime/calls/{call_id}/hangup",
        Cap::Inference,
    ),
    // -- MCP: the aggregate plane, and the self-admin plane on one path ----
    ("POST", "/mcp", Cap::Inference),
    ("GET", "/mcp", Cap::Inference),
    ("DELETE", "/mcp", Cap::Inference),
    // The one path with two capabilities: the POST is the self-admin plane
    // (§3.7), the GET and DELETE are the aggregate plane's session handlers.
    ("POST", "/mcp/admin", Cap::Admin),
    ("GET", "/mcp/admin", Cap::Inference),
    ("DELETE", "/mcp/admin", Cap::Inference),
    // A paired device's MCP host link (client-apps design §5.1): the `Chat`
    // layer, then the handler wants a device with a hosting grant.
    ("GET", "/mcp/host", Cap::Chat),
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
    // -- The Chat API (client-apps design §1.2) ----------------------------
    // `Chat`, not `Admin`: an owner holds it, and so does a paired device —
    // the three exports included, since a device reads every thread it may
    // see through list and get anyway (L2). What a device may *not* see of
    // it (admin threads, L3) is the Chat's own rule, not the gate's.
    ("GET", "/chat/api/threads", Cap::Chat),
    ("POST", "/chat/api/threads", Cap::Chat),
    ("GET", "/chat/api/threads/{id}", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/settings", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/delete", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/send", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/voice/warm", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/transcribe", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/speech/stop", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/pin", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/move", Cap::Chat),
    ("GET", "/chat/api/search", Cap::Chat),
    ("GET", "/chat/api/profiles", Cap::Chat),
    ("POST", "/chat/api/profiles", Cap::Chat),
    ("GET", "/chat/api/profiles/{id}", Cap::Chat),
    ("POST", "/chat/api/profiles/{id}", Cap::Chat),
    ("POST", "/chat/api/profiles/{id}/delete", Cap::Chat),
    ("POST", "/chat/api/profiles/{id}/reset", Cap::Chat),
    ("POST", "/chat/api/profiles/preview", Cap::Chat),
    ("POST", "/chat/api/profiles/test", Cap::Chat),
    ("POST", "/chat/api/profiles/speak", Cap::Chat),
    ("GET", "/chat/api/feed", Cap::Chat),
    ("GET", "/chat/api/threads/{id}/export", Cap::Chat),
    ("GET", "/chat/api/folders/{id}/export", Cap::Chat),
    ("GET", "/chat/api/export", Cap::Chat),
    ("GET", "/chat/api/folders", Cap::Chat),
    ("POST", "/chat/api/folders", Cap::Chat),
    ("POST", "/chat/api/folders/{id}", Cap::Chat),
    ("POST", "/chat/api/folders/{id}/delete", Cap::Chat),
    ("POST", "/chat/api/folders/{id}/current", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/archive", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/persist", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/continue", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/approvals", Cap::Chat),
    ("GET", "/chat/api/threads/{id}/tasks", Cap::Chat),
    ("POST", "/chat/api/threads/{id}/answer", Cap::Chat),
    (
        "POST",
        "/chat/api/threads/{id}/tasks/{task}/cancel",
        Cap::Chat,
    ),
    (
        "POST",
        "/chat/api/threads/{id}/messages/{mid}/delete",
        Cap::Chat,
    ),
    (
        "POST",
        "/chat/api/threads/{id}/messages/{mid}/edit",
        Cap::Chat,
    ),
    (
        "POST",
        "/chat/api/threads/{id}/messages/{mid}/regenerate",
        Cap::Chat,
    ),
    (
        "POST",
        "/chat/api/threads/{id}/messages/{mid}/speak",
        Cap::Chat,
    ),
    ("POST", "/chat/api/threads/{id}/attachments", Cap::Chat),
    ("POST", "/chat/api/attachments/{id}/delete", Cap::Chat),
    ("GET", "/chat/api/attachments/{id}", Cap::Chat),
    ("POST", "/chat/api/attachments/{id}/mode", Cap::Chat),
    ("POST", "/chat/api/attachments/{id}/transcribe", Cap::Chat),
    ("GET", "/chat/api/attachments/{id}/text", Cap::Chat),
    // The owner's alone: the dashboard's Chat page reads the rows a `chat`
    // frame named (review CL-11). A device's feed carries rows already.
    ("GET", "/chat/api/threads/rows", Cap::Admin),
    // -- The labs and their store: the owner's own playgrounds -------------
    ("GET", "/audio-lab/api/models", Cap::Admin),
    ("GET", "/audio-lab/api/voices", Cap::Admin),
    ("GET", "/audio-lab/api/refs", Cap::Admin),
    ("POST", "/audio-lab/api/refs", Cap::Admin),
    ("GET", "/audio-lab/api/refs/{name}", Cap::Admin),
    ("POST", "/audio-lab/api/refs/{name}/delete", Cap::Admin),
    ("POST", "/audio-lab/api/refs/{name}/text", Cap::Admin),
    ("POST", "/audio-lab/api/refs/{name}/transcribe", Cap::Admin),
    ("POST", "/audio-lab/api/speech", Cap::Admin),
    ("POST", "/audio-lab/api/transcriptions", Cap::Admin),
    ("POST", "/audio-lab/api/alignments", Cap::Admin),
    ("POST", "/audio-lab/api/tasks/run", Cap::Admin),
    ("POST", "/audio-lab/api/groups", Cap::Admin),
    ("GET", "/image-lab/api/models", Cap::Admin),
    ("POST", "/image-lab/api/groups", Cap::Admin),
    ("POST", "/image-lab/api/groups/edit", Cap::Admin),
    ("POST", "/image-lab/api/groups/{id}/run", Cap::Admin),
    ("GET", "/decision-lab/api/models", Cap::Admin),
    ("POST", "/decision-lab/api/count", Cap::Admin),
    ("POST", "/decision-lab/api/groups", Cap::Admin),
    (
        "POST",
        "/decision-lab/api/groups/{id}/cells/{cell}",
        Cap::Admin,
    ),
    ("POST", "/decision-lab/api/batches/preview", Cap::Admin),
    ("GET", "/decision-lab/api/batches", Cap::Admin),
    ("POST", "/decision-lab/api/batches", Cap::Admin),
    ("POST", "/decision-lab/api/batches/from-agent", Cap::Admin),
    ("GET", "/decision-lab/api/batches/{id}", Cap::Admin),
    ("GET", "/decision-lab/api/batches/{id}/items", Cap::Admin),
    ("POST", "/decision-lab/api/batches/{id}/cancel", Cap::Admin),
    ("POST", "/decision-lab/api/batches/{id}/resume", Cap::Admin),
    ("POST", "/decision-lab/api/batches/{id}/delete", Cap::Admin),
    ("GET", "/decision-lab/api/batches/{id}/metrics", Cap::Admin),
    (
        "POST",
        "/decision-lab/api/batches/{id}/threshold",
        Cap::Admin,
    ),
    ("GET", "/labs/api/{lab}/groups", Cap::Admin),
    ("GET", "/labs/api/{lab}/groups/{id}", Cap::Admin),
    ("POST", "/labs/api/{lab}/groups/{id}/delete", Cap::Admin),
    ("POST", "/labs/api/{lab}/clear", Cap::Admin),
    ("GET", "/labs/api/{lab}/usage", Cap::Admin),
    ("POST", "/labs/api/{lab}/orphans/delete", Cap::Admin),
    ("GET", "/labs/api/{lab}/export", Cap::Admin),
    ("GET", "/labs/api/{lab}/presets", Cap::Admin),
    ("POST", "/labs/api/{lab}/presets", Cap::Admin),
    ("GET", "/labs/api/{lab}/presets/{id}", Cap::Admin),
    ("POST", "/labs/api/{lab}/presets/{id}", Cap::Admin),
    ("POST", "/labs/api/{lab}/presets/{id}/delete", Cap::Admin),
    ("GET", "/labs/api/artifacts/{id}", Cap::Admin),
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
    // -- The device listener's own (device link design §4.6) --------------
    // `Public`: enrolment's handler checks the pairing token, which decides
    // alone. On the device listener's face only (`DEVICE_ROUTES`).
    ("POST", "/devices/enrol", Cap::Public),
    // -- The SPA -----------------------------------------------------------
    // Public, and it has to be: a browser with no session must still be
    // served the page that asks for one.
    ("GET", "/", Cap::Public),
    ("GET", "/ui", Cap::Public),
    ("GET", "/ui/", Cap::Public),
    ("GET", "/ui/{*path}", Cap::Public),
    ("GET", "/{*path}", Cap::Public),
];

/// Serve until `shutdown` resolves: [`bind`], then [`serve`].
pub async fn run(
    state: SharedState,
    addr: std::net::SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = bind(&state, addr).await?;
    serve(state, listener, shutdown, None).await
}

/// Refuse a dev instance on the production prefix, then bind `addr`.
///
/// Its own step so an entry point can act on the outcome before it serves:
/// the shell builds its window only once this process holds the port, and
/// never points one at whoever else does (chat-voice WP11 review m3).
pub async fn bind(
    state: &SharedState,
    addr: std::net::SocketAddr,
) -> anyhow::Result<tokio::net::TcpListener> {
    // Before anything lists, removes or starts a container by prefix.
    if let Some(why) =
        crate::config::dev_prefix_refusal(state.dev(), &state.snapshot().settings.container_prefix)
    {
        anyhow::bail!(why);
    }
    tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))
}

/// Serve on `listener` (from [`bind`]) until `shutdown` resolves.
///
/// `released`, when given, fires as the listener closes: right after
/// `shutdown`, before the open connections have drained. A restart that binds
/// the same port again waits for it instead of racing the old listener.
pub async fn serve(
    state: SharedState,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    released: Option<tokio::sync::oneshot::Sender<()>>,
) -> anyhow::Result<()> {
    // `bind` asked already; a caller with a listener of its own has not.
    if let Some(why) =
        crate::config::dev_prefix_refusal(state.dev(), &state.snapshot().settings.container_prefix)
    {
        anyhow::bail!(why);
    }
    let addr = listener.local_addr()?;
    // WebRTC calls' plain socket group (calls design §3.1): bound beside the
    // listener, closed with it before `released` fires.
    let realtime = state.snapshot().settings.realtime.webrtc;
    let released = state.calls.serve_plain(&realtime, addr, released).await;
    let app = build_router(state.clone());
    // axum drops the listener when the shutdown signal ends its accept loop,
    // which fires `released`.
    let listener = conn::PlainListener::new(listener, released);
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
    spawn_background_tasks(state.clone());
    // The device listener runs beside this one for as long as it serves
    // (device link design §4.1, §4.4): started from the `devices` settings
    // once this one is up — opening the CA may wait on the wallet's unlock
    // prompt, which must not hold the plain listener — and stopped by the
    // same stop, within the same bound, its own streams and sessions only.
    let shutdown = futures::FutureExt::shared(Box::pin(shutdown));
    let devices = {
        let (st, stopped) = (state.clone(), shutdown.clone());
        async move {
            let epoch = st.devices.listener.claim(&st).await;
            // Only the CA's opening is raced against the stop: the wallet's
            // prompt may wait for as long as nobody answers it, and a stop
            // must neither wait for it nor see the listener bind after it.
            // The start itself changes the listener's state and is never cut.
            let ca = tokio::select! {
                ca = st.devices.listener.open_ca(&st) => Some(ca),
                () = stopped.clone() => None,
            };
            if let Some(ca) = ca {
                st.devices.listener.start_claimed(&st, epoch, ca).await;
            }
            stopped.await;
            st.devices.listener.disarm(&st, epoch, STOP_WITHIN).await;
        }
    };
    let (served, ()) = tokio::join!(serve_app(&state, listener, app, shutdown), devices);
    served
}

/// Serve `app` until `shutdown` resolves, then stop within [`STOP_WITHIN`]
/// (`server::stop`): the long-lived streams opened on this server end at
/// once ([`Stops`]), the drain waits for the requests that finish by
/// themselves, then the sessions and turns opened on it are waited for —
/// all of it within the one bound, and what was still open when it ran out
/// said in the log. Public for the suites that shut a server down.
pub async fn serve_app<L>(
    state: &SharedState,
    listener: L,
    app: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()>
where
    L: axum::serve::Listener<Addr = std::net::SocketAddr>,
    for<'a> Conn: axum::extract::connect_info::Connected<axum::serve::IncomingStream<'a, L>>,
{
    serve_app_within(state, listener, app, shutdown, STOP_WITHIN).await
}

/// [`serve_app`] with its stop bounded by `within` instead of
/// [`STOP_WITHIN`]: for the suites that run a stop out of time.
pub async fn serve_app_within<L>(
    state: &SharedState,
    listener: L,
    app: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    within: std::time::Duration,
) -> anyhow::Result<()>
where
    L: axum::serve::Listener<Addr = std::net::SocketAddr>,
    for<'a> Conn: axum::extract::connect_info::Connected<axum::serve::IncomingStream<'a, L>>,
{
    stop::serve_stopping(state, listener, app, shutdown, within).await
}

/// The Chat's step of the hourly maintenance tick: the thread sweep
/// (auto-archive and purge, chat-archive-pin-attachments design §1) and the
/// change feed's retention (client-apps design §2.3). Public for the suites
/// that run the tick's step themselves.
pub async fn chat_upkeep(state: &crate::state::AppState) {
    let settings = state.snapshot().settings.clone();
    // Chat auto-archive + purge (chat-archive-pin-attachments design
    // §1). Two visible knobs, the same "0 disables that step" convention
    // as every other retention rule of the tick; pinned threads are
    // exempt from both, enforced in the query itself.
    match crate::store::sweep_chat_threads_ids(
        &state.db,
        settings.chat_archive_days,
        settings.chat_purge_days,
    )
    .await
    {
        Ok((0, purged)) if purged.is_empty() => {}
        Ok((archived, purged)) => {
            // The purged threads go as a delete makes them go — their live
            // events out of every device's reach, their turns cancelled, a
            // device's session closed — before the open feeds read the
            // records (client-apps design §2.2, §1.6's close-code note).
            state.chat_live.purged(&purged).await;
            state.chat_feed.wake();
            // Their open MCP tasks owe a cancel now (MCP Tasks design §1.5).
            state.mcp.resume_tasks().await;
            tracing::info!(
                "chat sweep: archived {archived}, purged {} threads",
                purged.len()
            )
        }
        Err(e) => tracing::warn!("chat sweep failed: {e}"),
    }
    // The deleted threads no live turn or session refers to any more.
    state.chat_live.feed().forget_gone();
    // The Chat change feed's records (client-apps design §2.3), by
    // their own visible setting; `0` keeps every record.
    match crate::store::feed::prune(&state.db, settings.chat_feed_retention_days).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            "pruned {n} chat feed records older than {} days \
             (Settings → Chat → Change feed → retention)",
            settings.chat_feed_retention_days
        ),
        Err(e) => tracing::warn!("chat feed pruning failed: {e}"),
    }
}

/// Periodic log pruning + llama-server status polling (§8, §10) and
/// restarting HF downloads interrupted by the last shutdown.
///
/// Once per state: [`serve`] calls it on every start, and the app's
/// "Restart gateway" serves again on the same state, which used to start
/// another reaper, status tick, pruner and boot pass beside the running
/// ones on each restart. The tasks belong to the state, not to a listener.
/// `false` when they ran already.
pub fn spawn_background_tasks(state: SharedState) -> bool {
    use std::sync::atomic::Ordering;
    if state.background.swap(true, Ordering::AcqRel) {
        tracing::debug!("background tasks run already; a restarted server shares them");
        return false;
    }
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

    // Agent-container reconciliation and the run-dir sweep (container-runtime
    // §6.4), beside the model runtime's boot and for the same reasons. Here
    // rather than in `AppState::init`: both act on `container_prefix`, and a
    // fresh data dir only gets its own prefix from the entry point's
    // dev-instance step, between `init` and this (chat-voice WP5 review B1).
    let st = state.clone();
    tokio::spawn(async move { crate::agents::container::boot_reconcile(&st).await });

    // The MCP server containers an earlier run left behind (MCP gateway
    // design §9), this instance's only, beside the agents' and for the same
    // reasons. The boot reconcile above may connect servers meanwhile; a
    // container younger than this process is never a leftover.
    let st = state.clone();
    tokio::spawn(async move { crate::mcp::container::boot_sweep(&st).await });

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
            chat_upkeep(&st).await;
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
    true
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

// `pub(crate)` and `JsonSchema`-derived so `openapi::planes::inference`
// (api-docs design §4.1 "`/api` query parameters ... the handler's `Query<T>`
// struct") can turn it into `GET /v1/audio/voices`'s one query parameter
// instead of hand-describing it a second time.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct VoicesQuery {
    #[serde(default)]
    model: String,
    /// Where the list comes from. Absent or `config`: a local audio model is
    /// answered from lmgw's own catalog (its presets, the voices its package
    /// ships, its embeddings and the voice library) without starting it.
    /// `engine`: ask the model's own server — a local model's container is
    /// started if it is down. A remote model is always asked.
    #[serde(default)]
    probe: Option<String>,
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
    let probe = match proxy::VoicesProbe::parse(q.probe.as_deref()) {
        Ok(p) => p,
        Err(msg) => {
            let err = GatewayError::BadRequest(msg);
            return (err.http_status(), Json(err.to_openai_json())).into_response();
        }
    };
    proxy::handle_audio_voices(state, q.model.trim(), probe).await
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
             rerankers; 'decide/…' local decision models (POST /v1/systemone); 'audio/…' local \
             audio.cpp models; 'image/…' local stable-diffusion.cpp \
             models; other prefixes are cloud upstreams passed through.",
            "A 503 whose error code is gpu_hold means local models are deliberately paused by \
             the owner; cloud aliases still work, and so do audio and aux models that run on \
             the CPU. \
             Do not retry in a loop.",
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

/// The listener a request came in on (`Via::Plain` for a router served
/// without connect info).
fn via_of(req: &Request) -> Via {
    req.extensions()
        .get::<axum::extract::ConnectInfo<Conn>>()
        .map_or(Via::Plain, |c| c.0.via)
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
    let mut models = crate::capabilities::exposed::exposed_entries(&state).await;
    // Chat models answer voice sessions once ASR and TTS are configured
    // (realtime design §12), and calls where the listener has them (realtime
    // calls design §9).
    if crate::realtime::advertise::speaks(&state).await {
        let calls = crate::realtime::advertise::calls(&state, via_of(&req));
        models
            .iter_mut()
            .for_each(|e| crate::realtime::advertise::advertise(e, calls));
    }
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
    let mut model = crate::capabilities::exposed::exposed_entry(&state, &id).await;
    if let Some(e) = model.as_mut() {
        if crate::realtime::advertise::speaks(&state).await {
            let calls = crate::realtime::advertise::calls(&state, via_of(&req));
            crate::realtime::advertise::advertise(e, calls);
        }
    }
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
