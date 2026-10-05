//! Chat: a persisted "test the models" playground. The conversation, composer,
//! model picker and live generation stats are the SPA's Chat page; this module
//! is its server (`/chat/api/*`): a small JSON API over [`store`] plus a
//! streaming `send` endpoint that dispatches the chat **in-process** through
//! the gateway's own egress adapter + [`proxy::drive_upstream`] — no internal
//! HTTP hop, no gateway key, and the model resolves through
//! [`Snapshot::resolve`] exactly like an API client. Telemetry is recorded
//! (ingress proto `chat`), so chat turns appear in Logs.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::ApiError;
use serde::Deserialize;
use serde_json::{json, Value};

use super::agentchat::ADMIN_KIND;
use super::chat_extract::{ChatJson, ChatPath, ChatQuery};
use super::chat_repo::ChatRepo;
use super::chat_turn::{self, Reply, Turn, TurnMode, NOT_SAVED};
use super::{
    chat_attach_gate, chat_attach_ingest, chat_attach_retry, chat_knowledge, chat_reasoning,
    chat_sampling,
};
use crate::config::{Route, UpstreamKind};
use crate::egress::Egress;
use crate::error::GatewayError;
use crate::ir::{ChatRequest, Params, StreamDelta, Usage};
use crate::proxy::{self, drive_upstream};
use crate::state::SharedState;
use crate::store::{self, ChatThread, ThreadMcp};

mod stopped;

// ---------------------------------------------------------------------------
// Thread JSON API
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct ListThreadsQuery {
    /// Any non-empty, non-`"0"` value switches to the archived-only list
    /// (chat-archive-pin-attachments design §1) — `?archived=1` is what the
    /// UI's Chat page sends. The exact value `?archived=all` returns active
    /// *and* archived threads together (review finding 1: the Agent Runs tab
    /// needs an agent's archived threads too, and it filters this same list
    /// client-side by `agent_id`). Absent = active only.
    #[serde(default)]
    archived: String,
}

/// `GET /chat/api/threads` — active threads, pinned first then most-recently-
/// active. `GET /chat/api/threads?archived=1` — archived threads, most-
/// recently-archived first. `GET /chat/api/threads?archived=all` — both
/// together, in the active list's own order (pinned first, then
/// `updated_at DESC`). Every mode carries the same per-thread fields and
/// `archived_count`, so the sidebar's toggle (and the Agent Runs tab) can
/// label itself without a second round trip. Agent- and admin-kind threads
/// are not special-cased here or in the sweep — they archive and purge like
/// any other thread. A listed thread carries its `voice` but not
/// `voice_resolved`: that is for the open thread (`GET …/threads/{id}`).
///
/// Every mode also carries `temporary`: the temporary threads (chat-complete
/// design §7), most recently active first, in their own array — they are
/// never archived or pinned, and the sidebar lists them in a group of their
/// own.
pub async fn list_threads(
    State(state): State<SharedState>,
    ChatQuery(q): ChatQuery<ListThreadsQuery>,
) -> Response {
    let mode = match q.archived.as_str() {
        "all" => store::ThreadListMode::All,
        "" | "0" => store::ThreadListMode::Active,
        _ => store::ThreadListMode::Archived,
    };
    let threads = ChatRepo::stored_threads(&state, mode)
        .await
        .unwrap_or_default();
    let archived_count = ChatRepo::archived_count(&state).await.unwrap_or(0);
    let snap = state.snapshot();
    let out: Vec<Value> = threads.iter().map(|t| thread_row_json(t, &snap)).collect();
    let temporary: Vec<Value> = ChatRepo::temporary_threads(&state)
        .iter()
        .map(|t| thread_row_json(t, &snap))
        .collect();
    // Folders ride along in every mode (with their thread counts), so the
    // sidebar needs one request.
    let folders = store::list_chat_folders(&state.db)
        .await
        .unwrap_or_default();
    Json(json!({
        "threads": out,
        "archived_count": archived_count,
        "temporary": temporary,
        "folders": folders,
    }))
    .into_response()
}

#[derive(Deserialize, Default)]
pub struct CreateReq {
    #[serde(default)]
    model_alias: String,
    /// `"chat"` (default) or `"admin"` — see [`ADMIN_KIND`].
    #[serde(default)]
    kind: String,
    /// A temporary chat (chat-complete design §7): kept in memory only,
    /// never written to the DB unless it is kept (`…/persist`). Always a
    /// `chat` thread, whatever `kind` says.
    #[serde(default)]
    temporary: bool,
    /// Create the thread in this folder, starting from the folder's defaults
    /// laid over the global ones (chat-complete design §5). Not with
    /// `temporary`: a temporary chat has no folder.
    #[serde(default)]
    folder_id: Option<i64>,
}

