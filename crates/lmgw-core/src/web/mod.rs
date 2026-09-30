//! The dashboard plane (§11): the Leptos SPA (`ui`), its JSON + SSE admin API
//! (`api`, `api_settings`, `api_agents`) and the three JSON mini-APIs the
//! user-facing surfaces talk to directly — Chat (`chat`), the Audio lab
//! (`audio_lab`) and the Image lab (`image_lab`).
//!
//! The remaining modules hold no routes at all: they are the shared internals
//! the API layer and `crate::ops` call (HF downloads, the wiring composition).
//! They lived next to the old askama/htmx page handlers, which were deleted at
//! the P8 cutover.

mod admin;
/// Service mode's reverse proxy (container-runtime §3.3, origins §4.2): the
/// agent origin's `Host` dispatch and `/agents/{id}/mcp`, on-demand start
/// included. Visible to [`crate::server`], which mounts the dispatch at the
/// root, outside CORS and inside the trace (§3.11).
pub(crate) mod agent_proxy;
mod agentchat;
/// `pub(crate)`: the openapi dashboard plane (`crate::openapi::planes::dashboard`
/// et al.) references this module's `Query`/`Json` extractor structs directly
/// for their `schemars::JsonSchema` impls (api-docs design §4.1), the same
/// reason [`api_usage`] already was.
pub(crate) mod api;
/// The agent catalog's HTTP face (agent-catalog design §5). Visible to
/// [`crate::ops`] for the same reason [`api_docs`] is: the `lmgw__agent*` tools
/// drive the dashboard's own import/read paths rather than a second copy of
/// them.
pub(crate) mod api_agents;
/// Visible to [`crate::ops`] for the same reason [`hf`] is: the `lmgw__docs_*`
/// tools create, ingest and re-embed corpora through the dashboard's own
/// handlers rather than a second copy of them.
pub(crate) mod api_docs;
/// `/api/knowledge/*` (chat-complete design §9.5): the Knowledge page's
/// backend.
mod api_knowledge;
/// `pub(crate)`: the openapi dashboard plane reads `SettingsFullPatch` and the
/// key-op patch structs' `JsonSchema` impls (api-docs design §4.1).
pub(crate) mod api_settings;
/// `/api/usage/*` (usage-analytics design §5): the query surface every chart
/// on the Usage page reads, so a chart cannot invent its own arithmetic.
pub mod api_usage;
mod audio;
mod audio_lab;
mod aux;
mod chat;
/// Thread-scoped actions that rewrite a conversation: Keep, and the message
/// actions (chat-complete design §3, §7).
mod chat_actions;
/// The `<file>` block format for Chat attachments (chat-archive-pin-
/// attachments design §2); the kinds themselves are `chat_attach_ingest`
/// (upload), `chat_attach_render` (into a request) and `chat_attach_gate`
/// (what a model can take).
mod chat_attach;
mod chat_attach_gate;
mod chat_attach_ingest;
mod chat_attach_render;
mod chat_attach_retry;
mod chat_attach_routes;
/// Chat folders (chat-complete design §5): their routes, the defaults'
/// checks, and starting a thread from a folder.
mod chat_export;
/// The Chat API's `Json` / `Query` / `Path` extractors, refused in the flat
/// `ApiError` shape.
mod chat_extract;
mod chat_folders;
/// Knowledge bases in Chat threads (chat-complete design §9.3): which bases a
/// turn uses, auto mode's retrieval and `<context>` block, the checks.
mod chat_knowledge;
/// One live turn per Chat thread, and the generation a turn's reply is
/// checked against before it is saved (review R1 finding 1).
pub(crate) mod chat_live;
mod chat_neutralise;
/// A Chat thread's `x-lmgw-reasoning*` overrides: their checks, their
/// control, and which of them the answering route drops.
mod chat_reasoning;
/// The seam every Chat handler reads and writes a thread through: the DB, or
/// the in-memory temporary threads (chat-complete design §7).
mod chat_repo;
mod chat_sampling;
/// Chat search (chat-complete design §4): the FTS route.
mod chat_search;
/// Temporary chats' in-memory store. `pub(crate)` because `AppState` holds
/// it.
pub(crate) mod chat_temp;
/// One assistant turn: `start_turn` (send, edit, regenerate, continue share
/// it), the request it builds, and its persist step.
mod chat_turn;
/// Visible to [`crate::ops`] so the tool plane drives the same download,
/// re-download and update-check paths as the dashboard rather than a parallel
/// implementation of them.
pub(crate) mod hf;
/// The Image lab's mini-API (image-generation design §8): the Audio lab's
/// sibling for `/v1/images/*`.
mod image_lab;
/// `POST /api/op/{name}`'s body extractor: no body is `{}`.
mod op_body;
/// The op dispatcher's vocabulary (api-docs design §4.7). `pub` because the
/// openapi op table (`crate::openapi::ops`) reads it to know which op names
/// it has to document, the same way it reads `server::CAPABILITY_TABLE`.
pub mod op_names;
mod responses;
/// The login (principals design §3.4): the four `/api/session*` routes, and
/// the one-shot nonce the Tauri shell opens its window with. `pub` because
/// the shell mints that nonce in-process — it hosts this crate — and because
/// `AppState` holds the desk it is minted from.
pub mod session;
mod ui;
mod wiring;