/// `POST /chat/api/threads` — create a thread, returning it. A plain chat
/// thread starts from the default system prompt (Settings → Chat), as its own
/// copy; an Admin Chat thread has its built-in prompt already, and starts
/// with nothing of the thread's own to append to it. `{temporary: true}`
/// creates a temporary one, with a negative id; `{folder_id}` starts it from
/// that folder's defaults.
pub async fn create_thread(
    State(state): State<SharedState>,
    ChatJson(req): ChatJson<CreateReq>,
) -> Response {
    let kind = if req.kind == ADMIN_KIND && !req.temporary {
        ADMIN_KIND
    } else {
        "chat"
    };
    let snap = state.snapshot();
    let prompt = if kind == ADMIN_KIND {
        ""
    } else {
        snap.settings.default_chat_prompt()
    };
    let repo = if req.temporary {
        ChatRepo::Temp
    } else {
        ChatRepo::Db
    };
    if let Some(folder_id) = req.folder_id {
        if req.temporary {
            return err_json(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "a temporary chat cannot be created in a folder",
            );
        }
        return match super::chat_folders::create_in_folder(
            &state,
            folder_id,
            &req.model_alias,
            kind,
            prompt,
        )
        .await
        {
            Ok(t) => Json(thread_json(&state, &t).await).into_response(),
            Err(r) => r,
        };
    }
    match repo
        .create_thread(&state, &req.model_alias, kind, prompt)
        .await
    {
        Ok(t) => Json(thread_json(&state, &t).await).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// A [`ChatThread`] as the wire shape, plus the fields that aren't columns:
/// `purge_at` (`archived_at + chat_purge_days`), computed here rather than
/// stored so changing the setting reprices every archived thread's deadline
/// immediately instead of only the next time it is archived. `None` when
/// active, pinned, or purging is switched off (design §1) — a pinned thread
/// cannot really carry both flags at once (pinning restores), but the guard
/// costs nothing and keeps the promise literal either way. `temporary`:
/// whether the thread lives only in memory (chat-complete design §7). And
/// `voice_resolved`: what its voice resolves to now, field by field with
/// each value's source (chat-voice design §2.3), its speech style the one
/// its speech uses.
pub(super) async fn thread_json(state: &SharedState, t: &ChatThread) -> Value {
    let mut v = thread_row_json(t, &state.snapshot());
    v["voice_resolved"] = json!(super::chat_voice::resolve_shown(state, t).await);
    v
}

/// [`thread_json`] as the thread list carries it: without
/// `voice_resolved`, which only an open thread shows, so a list refresh
/// resolves nothing.
fn thread_row_json(t: &ChatThread, snap: &crate::config::Snapshot) -> Value {
    let purge_days = snap.settings.chat_purge_days;
    let mut v = serde_json::to_value(t).expect("ChatThread always serializes");
    let purge_at = (!t.pinned && purge_days > 0)
        .then_some(t.archived_at.as_deref())
        .flatten()
        .and_then(|a| store::chat_thread_purge_at(a, purge_days));
    v["purge_at"] = json!(purge_at);
    v["temporary"] = json!(ChatRepo::of(t.id).is_temp());
    v
}

/// The refusal of a stored-thread-only action (pin, archive) on a temporary
/// thread: it is discarded, not archived, and has nothing to pin until it is
/// kept.
fn temporary_refusal(what: &str) -> Response {
    err_json(
        StatusCode::CONFLICT,
        "temporary_thread",
        format!("a temporary chat cannot be {what} — Keep it first"),
    )
}

/// `GET /chat/api/threads/{id}` — thread settings + full message history,
/// each message carrying its own `attachments`, plus the thread's
/// `draft_attachments` (chat-archive-pin-attachments design §2). The thread
/// carries `continue: {ok, reason}` — whether its last reply can be continued
/// (chat-complete design §3).
pub async fn get_thread(State(state): State<SharedState>, ChatPath(id): ChatPath<i64>) -> Response {
    let repo = ChatRepo::of(id);
    let Ok(Some(thread)) = repo.thread(&state, id).await else {
        return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found");
    };
    let messages = repo.messages(&state, id).await.unwrap_or_default();
    let mut atts = repo.attachments_meta(&state, id).await.unwrap_or_default();
    chat_attach_gate::annotate_drafts(&state, &thread, &mut atts).await;
    let mut by_message: HashMap<i64, Vec<&store::ChatAttachmentMeta>> = HashMap::new();
    let mut drafts: Vec<&store::ChatAttachmentMeta> = Vec::new();
    for a in &atts {
        match a.message_id {
            Some(mid) => by_message.entry(mid).or_default().push(a),
            None => drafts.push(a),
        }
    }
    let last = messages.last();
    let messages: Vec<Value> = messages
        .iter()
        .map(|m| {
            let mut v = serde_json::to_value(m).expect("ChatMessageRow always serializes");
            v["attachments"] = json!(by_message.get(&m.id).cloned().unwrap_or_default());
            v
        })
        .collect();
    let snap = state.snapshot();
    let mut thread_v = thread_json(&state, &thread).await;
    thread_v["continue"] = json!(chat_turn::continue_state(&snap, &thread, last));
    Json(json!({
        "thread": thread_v,
        "messages": messages,
        "draft_attachments": drafts,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct PinReq {
    pinned: bool,
}

/// `POST /chat/api/threads/{id}/pin` — pinning an archived thread also
/// restores it (design §1).
pub async fn pin_thread(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<PinReq>,
) -> Response {
    let repo = ChatRepo::of(id);
    if repo.thread(&state, id).await.ok().flatten().is_none() {
        return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found");
    }
    if repo.is_temp() {
        return temporary_refusal("pinned");
    }
    if let Err(e) = repo.set_pinned(&state, id, req.pinned).await {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string());
    }
    respond_with_thread(&state, id).await
}

#[derive(Deserialize)]
pub struct ArchiveReq {
    archived: bool,
}

/// `POST /chat/api/threads/{id}/archive` — `{archived: true}` archives it by
/// hand; `{archived: false}` restores it (clears `archived_at`, bumps
/// `updated_at` — design §1).
pub async fn archive_thread(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<ArchiveReq>,
) -> Response {
    let repo = ChatRepo::of(id);
    if repo.thread(&state, id).await.ok().flatten().is_none() {
        return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found");
    }
    if repo.is_temp() {
        return temporary_refusal("archived");
    }
    let res = repo.set_archived(&state, id, req.archived).await;
    if let Err(e) = res {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string());
    }
    respond_with_thread(&state, id).await
}

/// The updated thread, re-read after a pin/archive write — simpler than
/// hand-tracking what each mutation changed, and correct even when
/// [`store::set_chat_thread_pinned`]'s implicit restore fired underneath it.
async fn respond_with_thread(state: &SharedState, id: i64) -> Response {
    match ChatRepo::of(id).thread(state, id).await {
        Ok(Some(t)) => Json(thread_json(state, &t).await).into_response(),
        _ => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the thread vanished immediately after being updated",
        ),
    }
}

/// A **patch**: an absent field leaves that setting alone.
///
/// The page patches this endpoint from two places with different halves — the
/// header's model picker sends only `model_alias`, the settings drawer sends
/// everything but — so "absent" has to mean "unchanged" or each save blanks
/// what the other owns. The sampling fields are doubly wrapped because `null`
/// is meaningful for them: absent keeps the value, `null` clears it back to the
/// upstream's own default.
#[derive(Deserialize)]
pub struct SettingsReq {
    model_alias: Option<String>,
    system_prompt: Option<String>,
    #[serde(default, deserialize_with = "present")]
    temperature: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    max_tokens: Option<Option<i64>>,
    /// Registered MCP servers this thread attaches.
    mcp_tools: Option<Vec<ThreadMcp>>,
    /// The reasoning overrides ([`super::chat_reasoning`]), wrapped like the
    /// sampling fields: `null` clears one back to the route's default.
    #[serde(default, deserialize_with = "present")]
    reasoning_enabled: Option<Option<bool>>,
    #[serde(default, deserialize_with = "present")]
    reasoning_effort: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    reasoning_budget: Option<Option<i64>>,
    /// The sampling overrides ([`super::chat_sampling`]), wrapped the same
    /// way; `stop` is the whole list (`[]` clears it).
    #[serde(default, deserialize_with = "present")]
    top_p: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    top_k: Option<Option<i64>>,
    #[serde(default, deserialize_with = "present")]
    min_p: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    repeat_penalty: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    presence_penalty: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    frequency_penalty: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    seed: Option<Option<i64>>,
    stop: Option<Vec<String>>,
    /// Knowledge bases ([`super::chat_knowledge`]): the whole selection
    /// (`[]` clears it), `"auto"` | `"tool"`, and the retrieval budget
    /// (`null` = the `chat_kb_budget_tokens` setting).
    kb_ids: Option<Vec<i64>>,
    kb_mode: Option<String>,
    #[serde(default, deserialize_with = "present")]
    kb_budget_tokens: Option<Option<i64>>,
    /// The voice overrides as a whole object (chat-voice design §2.2,
    /// [`super::chat_voice::apply_thread_voice`]); `null` clears them.
    #[serde(default, deserialize_with = "present")]
    voice: Option<Value>,
}

/// `Some(value)` for a field that was sent — including one sent as `null`,
/// which a bare `Option<Option<T>>` would flatten into "absent" and so make
/// clearing a sampling setting impossible.
fn present<'de, T, D>(de: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(de).map(Some)
}

/// `POST /chat/api/threads/{id}/settings` — patch model + sampling settings,
/// the reasoning overrides, the thread's attached MCP servers, its
/// knowledge bases and its voice (the title is preserved; it auto-names on first send). Overrides that contradict each
/// other are a 400 `bad_request`, and nothing is written. Answers `{ok,
/// continue, voice, voice_resolved}`: the thread's `continue` re-judged under
/// the new settings (a model switch or reasoning toggle changes it), its
/// voice as stored and what that resolves to now.
pub async fn update_thread(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<SettingsReq>,
) -> Response {
    let repo = ChatRepo::of(id);
    let Ok(Some(mut t)) = repo.thread(&state, id).await else {
        return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found");
    };
    if let Some(v) = req.model_alias {
        t.model_alias = v;
    }
    if let Some(v) = req.system_prompt {
        t.system_prompt = v;
    }
    if let Some(v) = req.temperature {
        t.temperature = v;
    }
    if let Some(v) = req.max_tokens {
        t.max_tokens = v;
    }
    if let Some(v) = req.mcp_tools {
        t.mcp_tools = v;
    }
    if let Some(v) = req.reasoning_enabled {
        t.reasoning_enabled = v;
    }
    if let Some(v) = req.reasoning_effort {
        t.reasoning_effort = v;
    }
    if let Some(v) = req.reasoning_budget {
        t.reasoning_budget = v;
    }
    if let Some(v) = req.top_p {
        t.top_p = v;
    }
    if let Some(v) = req.top_k {
        t.top_k = v;
    }
    if let Some(v) = req.min_p {
        t.min_p = v;
    }
    if let Some(v) = req.repeat_penalty {
        t.repeat_penalty = v;
    }
    if let Some(v) = req.presence_penalty {
        t.presence_penalty = v;
    }
    if let Some(v) = req.frequency_penalty {
        t.frequency_penalty = v;
    }
    if let Some(v) = req.seed {
        t.seed = v;
    }
    if let Some(v) = req.stop {
        t.stop = v;
    }
    if let Err(msg) = chat_reasoning::check(&mut t).and_then(|()| chat_sampling::check(&mut t)) {
        return err_json(StatusCode::BAD_REQUEST, "bad_request", msg);
    }
    if let Err(msg) = chat_knowledge::apply_settings(
        &state,
        &mut t,
        req.kb_ids,
        req.kb_mode,
        req.kb_budget_tokens,
    )
    .await
    {
        return err_json(StatusCode::BAD_REQUEST, "bad_request", msg);
    }
    let seed = match super::chat_voice::apply_thread_voice(&state, &mut t, req.voice).await {
        Ok(seed) => seed,
        Err(msg) => return err_json(StatusCode::BAD_REQUEST, "bad_request", msg),
    };
    match repo.update_settings(&state, &t, seed).await {
        Ok(voice) => {
            t.voice = voice;
            let last = repo.last_message(&state, id).await.ok().flatten();
            let snap = state.snapshot();
            let verdict = chat_turn::continue_state(&snap, &t, last.as_ref());
            Json(json!({
                "ok": true,
                "continue": verdict,
                "voice": t.voice,
                "voice_resolved": super::chat_voice::resolve_shown(&state, &t).await,
            }))
            .into_response()
        }
        // The thread went away between the read above and this write (a
        // delete in another tab, a temporary chat discarded or kept): the
        // 404 of any missing thread.
        Err(GatewayError::NotFound(msg)) => err_json(StatusCode::NOT_FOUND, "not_found", msg),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// `POST /chat/api/threads/{id}/delete`.
pub async fn delete_thread(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
) -> Response {
    let _ = ChatRepo::of(id).delete_thread(&state, id).await;
    Json(json!({ "ok": true })).into_response()
}

// ---------------------------------------------------------------------------
// Attachments (chat-archive-pin-attachments design §2)
// ---------------------------------------------------------------------------

/// Every chat route's refusal shape: [`lmgw_api_types::ApiError`] — the same
/// flat `{code, message}` body every other `/api` route answers with (review
/// finding 2). This surface used to nest its own `{"error":{code,message}}`
/// instead, one JSON shape a client had to special-case for exactly this one
/// group of routes; every refusal below goes through this now, including the
/// ones that used to be a bare status code or a plain-text body.
pub(super) fn err_json(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
) -> Response {
    (
        status,
        Json(ApiError {
            code: code.to_string(),
            message: message.into(),
        }),
    )
        .into_response()
}

/// The flat `body_limit` refusal (review findings 2 and 8): same code,
/// status and wording [`crate::server::body_limit_mw`] uses for `/v1`, just
/// in this surface's `ApiError` shape rather than the OpenAI/Anthropic one —
/// `/v1`'s shape is wrong for a route with no model or provider in the
/// picture.
fn body_limit_413(max_mb: u32) -> Response {
    err_json(
        StatusCode::PAYLOAD_TOO_LARGE,
        "body_limit",
        GatewayError::BodyTooLarge { max_mb }.to_string(),
    )
}

/// Read a request body under `max_body_mb`'s ceiling, in this route's own
/// `ApiError` shape.
///
/// Before this, the attachment upload route wore `body_limit_mw` like `/v1`
/// does, extracting its body as plain [`Bytes`]. That caught a *declared*
/// oversize body (a `Content-Length` over the limit) before the handler ever
/// ran, but a **chunked** body — no declared length at all — sailed past that
/// early check, and axum's own [`Bytes`] extractor tripped on the
/// `body_limit_mw`-installed [`http_body_util::Limited`] wrapper with its
/// generic plain-text "length limit exceeded" 413 instead of this API's named
/// one (review finding 8). Reading the body here, ourselves, with an explicit
/// limit is the one path that covers both cases as this route's own shape:
/// declared-oversize is still refused before a byte is read, and a chunked
/// overrun is caught by the same [`axum::body::to_bytes`] call that reads it.
pub(super) async fn read_upload_body(
    body: Body,
    headers: &HeaderMap,
    max_mb: u32,
) -> Result<Bytes, Response> {
    if max_mb == 0 {
        return axum::body::to_bytes(body, usize::MAX)
            .await
            .map_err(|e| err_json(StatusCode::BAD_REQUEST, "bad_request", e.to_string()));
    }
    let max_bytes = max_mb as u64 * 1024 * 1024;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max_bytes) {
        return Err(body_limit_413(max_mb));
    }
    let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    axum::body::to_bytes(body, max).await.map_err(|e| {
        match e
            .into_inner()
            .downcast::<http_body_util::LengthLimitError>()
        {
            Ok(_) => body_limit_413(max_mb),
            Err(e) => err_json(StatusCode::BAD_REQUEST, "bad_request", e.to_string()),
        }
    })
}

#[derive(Deserialize)]
pub struct UploadQuery {
    name: String,
}

/// `POST /chat/api/threads/{id}/attachments?name=FILENAME` — raw body, sniffed
/// into a kind (never trusted from `name` or a `Content-Type`), stored as a
/// draft. Body size is bounded by `max_body_mb`, read and enforced here
/// directly (see [`read_upload_body`]) rather than through the shared
/// `body_limit_mw` — that middleware's 413 is shaped for `/v1`, not for this
/// route's flat `ApiError` (review findings 2, 8).
pub async fn upload_attachment(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatQuery(q): ChatQuery<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let repo = ChatRepo::of(id);
    let Some(thread) = repo.thread(&state, id).await.ok().flatten() else {
        return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found");
    };
    let max_mb = state.snapshot().settings.max_body_mb;
    let body = match read_upload_body(body, &headers, max_mb).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    // Before sniffing: an empty upload is refused outright rather than
    // becoming an empty `text` attachment (review nit) — there is nothing a
    // model could do with a zero-byte "file".
    if body.is_empty() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "empty_attachment",
            "the file is empty",
        );
    }
    let name = q.name.trim();
    let name = if name.is_empty() { "untitled" } else { name };
    let new = match chat_attach_ingest::ingest(&state, &thread, name, body).await {
        Ok(n) => n,
        Err((status, code, msg)) => return err_json(status, code, msg),
    };
    match repo.insert_attachment(&state, id, &new).await {
        Ok(aid) => {
            let mut meta = [new.into_meta(aid, id)];
            chat_attach_gate::annotate_drafts(&state, &thread, &mut meta).await;
            Json(&meta[0]).into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// `POST /chat/api/attachments/{id}/delete` — drafts only; 409 once sent.
pub async fn delete_attachment(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
) -> Response {
    match ChatRepo::of(id).delete_draft(&state, id).await {
        Ok(store::DeleteAttachmentOutcome::Deleted) => Json(json!({ "ok": true })).into_response(),
        Ok(store::DeleteAttachmentOutcome::NotFound) => {
            err_json(StatusCode::NOT_FOUND, "not_found", "attachment not found")
        }
        Ok(store::DeleteAttachmentOutcome::AlreadySent) => err_json(
            StatusCode::CONFLICT,
            "attachment_sent",
            "this attachment was already sent with a message and can no longer be deleted",
        ),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// `GET /chat/api/attachments/{id}` — the raw bytes. `nosniff` +
/// `sandbox` so nothing uploaded can execute on the dashboard's own origin
/// (design §2) even though it is served from it.
pub async fn get_attachment(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
) -> Response {
    let att = match ChatRepo::of(id).attachment(&state, id).await {
        Ok(Some(a)) => a,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "not_found", "attachment not found"),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    let content_type = if att.kind == "text" {
        "text/plain; charset=utf-8".to_string()
    } else {
        att.mime.clone()
    };
    let mut resp = (StatusCode::OK, att.data).into_response();
    let headers = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&content_type) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("sandbox"),
    );
    resp
}

// ---------------------------------------------------------------------------
// Streaming send
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SendReq {
    content: String,
    /// Draft attachment ids this message binds, in the order the composer
    /// uploaded them (design §2). `content` may be empty when this is
    /// non-empty — a message can be "just a file".
    #[serde(default)]
    attachments: Vec<i64>,
    /// Knowledge bases picked with `#` for this message alone, on top of the
    /// thread's own (chat-complete design §9.3).
    #[serde(default)]
    kb_refs: Vec<i64>,
    /// A dictated message's `{via: "dictation", asr, asr_answered_by, asr_ms,
    /// audio_ms}` (chat-voice design §3, §5), stored on the user message.
    #[serde(default)]
    voice: Option<Value>,
    /// Read the reply aloud as it streams (chat-voice design §6.4).
    #[serde(default)]
    speak: bool,
}

/// `POST /chat/api/threads/{id}/send` — persist the user turn, then stream the
/// assistant reply as SSE (`turn` / `retrieval` / `delta` / `usage` / `tool` /
/// `error` / `done` events; [`chat_turn::start_turn`]). The client reads the
/// body with `fetch` + a stream reader.
pub async fn send(
    State(state): State<SharedState>,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<SendReq>,
) -> Response {
    let content = req.content.trim().to_string();
    // Dedup keeping the first occurrence: the list's order is the order the
    // model reads the files in.
    let mut attachment_ids: Vec<i64> = Vec::with_capacity(req.attachments.len());
    for id in &req.attachments {
        if !attachment_ids.contains(id) {
            attachment_ids.push(*id);
        }
    }
    if content.is_empty() && attachment_ids.is_empty() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "empty_message",
            "the message is empty",
        );
    }
    let voice = match req.voice.map(store::MessageVoice::dictation_from_input) {
        None => None,
        Some(Ok(v)) => Some(v),
        Some(Err(msg)) => return err_json(StatusCode::BAD_REQUEST, "bad_request", msg),
    };
    let repo = ChatRepo::of(id);
    let Ok(Some(thread)) = repo.thread(&state, id).await else {
        return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found");
    };
    let kb_refs = match chat_knowledge::check_kb_ids(&state, &req.kb_refs, &[]).await {
        Ok(ids) => ids,
        Err(msg) => return err_json(StatusCode::BAD_REQUEST, "bad_request", msg),
    };

    // Validate the draft ids up front, before anything is written: "ids must
    // be drafts of this thread, else 400" (design §2). A bad id must leave
    // the drafts exactly as they were, so the client can retry. This is a
    // best-effort pre-check for the ordinary "wrong/foreign/already-sent id"
    // case; the atomic bind below (finding 5) is what actually enforces it
    // against a concurrent send of the same drafts.
    let new_atts = if attachment_ids.is_empty() {
        Vec::new()
    } else {
        let found = repo
            .drafts_by_ids(&state, id, &attachment_ids)
            .await
            .unwrap_or_default();
        if found.len() != attachment_ids.len() {
            return err_json(
                StatusCode::BAD_REQUEST,
                "attachment_not_draft",
                "one or more attachment ids are not drafts of this thread",
            );
        }
        // The request's order, not upload order: it is the order they bind in.
        let mut found = found;
        found.sort_by_key(|a| attachment_ids.iter().position(|i| *i == a.id));
        found
    };

    // Vision only matters to a thread that has ever carried an image — the
    // brand-new ones just validated above already count.
    let caps = chat_attach_gate::thread_caps(&state, repo, &thread).await;
    let vision = caps.vision;
    // A *new* image and a model that cannot see it: refuse the whole send,
    // named, before any DB write (400 `model_no_vision`) — an image already
    // in history instead gets a placeholder when the request is actually
    // built.
    if vision == Some(false) && new_atts.iter().any(|a| a.kind == "image") {
        return err_json(
            StatusCode::BAD_REQUEST,
            "model_no_vision",
            format!(
                "'{}' does not accept images — remove the image or switch models",
                thread.model_alias
            ),
        );
    }

    // The other kinds' reasons (a PDF with no mode chosen, pages for a model
    // that cannot see them, audio nothing can hear or transcribe): the same
    // predicate the draft chips show as `blockers`.
    let mut new_atts = new_atts;
    chat_attach_retry::retry_failed(&state, &thread, &mut new_atts, caps).await;
    let stt_set = super::chat_voice::asr_alias(&state.snapshot(), &thread).is_some();
    let blocked: Vec<String> = new_atts
        .iter()
        .flat_map(|a| chat_attach_gate::blockers(a, &thread.model_alias, caps, stt_set))
        .collect();
    if !blocked.is_empty() {
        return err_json(
            StatusCode::UNPROCESSABLE_ENTITY,
            "attachment_blocked",
            blocked.join("; "),
        );
    }

    // Persist the user turn up front so a disconnect still records what was
    // asked, and name an untitled thread from its first message — the first
    // attachment's name when there is no text (design §2). Insert + bind are
    // one transaction (review finding 5): two concurrent sends validating the
    // same drafts above must not let the loser's bind match zero rows and
    // silently mail its message with none of the files it claimed — see
    // `store::append_user_message_with_attachments`. A DB failure here is a
    // 500, never a silent "sent with no attachments".
    let user_message_id = match repo
        .append_user_message(
            &state,
            id,
            &content,
            &attachment_ids,
            &kb_refs,
            voice.as_ref(),
        )
        .await
    {
        Ok(store::SendMessageOutcome::Sent(mid)) => mid,
        Ok(store::SendMessageOutcome::AttachmentNotDraft) => {
            return err_json(
                StatusCode::CONFLICT,
                "attachment_not_draft",
                "one or more attachments were no longer drafts of this thread by the time the \
                 message was saved — a concurrent send may have already used them",
            );
        }
        Err(e) => {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string());
        }
    };
    if thread.title == "New chat" || thread.title.trim().is_empty() {
        let title_source = if !content.is_empty() {
            content.clone()
        } else {
            new_atts.first().map(|a| a.name.clone()).unwrap_or_default()
        };
        let _ = repo
            .set_title(&state, id, &derive_title(&title_source))
            .await;
    }

    // Everything from the history onward — sending into an archived thread
    // restoring it included (design §1) — is the turn.
    let mode = TurnMode::Fresh {
        user_message_id: Some(user_message_id),
    };
    let speak = req.speak.then(super::chat_voice::ReadAloud::default);
    chat_turn::start_turn(&state, repo, &thread, mode, caps, speak).await
}

/// Open the upstream chat stream through the gate's send. A failure is the
/// row's status and the gateway error (the upstream's own status for an
/// error answer, which the row has always said). A request the gate sends
/// elsewhere before anything was sent — a ladder climb's fallback, a
/// candidate alias's next pick — comes back as
/// [`crate::gate::Sent::Rerouted`]; an upstream response is always a
/// successful one. `prompt_sent` says whether a request is out that the
/// upstream may be working on, for a stop's row ([`stopped`]).
#[allow(clippy::too_many_arguments)]
async fn open_upstream(
    state: &SharedState,
    hold: Option<&crate::vram::LocalHold>,
    route: &Route,
    lease: &mut crate::gate::TurnLease,
    ir: &ChatRequest,
    params: &Params,
    timeout: Option<Duration>,
    egress: &'static dyn Egress,
    prompt_sent: &std::sync::atomic::AtomicBool,
    fitted: &mut proxy::reasoning_fit::Fitted,
    fallback: Option<crate::gate::FallbackReason>,
) -> Result<crate::gate::Sent, (u16, GatewayError)> {
    // The connect goes through the shared dead-container retry (§3.2); the
    // relay that follows is the client's stream and is not replayable. On a
    // ladder row it only starts once the count says the request fits the
    // rung that answered. A refused off is retried before that (§5.6).
    use std::sync::atomic::Ordering;
    let sent = proxy::reasoning_fit::send_chat(
        state,
        hold,
        route,
        lease,
        ir,
        params,
        true,
        None,
        timeout,
        Some(prompt_sent),
        fitted,
        proxy::reasoning_fit::RowAs::InProcess {
            key: proxy::KeyRef::default(),
            ingress_proto: "chat",
            alias: &ir.model_alias,
            fallback,
        },
        |r, p| {
            prompt_sent.store(true, Ordering::Relaxed);
            egress.build_chat(&state.http, &r.upstream, &r.upstream_model, ir, p, true)
        },
    )
    .await
    .map_err(|e| (e.http_status().as_u16(), e))?;
    let crate::gate::Sent::Upstream(resp) = sent else {
        return Ok(sent);
    };
    let status = resp.status();
    if !status.is_success() {
        // An error answer: the upstream took no prompt to work on.
        prompt_sent.store(false, Ordering::Relaxed);
        let bytes = resp.bytes().await.unwrap_or_default();
        return Err((
            status.as_u16(),
            crate::gate::attribute(egress.map_error(status.as_u16(), &bytes), route),
        ));
    }
    Ok(crate::gate::Sent::Upstream(resp))
}

/// The spawned worker behind a plain turn ([`chat_turn::start_turn`]):
/// resolve → dispatch → relay deltas → persist the assistant turn → record
/// telemetry. A page that stops reading (Stop, a reload) still gets its
/// partial reply saved; every wait before the stream — resolve, GPU
/// admission, the gate's count and pool, the upstream's response — and the
/// stream itself give way the moment the turn is stopped or replaced by a
/// newer one, which drops the upstream request (review R1 finding 1).
pub(super) async fn run_send(
    state: SharedState,
    turn: Turn,
    mut ir: ChatRequest,
    tx: chat_turn::Events,
) {
    let emit = |ev: &'static str, data: String| {
        let tx = tx.clone();
        async move { tx.send(chat_turn::TurnFrame::new(ev, data)).await.is_ok() }
    };

    // Resolve the alias exactly like an API client would — including the GPU
    // hold's re-route to a fallback, or its refusal (gpu-hold design §2: the
    // Chat tab is interactive, somebody is waiting on it). The fallback alias
    // is not reported back to the island: this worker's SSE headers went out
    // before the resolve, so there is no `x-lmgw-fallback` to set, and the
    // dashboard reads the hold state from the titlebar's `vram` frame instead.
    // The turn's log row records it (`fallback_reason`), whichever swap it was,
    // and the saved reply its `answered_by`.
    let resolved = turn
        .or_stop(
            &tx,
            crate::gate::resolve(&state, &ir.model_alias, crate::gate::RouteCheck::None),
        )
        .await;
    let routed = match resolved {
        Ok(Ok(r)) => r,
        Ok(Err(f)) => {
            // The GPU hold or a benchmark refuses a local model with no
            // usable fallback here, before admission: `held` all the same.
            super::chat_voice::held_at_resolve(&tx, &ir.model_alias, &f.error).await;
            return chat_turn::refuse(&tx, &f.error).await;
        }
        Err(why) => return turn.report_stop(why, &tx).await,
    };

    let started = Instant::now();
    state.telemetry.request_started();

    // GPU admission (§9b), the gate's last per-request stage. The Chat tab is
    // real model traffic and takes its turn like any API client would; the
    // guard lives until this worker returns, which is after the last token has
    // been relayed and persisted. A local route comes back on the port its
    // container answers on (§5).
    // Images the thread's model may see are image parts by now (`vision`
    // turned the rest into placeholders): an outside-VRAM swap must not hand
    // them to a fallback that cannot, and a candidate alias refuses a facet
    // it does not enable (`Routed::using`).
    let uses = crate::gate::request_facets(&ir, None);
    let admitted = match routed.using(uses) {
        Ok(routed) => {
            let admit = super::chat_voice::admit_reporting(&state, routed, &ir.model_alias, &tx);
            turn.or_stop(&tx, admit).await
        }
        Err(f) => Ok(Err(f)),
    };
    let mut opened = match admitted {
        Ok(Ok(o)) => o,
        Ok(Err(f)) => {
            let e = f.error;
            if let Some(route) = &f.route {
                record_chat_call(
                    &state,
                    &ir.model_alias,
                    route,
                    f.headers.fallback_reason(),
                    started,
                    e.http_status().as_u16(),
                    None,
                    Usage::default(),
                    None,
                    None,
                    None,
                    Some((e.kind(), e.to_string())),
                )
                .await;
            } else {
                // Nothing to key a log row to (a refusal before a route was
                // settled on — not one admission makes today, but the gate's
                // contract allows it): close the gauge opened above.
                state.telemetry.request_abandoned();
            }
            let sent =
                chat_turn::SentAs::of(chat_turn::answered_by(&state.snapshot(), &f.headers), &ir);
            return chat_turn::refuse_sent(&tx, &e, sent).await;
        }
        Err(why) => {
            // Stopped while waiting for the GPU or a cold start: the
            // admission is dropped with the wait, and no call was made.
            state.telemetry.request_abandoned();
            return turn.report_stop(why, &tx).await;
        }
    };

    // Served once — and again on whatever the gate sends the request to
    // before anything was sent: a ladder climb's fallback (ladder design §12
    // entry 8), which has no hold and cannot hand it on again, or a candidate
    // alias's next pick (candidate-aliases §12 entry 45), which excludes one
    // more candidate each time.
    while let Some(next) = relay(&state, &turn, &mut ir, opened, started, &tx, &emit).await {
        opened = next;
    }
}

/// Everything [`run_send`] does after admission, on the route `opened`
/// settled on: the gate's per-send half, the send, the relay, the persisted
/// assistant turn and its log row. Returns the next admission when the gate
/// sent the request elsewhere before anything was sent.
async fn relay<E, Fut>(
    state: &SharedState,
    turn: &Turn,
    ir: &mut ChatRequest,
    opened: crate::gate::Opened,
    started: Instant,
    tx: &chat_turn::Events,
    emit: &E,
) -> Option<crate::gate::Opened>
where
    E: Fn(&'static str, String) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let crate::gate::Opened {
        route,
        hold: admission,
        headers,
    } = opened;
    let answered_by = chat_turn::answered_by(&state.snapshot(), &headers);
    // What a refusal below says the request went out as (`TurnFrame::sent`).
    let sent_as = chat_turn::SentAs::of(answered_by.clone(), ir);

    // Ask llama-server for per-token timings so the stats panel shows real
    // server-measured prefill/decode speeds live during generation (not client
    // estimates). Only llama.cpp understands this flag; cloud upstreams would
    // reject the unknown field, so gate it on the upstream kind — of the route
    // the gate settled on, which is the one the bytes go to. Never clobbers an
    // explicit value the alias may have set via passthrough, and is taken out
    // again should a fallback answer instead.
    let timings_key = "timings_per_token";
    let asked_timings = route.upstream.kind == UpstreamKind::LlamaServer
        && !ir.passthrough.contains_key(timings_key);
    if asked_timings {
        ir.passthrough
            .insert(timings_key.to_string(), serde_json::Value::Bool(true));
    }

    // Resolved after admission: everything below reads the *held* endpoint.
    let egress = crate::egress::for_protocol(route.upstream.protocol);
    let timeout = route.upstream.request_timeout();
    // The thread's own sampling choices, minus what this route cannot take —
    // the alias's defaults fill in around them untouched — and, for a
    // continue, the prefill this route has to take: the thread's own route
    // was checked before the turn started; this is the route the gate
    // settled on — a GPU-hold or outside-VRAM fallback, a ladder climb, a
    // candidate — refused by name rather than answered with a fresh message
    // appended to the reply being continued.
    let fit = match chat_turn::fit_route(&route, ir, (turn.is_continue(), turn.local_only())) {
        Ok(fit) => fit,
        Err(e) => {
            record_chat_call(
                state,
                &ir.model_alias,
                &route,
                headers.fallback_reason(),
                started,
                e.http_status().as_u16(),
                None,
                Usage::default(),
                None,
                None,
                None,
                Some((e.kind(), e.to_string())),
            )
            .await;
            chat_turn::refuse_sent(tx, &e, sent_as).await;
            return None;
        }
    };
    let mut params = fit.params.with_defaults(&route.param_defaults);
    let mut reasoning_ignored = fit.ignored;
    // An off this cloud model cannot take as asked goes out in the form it
    // can (model-capabilities design §5.6), and says so with the rest.
    let mut fitted = proxy::reasoning_fit::fit(state, &route, &mut params).await;
    let continued = if turn.is_continue() {
        chat_turn::mark_continuation(&route, ir)
    } else {
        Vec::new()
    };

    // The gate's per-send half (a guarded row's clamp, count and pool
    // reservation, a ladder's clamp, held until the stream below has been
    // drained) and the send itself, both given up the moment the turn is
    // stopped: dropping them drops the reservation and the request.
    let prompt_sent = std::sync::atomic::AtomicBool::new(false);
    let sent = turn
        .or_stop(tx, async {
            let (mut lease, gated) = crate::gate::fit_chat(
                state,
                admission.as_ref(),
                &route,
                ir,
                &mut params,
                true,
                None,
            )
            .await?;
            let opened = open_upstream(
                state,
                admission.as_ref(),
                &route,
                &mut lease,
                &gated,
                &params,
                timeout,
                egress,
                &prompt_sent,
                &mut fitted,
                headers.fallback_reason(),
            )
            .await;
            Ok::<_, crate::gate::FitRefusal>((lease, opened))
        })
        .await;
    let (lease, opened) = match sent {
        Ok(Ok(v)) => v,
        Ok(Err(f)) => {
            // The clamp (step 1) already ran even though the reservation
            // refused — this row must still carry it (review finding 7).
            record_chat_call(
                state,
                &ir.model_alias,
                &route,
                headers.fallback_reason(),
                started,
                f.error.http_status().as_u16(),
                None,
                Usage::default(),
                None,
                f.max_tokens_clamped,
                f.rung.as_ref().map(crate::gate::RungTag::log),
                Some((f.error.kind(), f.error.to_string())),
            )
            .await;
            chat_turn::refuse_sent(tx, &f.error, sent_as).await;
            return None;
        }
        Err(why) => {
            if prompt_sent.into_inner() {
                // Stopped while the upstream had the request: the prompt
                // it may be working on counts, as on the stock path.
                let (usage, note) = proxy::unanswered_usage(ir);
                record_chat_call(
                    state,
                    &ir.model_alias,
                    &route,
                    headers.fallback_reason(),
                    started,
                    200,
                    None,
                    usage,
                    None,
                    None,
                    None,
                    Some(("canceled", note)),
                )
                .await;
            } else {
                state.telemetry.request_abandoned();
            }
            turn.report_stop(why, tx).await;
            return None;
        }
    };
    let max_tokens_clamped = lease.max_tokens_clamped();
    let rung = lease.rung_log();
    fitted.report(&mut reasoning_ignored);
    let resp = match opened {
        Ok(crate::gate::Sent::Upstream(r)) => r,
        Ok(crate::gate::Sent::Rerouted(Ok(next))) => {
            drop((lease, admission, headers));
            if asked_timings {
                ir.passthrough.remove(timings_key);
            }
            for key in continued {
                ir.passthrough.remove(key);
            }
            return Some(next);
        }
        // A refusal instead (a candidate alias's deferral): this turn's row,
        // under the refusal's own headers.
        Ok(crate::gate::Sent::Rerouted(Err(f))) => {
            let e = f.error;
            let sent_as = chat_turn::SentAs {
                answered_by: chat_turn::answered_by(&state.snapshot(), &f.headers),
                ..sent_as
            };
            record_chat_call(
                state,
                &ir.model_alias,
                &route,
                f.headers.fallback_reason(),
                started,
                e.http_status().as_u16(),
                None,
                Usage::default(),
                None,
                None,
                None,
                Some((e.kind(), e.to_string())),
            )
            .await;
            chat_turn::refuse_sent(tx, &e, sent_as).await;
            return None;
        }
        Err((status, e)) => {
            record_chat_call(
                state,
                &ir.model_alias,
                &route,
                headers.fallback_reason(),
                started,
                status,
                None,
                Usage::default(),
                None,
                max_tokens_clamped,
                rung,
                Some((e.kind(), e.to_string())),
            )
            .await;
            chat_turn::refuse_sent(tx, &e, sent_as).await;
            return None;
        }
    };

    // Drain the stream: accumulate the assistant text and relay each delta as a
    // client event. Shares the normalized producer with the public proxy.
    let decoder = egress.new_decoder();
    let mut assistant = String::new();
    let mut reasoning = String::new();
    // Kept outside the stream's future, which a stop drops (`stopped`).
    let mut read = stopped::Read::default();
    let drained = drive_upstream(resp, decoder, timeout, started, &state.telemetry, |delta| {
        read.note(&delta, started);
        let (ev, data) = match &delta {
            StreamDelta::TextDelta(t) => {
                assistant.push_str(t);
                ("delta", json!({ "text": t }).to_string())
            }
            // Reasoning ("thinking") stream — kept separate from the answer so
            // the UI can show it in a collapsible block and a reasoning-only
            // turn (out of token budget) is still recorded, not blank.
            StreamDelta::ReasoningDelta(r) => {
                reasoning.push_str(r);
                ("reasoning", json!({ "text": r }).to_string())
            }
            StreamDelta::ToolCallStart { index, id, name } => (
                "tool",
                json!({ "event": "start", "index": index, "id": id, "name": name }).to_string(),
            ),
            StreamDelta::ToolCallArgsDelta { index, fragment } => (
                "tool",
                json!({ "event": "args", "index": index, "fragment": fragment }).to_string(),
            ),
            StreamDelta::Usage(u) => (
                "usage",
                json!({ "prompt_tokens": u.prompt_tokens, "completion_tokens": u.completion_tokens })
                    .to_string(),
            ),
            StreamDelta::Stop(r) => ("stop", json!({ "reason": r.to_openai() }).to_string()),
            // Server-measured prefill/decode timings (llama.cpp). Live per token
            // when `timings_per_token` is on, so the panel updates as it streams.
            StreamDelta::Timings(t) => ("stats", json!(t).to_string()),
            StreamDelta::Error(msg) => ("error", json!({ "message": msg }).to_string()),
        };
        emit(ev, data)
    });
    // Raced against the stop, not only noticed at the next delta: a long
    // prefill emits nothing, and the GPU should not finish it for nobody.
    let (outcome, stopped) = match turn.or_stop(tx, drained).await {
        Ok(o) => (o, None),
        Err(why) => (read.stopped(), Some(why)),
    };
    // The upstream has ended (`drive_upstream` consumed and dropped the
    // response): this send no longer occupies the pool — at once when it ran
    // to its end, once llama-server lets go of the slot otherwise (the
    // dashboard tab closed mid-answer, a stall, an upstream error).
    lease.end(outcome.completed);
    // The send is over, and so is its GPU claim: nothing below needs the
    // model. A heard voice turn's save waits for its user row, which waits
    // for its transcript — an ASR call that may need the very VRAM this
    // claim pins (voice-audio-input design §3.4; the tool loop lets go of
    // its claim at the same point).
    drop(admission);

    // Persist the assistant reply (even if partial / the client went away):
    // a new row, or the continued one grown by it — unless the thread moved
    // on meanwhile.
    let saved = turn
        .persist(
            state,
            Reply {
                text: &assistant,
                reasoning: &reasoning,
                prompt_tokens: outcome.usage.prompt_tokens.map(|v| v as i64),
                completion_tokens: outcome.usage.completion_tokens.map(|v| v as i64),
                ir_messages: None,
                answered_by: answered_by.clone(),
                stopped: outcome.aborted,
                failed: outcome.error.is_some(),
            },
        )
        .await;

    let status = if outcome.error.is_some() { 502 } else { 200 };
    // A stop (the turn's, or a reader gone) is the stock path's `canceled`
    // row with what the call cost so far (`stopped`).
    let (row_usage, row_error) = match stopped::usage(&outcome, ir, read.produced) {
        Some((usage, note)) => (usage, Some(("canceled", note))),
        None => (
            outcome.usage,
            outcome.error.as_ref().map(|(k, m)| (k.as_str(), m.clone())),
        ),
    };
    record_chat_call(
        state,
        &ir.model_alias,
        &route,
        headers.fallback_reason(),
        started,
        status,
        outcome.ttfb_ms,
        row_usage,
        outcome.timings,
        max_tokens_clamped,
        rung,
        row_error,
    )
    .await;

    // A caller that raised its own stop still reads: it gets `done`, which
    // names the partial reply just saved.
    if let Some(why) = stopped.filter(|w| *w != chat_turn::Stopped::Interrupted) {
        turn.report_stop(why, tx).await;
        return None;
    }
    // A stream that broke before it said anything saved nothing: `done
    // {aborted}`, as for an upstream that refused outright.
    if outcome.error.is_some() && assistant.is_empty() && reasoning.is_empty() {
        let _ = emit("done", json!({ "aborted": true }).to_string()).await;
        return None;
    }
    // A model that reasoned although off was asked — a local template that
    // cannot stop, a cloud model at its lowest level — keeps its reasoning
    // with the reply, and the turn says so (model-capabilities design §5.6).
    fitted.observe(&route, !reasoning.is_empty());
    fitted.report(&mut reasoning_ignored);
    let reasoning_note = fitted.note(answered_by.as_deref().unwrap_or(turn.model()));
    let total_ms = started.elapsed().as_millis() as i64;
    if saved.refused {
        let _ = emit(
            "error",
            json!({ "message": NOT_SAVED, "code": "not_saved" }).to_string(),
        )
        .await;
    }
    let _ = emit(
        "done",
        json!({
            "message_id": saved.id,
            // False: nothing of this reply is stored (its bubble is not a
            // row, and no action on it can work).
            "saved": saved.saved(),
            // What answered: the thread's model, and the alias that took
            // its place (a fallback, a candidate's pick), when one did.
            "model": turn.model(),
            "answered_by": answered_by,
            "prompt_tokens": outcome.usage.prompt_tokens,
            "completion_tokens": outcome.usage.completion_tokens,
            "ttfb_ms": outcome.ttfb_ms,
            "total_ms": total_ms,
            "aborted": outcome.aborted,
            // Authoritative final timings (llama.cpp); null for cloud upstreams.
            "timings": outcome.timings,
            // The thread's reasoning and sampling overrides this route did
            // not send.
            "reasoning_ignored": reasoning_ignored,
            // The model reasoned although off was asked, in a sentence.
            "reasoning_note": reasoning_note,
        })
        .to_string(),
    )
    .await;
    None
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// First line of the first user message, clipped to a sidebar-friendly length.
pub(super) fn derive_title(s: &str) -> String {
    let first = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let clipped: String = first.chars().take(48).collect();
    if clipped.is_empty() {
        "New chat".to_string()
    } else if first.chars().count() > 48 {
        format!("{clipped}…")
    } else {
        clipped
    }
}