use axum::routing::{get, post};
use axum::Router;

use crate::state::SharedState;

pub fn routes(state: &SharedState) -> Router<SharedState> {
    Router::new()
        .merge(api::routes(state))
        .merge(api_agents::routes(state))
        // Before `ui::routes()`, whose `/{*path}` catch-all would otherwise
        // take `/agents/<id>/mcp` and the moved app mount. `ui::API_PREFIXES`
        // is deliberately NOT touched: adding `agents/` there would 404 the
        // SPA's own detail page at `/agents/<id>`, which a two-segment static
        // route already keeps.
        .merge(agent_proxy::routes(state))
        .merge(api_docs::routes(state))
        .merge(api_knowledge::routes(state))
        .merge(
            labs_routes().route_layer(crate::server::require(state, crate::principal::Cap::Admin)),
        )
        // Ahead of `ui::routes`, whose `/{*path}` catch-all takes every path
        // no other plane claimed: `api/` is one of its `API_PREFIXES`, so a
        // session route registered after it would answer `404` instead of
        // logging anyone in (principals §3.4).
        .merge(session::routes(state))
        // Last: the SPA owns `/` and every path no other plane claimed.
        .merge(ui::routes(state))
}

/// Chat, the Audio lab and the Image lab — the three surfaces the owner drives
/// by hand. All [`Cap::Admin`](crate::principal::Cap::Admin): they spend money
/// on the owner's aliases with no key of their own.
fn labs_routes() -> Router<SharedState> {
    // Bounded by `max_body_mb` like `/v1`, not unbounded like the audio/image
    // labs below: an attachment is a request-shaped upload (design §2). It
    // does *not* share `/v1`'s `body_limit_mw` layer, though — that
    // middleware's 413 is OpenAI/Anthropic-shaped, and this route answers in
    // this surface's own flat `ApiError` shape (review findings 2, 8), so
    // `chat::upload_attachment` reads and bounds its own body
    // (`chat::read_upload_body`). `DefaultBodyLimit::disable()` still has to
    // stay: without it axum's own 2 MiB default would reject a body before
    // the handler gets a chance to read it at all.
    let attachments = Router::new()
        .route(
            "/chat/api/threads/{id}/attachments",
            post(chat::upload_attachment),
        )
        .layer(axum::extract::DefaultBodyLimit::disable());

    Router::new()
        .route(
            "/chat/api/threads",
            get(chat::list_threads).post(chat::create_thread),
        )
        .route(
            "/chat/api/folders",
            get(chat_folders::list_folders).post(chat_folders::create_folder),
        )
        .route("/chat/api/folders/{id}", post(chat_folders::update_folder))
        .route(
            "/chat/api/folders/{id}/delete",
            post(chat_folders::delete_folder),
        )
        .route(
            "/chat/api/threads/{id}/move",
            post(chat_folders::move_thread),
        )
        .route("/chat/api/search", get(chat_search::search))
        .route(
            "/chat/api/threads/{id}/export",
            get(chat_export::export_thread),
        )
        .route(
            "/chat/api/folders/{id}/export",
            get(chat_export::export_folder),
        )
        .route("/chat/api/export", get(chat_export::export_all))
        .route("/chat/api/threads/{id}", get(chat::get_thread))
        .route("/chat/api/threads/{id}/settings", post(chat::update_thread))
        .route("/chat/api/threads/{id}/delete", post(chat::delete_thread))
        .route("/chat/api/threads/{id}/send", post(chat::send))
        .route("/chat/api/threads/{id}/pin", post(chat::pin_thread))
        .route("/chat/api/threads/{id}/archive", post(chat::archive_thread))
        .route(
            "/chat/api/threads/{id}/persist",
            post(chat_actions::persist_thread),
        )
        .route(
            "/chat/api/threads/{id}/continue",
            post(chat_actions::continue_reply),
        )
        .route(
            "/chat/api/threads/{id}/messages/{mid}/delete",
            post(chat_actions::delete_message),
        )
        .route(
            "/chat/api/threads/{id}/messages/{mid}/edit",
            post(chat_actions::edit_message),
        )
        .route(
            "/chat/api/threads/{id}/messages/{mid}/regenerate",
            post(chat_actions::regenerate_message),
        )
        .merge(attachments)
        .route(
            "/chat/api/attachments/{id}/delete",
            post(chat::delete_attachment),
        )
        .route("/chat/api/attachments/{id}", get(chat::get_attachment))
        .route(
            "/chat/api/attachments/{id}/mode",
            post(chat_attach_routes::set_mode),
        )
        .route(
            "/chat/api/attachments/{id}/transcribe",
            post(chat_attach_retry::transcribe),
        )
        .route(
            "/chat/api/attachments/{id}/text",
            get(chat_attach_routes::get_text),
        )
        // Audio lab: the audio.cpp playground (Chat's counterpart for
        // `/v1/audio/*`). Reference clips and TTS input can be arbitrarily
        // large, so the upload and dispatch routes drop axum's body limit the
        // same way `/v1/audio/*` does in `server::build_router`.
        .route("/audio-lab/api/models", get(audio_lab::list_models))
        .route("/audio-lab/api/voices", get(audio_lab::list_voices))
        .route(
            "/audio-lab/api/refs",
            get(audio_lab::list_refs)
                .post(audio_lab::upload_ref)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/audio-lab/api/refs/{name}", get(audio_lab::get_ref))
        .route(
            "/audio-lab/api/refs/{name}/delete",
            post(audio_lab::delete_ref),
        )
        .route(
            "/audio-lab/api/refs/{name}/text",
            post(audio_lab::set_ref_text),
        )
        .route(
            "/audio-lab/api/speech",
            post(audio_lab::speech).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/audio-lab/api/transcriptions",
            post(audio_lab::transcriptions).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/audio-lab/api/alignments",
            post(audio_lab::alignments).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/audio-lab/api/tasks/run",
            post(audio_lab::tasks_run).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        // Image lab: the sd-server playground (image-generation §8). Both
        // dispatch routes drop axum's body limit exactly as `/v1/images/*`
        // does in `server::build_router` — an edit carries whole images, and a
        // generation's answer is bounded by nothing but the picture.
        .route("/image-lab/api/models", get(image_lab::list_models))
        .route(
            "/image-lab/api/generate",
            post(image_lab::generate).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/image-lab/api/edit",
            post(image_lab::edit).layer(axum::extract::DefaultBodyLimit::disable()),
        )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Percent-encode for a URL path/query segment.
pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 32 hex chars of randomness for generated gateway keys.
pub(crate) fn rand_hex32() -> String {
    let n: u128 = rand::random();
    format!("{n:032x}")
}