/// Log a chat turn so it shows up in the Logs tab alongside API traffic.
/// Mirrors the proxy's request log with ingress proto `chat`.
#[allow(clippy::too_many_arguments)]
async fn record_chat_call(
    state: &SharedState,
    model: &str,
    route: &Route,
    fallback: Option<crate::gate::FallbackReason>,
    started: Instant,
    status: u16,
    ttfb_ms: Option<i64>,
    usage: Usage,
    timings: Option<crate::ir::Timings>,
    max_tokens_clamped: Option<u32>,
    rung: Option<i64>,
    error: Option<(&str, String)>,
) {
    // The chat *stream* itself can't yet share `proxy::sample_once` (that helper
    // is non-streaming; chat is SSE) — but its **log row** routes through the
    // shared in-process recorder so chat/workflows/sampling write one identical
    // row shape (§8 "one path instead of three copies"). TODO(one-path, §8):
    // unify the streaming call path too if/when sample_once grows a stream mode.
    proxy::record_in_process(
        proxy::InProcessLog {
            key: proxy::KeyRef::default(),
            ingress_proto: "chat",
            alias: model,
            route,
            started,
            streamed: true,
            class: crate::telemetry::RequestClass::Chat,
            timings,
            max_tokens_clamped,
            fallback,
            rung,
        },
        status,
        ttfb_ms,
        usage,
        error,
        state,
    )
    .await;
}
