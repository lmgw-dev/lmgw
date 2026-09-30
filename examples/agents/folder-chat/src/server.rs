//! The web server: the UI, a small JSON API, two SSE streams and the MCP face.
//!
//! | Route | What |
//! |---|---|
//! | `GET /healthz` | `200 ok` as soon as the socket listens — the only unguarded route |
//! | `GET /`, `/app.css`, `/app.js` | the UI, compiled in (`include_str!`), no CDN, no fonts |
//! | `GET /api/status` | [`FolderChat::status`], the sync run, the config and the named constants |
//! | `POST /api/sync` | start a sync: `202 {run}`, or `409 sync_running` |
//! | `POST /api/sync/stop` | stop the running sync: `200 {run}` once it has stopped, or `409 no_sync` |
//! | `POST /api/vision/retry` | Retry failed pages: start a sync that first forgets every cached page failure: `202 {run}`, or `409 sync_running` |
//! | `GET /api/sync/events` | SSE: `run` (a run's header), then `sync` (one [`SyncEvent`] each) |
//! | `POST /api/chat` | SSE: `meta`, then `reasoning` / `text` deltas, `finish`, `usage` — or `error`; a body over [`ChatBodyLimit`] is `413 body_too_large` |
//! | `POST /mcp` | the MCP face ([`crate::mcp`]), through lmgw's `/agents/<id>/mcp` only |
//!
//! # Four guards, in this order
//!
//! Every route but `/healthz` passes all four. The agent origin asks for no
//! credential — lmgw dispatches it on the `Host` header with no principal, to
//! whoever can reach the gateway port (docs/agents.md §7, "The origin") — so
//! whom this app serves is this app's decision, made from the headers only
//! lmgw can set.
//!
//! **1. Provenance.** The request must carry exactly one `X-Forwarded-Host`,
//! equal to the authority of `LMGW_APP_ORIGIN` (`<id>.<suffix>:<port>`).
//! lmgw's proxy sets it — `insert`, never `append` — on both the agent origin
//! and the `/agents/<id>/mcp` face, and drops one a client supplies (§7, "The
//! proxy"). So a request that reaches the container's published loopback port
//! directly, from any local process, or a DNS-rebinding page that resolves
//! some name to `127.0.0.1` and talks to that port, arrives without it and is
//! refused `403 not_via_lmgw`. Defence in depth: the port is loopback-only and
//! ephemeral, but the guard costs nothing and names what it saw.
//!
//! **2. Face.** lmgw says which face a request came in on, in exactly one
//! `X-Lmgw-Face`: `mcp` through its Admin-gated `/agents/<id>/mcp`, `app` on
//! the agent origin. `X-Forwarded-Host` is the same on both, so without this
//! anyone who can reach the gateway port could send `POST /mcp` with this
//! agent's host name and call `read` on any file of the folder. `/mcp` needs
//! `mcp` (`403 not_mcp_face`); every other route needs `app`
//! (`403 not_app_face`), so lmgw's MCP client cannot drive the UI's API
//! either.
//!
//! **3. Client address** (the app face only). lmgw puts the TCP peer that
//! reached it in exactly one `X-Forwarded-For`. By default only a loopback
//! address — `127.0.0.0/8` or `::1` — is served: a browser on this machine.
//! Anything else is `403 remote_client`, which names the owner's switch, the
//! `allow_remote` config field ("Serve other machines"). **Another container
//! on this box counts as remote** when it reaches the gateway through
//! `host.containers.internal`: it arrives from the host's own interface
//! address, not loopback. (The exception is a container lmgw starts while the
//! gateway is bound to loopback: it reaches lmgw through pasta's `-T` forward
//! and arrives from `127.0.0.1`, so it counts as this machine.) A missing or
//! unparseable `X-Forwarded-For` is `403 not_via_lmgw`. The MCP face is not
//! address-checked: its peer is lmgw's own MCP client dialling itself, and its
//! trust is the Admin gate in front of it, not the address.
//!
//! **4. CSRF** (every method but `GET` and `HEAD`). The agent origin is public
//! to state-changing requests from any page a browser on this machine has open
//! — lmgw forwards them, the app has to judge them (§7, "The origin"). Such a
//! request must have:
//!
//! - `X-Folder-Chat: 1` — `403 csrf_header` otherwise. A custom header forces
//!   a CORS preflight, and this app sends **no CORS headers at all**, so the
//!   preflight fails and a page on another site can never send it. **The MCP
//!   face is exempt from this one**: MCP clients do not send it, `/mcp` is
//!   reached only through lmgw's Admin-gated `/agents/<id>/mcp` (rule 2), and
//!   the two rules below still apply to it.
//! - `Content-Type: application/json` (parameters such as `; charset=utf-8`
//!   allowed) — `403 csrf_content_type` otherwise. A cross-site form can only
//!   send `text/plain`, `application/x-www-form-urlencoded` or
//!   `multipart/form-data`, and a `no-cors` fetch only those same types; a
//!   JSON body from another origin needs the preflight that fails.
//! - An `Origin`, when a browser sends one, equal to `LMGW_APP_ORIGIN` —
//!   `403 csrf_origin` otherwise. Browsers attach `Origin` to every `POST`, so
//!   this refuses a cross-site request even if some browser let one of the
//!   rules above slip; server-side clients (lmgw's MCP client) send none.
//!
//! Every refusal is `403` with `{"error": {"code", "message"}}`, the message
//! naming the rule.
//!
//! # Framing
//!
//! lmgw's App tab shows this page in an iframe, so the CSP's
//! `frame-ancestors` names the dashboard's loopback origins at the gateway's
//! port — `127.0.0.1`, `localhost` and `[::1]`, the port taken from
//! `LMGW_APP_ORIGIN` — plus `'self'`, and nothing else ([`frame_ancestors`]).
//! The desktop window loads the dashboard from `http://127.0.0.1:<port>`.
//!
//! # SSE and shutdown
//!
//! Every SSE stream ends when the shutdown token fires, so a graceful shutdown
//! never waits on a browser tab. A chat answer runs **inside** its response
//! stream: a client that disconnects drops the stream, which drops the
//! retrieval or the upstream chat request mid-flight — lmgw sees its client go
//! away and cancels the model's generation.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use serde::Serialize;
use serde_json::json;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::app::Status;
use crate::budget::{BudgetError, DEFAULT_ANSWER_RESERVE_TOKENS, FALLBACK_CONTEXT_TOKENS};
use crate::chat::{default_search_params, AskError, ChatRequest, Depths, MAX_EXCERPTS};
use crate::gateway::{ChatChunk, GatewayError, ModelInfo};
use crate::hub::{HubItem, RunInfo, SyncHub};
use crate::scan::SkipReason;
use crate::sync::{AbortKind, SyncError, SyncEvent, EMBED_BATCH_SIZE};
use crate::FolderChat;

/// What lmgw's start probe asks for (`run.service.health_path`).
pub const HEALTH_PATH: &str = "/healthz";

/// The header every state-changing request from the UI carries.
pub const CSRF_HEADER: &str = "x-folder-chat";

/// The header lmgw's proxy names the face a request came in on with: `app`
/// on the agent origin, `mcp` through `/agents/<id>/mcp`.
pub const FACE_HEADER: &str = "x-lmgw-face";

/// The one path the MCP face serves (`run.provides.mcp`).
pub const MCP_PATH: &str = "/mcp";

/// How often an idle SSE stream sends a comment line, so an answer that waits
/// on a model loading its weights, or a sync events tab with nothing
/// happening, is not taken for a dead connection by anything in between.
pub const SSE_KEEP_ALIVE: Duration = Duration::from_secs(15);

/// The most bytes a `POST /api/chat` body can need per token of the chat
/// model's context; the body limit is `context_length ×` this
/// ([`ChatBodyLimit`]).
///
/// Why 48: a conversation the budget accepts fits the context as the budget's
/// estimator counts it ([`crate::chunk::estimator`], 4 characters a token),
/// and one character takes at most 12 bytes in a JSON string — a character
/// outside the Basic Multilingual Plane written as an escaped surrogate pair,
/// `\ud83d\ude00` (raw UTF-8 is at most 4 bytes, a `\u00e9` escape 6). 4 × 12
/// = 48. A message's own JSON framing (`{"role":"assistant","content":""},`,
/// 34 bytes) is covered by the framing tokens the budget counts per message
/// (4 × 48 = 192 bytes). So a body over the limit holds a conversation the
/// budget would refuse anyway; refusing it before it is read whole only saves
/// the memory. A test trips when the estimator's ratio changes.
pub const MAX_BYTES_PER_TOKEN: u64 = 48;

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_CSS: &str = include_str!("../ui/app.css");
const APP_JS: &str = include_str!("../ui/app.js");

/// The UI's own content security policy, less its `frame-ancestors`
/// ([`frame_ancestors`] adds that per origin): its script and style come from
/// this origin only, and nothing it renders — model output included — can
/// load or run anything else.
const CSP_BASE: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                        connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'";

/// The reason on the `aborted` event of a sync the owner stopped with
/// `POST /api/sync/stop` (the Stop sync button).
pub const OWNER_STOP_REASON: &str = "stopped by the owner";

/// The reason on the `aborted` event of a sync stopped by SIGTERM or SIGINT.
pub const SHUTDOWN_STOP_REASON: &str = "stopped because the app is shutting down";

/// The `kind` of the `aborted` event a stop from outside the sync ends with —
/// the owner's or shutdown's, told apart by the reason ([`SyncStop::reason`]).
/// A deliberate stop, not a failure, which the UI shows as a neutral notice
/// ("Sync stopped: stopped by the owner"); the next sync resumes from it.
pub const STOP_ABORT_KIND: AbortKind = AbortKind::Stopped;

#[derive(Debug, thiserror::Error)]
pub enum OriginError {
    #[error(
        "LMGW_APP_ORIGIN is '{0}', which is not an http(s) origin; lmgw sets it to \
         http://<id>.<suffix>:<port> in service mode"
    )]
    Invalid(String),
}

struct Shared {
    app: Arc<FolderChat>,
    hub: Arc<SyncHub>,
    /// `LMGW_APP_ORIGIN` without a trailing slash.
    origin: String,
    /// Its authority — what `X-Forwarded-Host` must say.
    authority: String,
    /// The agent id: the origin's first label, which lmgw makes the id itself
    /// (§7, "The origin") — for the messages that name `/agents/<id>/mcp`.
    id: String,
    /// [`CSP_BASE`] plus this origin's [`frame_ancestors`].
    csp: HeaderValue,
    shutdown: CancellationToken,
    task: Mutex<Option<RunningSync>>,
}

/// The sync task — its output says whether a stop ended it — and the way to
/// stop it with a reason.
struct RunningSync {
    handle: JoinHandle<bool>,
    stop: oneshot::Sender<SyncStop>,
}

/// Why a running sync is stopped from outside it ([`Server::stop_sync`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStop {
    /// `POST /api/sync/stop` — the Stop sync button.
    Owner,
    /// SIGTERM or SIGINT.
    Shutdown,
}

impl SyncStop {
    /// The `reason` of the `aborted` event the sync ends with.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Owner => OWNER_STOP_REASON,
            Self::Shutdown => SHUTDOWN_STOP_REASON,
        }
    }
}

/// The server: state, router, and the sync it runs.
#[derive(Clone)]
pub struct Server {
    shared: Arc<Shared>,
}

/// `http://folder-chat.localhost:8001` → (`http://folder-chat.localhost:8001`,
/// `folder-chat.localhost:8001`).
pub fn parse_origin(origin: &str) -> Result<(String, String), OriginError> {
    let trimmed = origin.trim().trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
        .ok_or_else(|| OriginError::Invalid(origin.to_string()))?;
    if rest.is_empty() || rest.contains('/') || rest.contains('@') {
        return Err(OriginError::Invalid(origin.to_string()));
    }
    Ok((trimmed.to_string(), rest.to_string()))
}

/// The CSP `frame-ancestors` directive for the app at `origin` (as
/// [`parse_origin`] returns it): `'self'` and lmgw's dashboard on the
/// loopback names at the gateway's port — the port of the agent origin's
/// authority, which is the gateway's own (the origin is served by the same
/// listener as the dashboard), in the origin's scheme.
///
/// `'none'` would break the App tab, which shows this page in an iframe; the
/// desktop window loads the dashboard from `http://127.0.0.1:<port>`. A
/// dashboard opened from a LAN address (`http://192.168.1.20:8001/`) is not
/// listed and cannot frame the app — its App tab shows a refused frame, and
/// "Open full page" opens the app at its own origin instead. Listing LAN
/// addresses would let any page served at them frame this one.
///
/// `http://folder-chat.localhost:8001` →
/// `frame-ancestors 'self' http://127.0.0.1:8001 http://localhost:8001 http://[::1]:8001`.
/// Without a port in the authority the scheme's default applies, and the
/// sources carry none.
pub fn frame_ancestors(origin: &str) -> String {
    let (scheme, authority) = origin.split_once("://").unwrap_or(("http", origin));
    let port = authority
        .rsplit_once(':')
        .map(|(_, p)| p)
        .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    format!(
        "frame-ancestors 'self' {scheme}://127.0.0.1{port} {scheme}://localhost{port} \
         {scheme}://[::1]{port}"
    )
}

impl Server {
    pub fn new(
        app: Arc<FolderChat>,
        app_origin: &str,
        shutdown: CancellationToken,
    ) -> Result<Self, OriginError> {
        let (origin, authority) = parse_origin(app_origin)?;
        let id = authority
            .split(['.', ':'])
            .next()
            .unwrap_or_default()
            .to_string();
        let csp = HeaderValue::from_str(&format!("{CSP_BASE}; {}", frame_ancestors(&origin)))
            .map_err(|_| OriginError::Invalid(app_origin.to_string()))?;
        Ok(Self {
            shared: Arc::new(Shared {
                app,
                hub: Arc::new(SyncHub::new()),
                origin,
                authority,
                id,
                csp,
                shutdown,
                task: Mutex::new(None),
            }),
        })
    }

    pub fn hub(&self) -> &Arc<SyncHub> {
        &self.shared.hub
    }

    pub fn authority(&self) -> &str {
        &self.shared.authority
    }

    pub fn router(&self) -> Router {
        let st = self.shared.clone();
        let mcp = crate::mcp::service(st.app.clone(), st.shutdown.clone());
        Router::new()
            .route("/", get(index))
            .route("/app.css", get(css))
            .route("/app.js", get(js))
            .route("/api/status", get(status))
            .route("/api/sync", post(sync_now))
            .route("/api/sync/stop", post(sync_stop))
            .route("/api/vision/retry", post(vision_retry))
            .route("/api/sync/events", get(sync_events))
            // Not axum's fixed 2 MB, which would refuse a long conversation
            // with a million-token model before the budget could say
            // anything: the handler reads the body itself, up to the limit
            // derived from the chat model's context ([`ChatBodyLimit`]).
            .route(
                "/api/chat",
                post(chat).layer(axum::extract::DefaultBodyLimit::disable()),
            )
            .route_service(MCP_PATH, mcp)
            .fallback(not_found)
            // Outermost last: provenance runs first, then face, then client
            // address, then CSRF.
            .layer(middleware::from_fn_with_state(st.clone(), csrf))
            .layer(middleware::from_fn_with_state(st.clone(), client_address))
            .layer(middleware::from_fn_with_state(st.clone(), face))
            .layer(middleware::from_fn_with_state(st.clone(), provenance))
            // After the layers, so the start probe needs no header.
            .route(HEALTH_PATH, get(healthz))
            .with_state(self.clone())
    }

    /// Start a sync unless one is running; `None` if one is. With `preload`,
    /// the index already on disk is loaded first (the start-up run), so
    /// questions are answered from it while the sync goes on.
    pub fn start_sync(&self, preload: bool) -> Option<u64> {
        self.start_sync_with(preload, false)
    }

    /// [`Server::start_sync`], with the owner's Retry failed pages first when
    /// `retry_failed` ([`FolderChat::sync_with`]).
    pub fn start_sync_with(&self, preload: bool, retry_failed: bool) -> Option<u64> {
        // Held until the task is in the slot, so a stop never finds the slot
        // empty while the run it is meant to stop is starting.
        let mut slot = self.shared.task.lock().unwrap_or_else(|p| p.into_inner());
        let run = self.shared.hub.try_begin()?;
        let (stop_tx, stop_rx) = oneshot::channel::<SyncStop>();
        let st = self.shared.clone();
        let handle = tokio::spawn(async move {
            // Ends the run however the task ends — a panic included — so a
            // failed sync can never leave every later one refused with 409.
            let _finish = FinishRun {
                hub: st.hub.clone(),
                run,
            };
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SyncEvent>();
            let hub = st.hub.clone();
            let pump = tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    hub.push(run, ev);
                }
            });
            let app = st.app.clone();
            let work = async move {
                if preload {
                    match app.load_existing().await {
                        Ok(true) => tracing::info!(
                            "loaded the index on disk; questions are answered from it while the sync runs"
                        ),
                        Ok(false) => {
                            tracing::info!("no usable index on disk yet; the sync builds one")
                        }
                        Err(e) => tracing::warn!("the index on disk could not be loaded: {e}"),
                    }
                }
                app.sync_with(tx, retry_failed).await
            };
            // A dropped sender (the slot replaced) is not a stop.
            let stopped = async move {
                match stop_rx.await {
                    Ok(why) => why,
                    Err(_) => std::future::pending().await,
                }
            };
            // `biased`: a sync whose work is already done ends as done, not
            // as stopped.
            let outcome = tokio::select! {
                biased;
                r = work => r,
                why = stopped => {
                    // `work` is dropped by now, at whatever await point it was
                    // at ([`Server::stop_sync`] says why the index stays
                    // consistent), and the event sender with it: the pump
                    // delivers what the sync sent before it stopped, so the
                    // `aborted` event is the run's last.
                    let _ = pump.await;
                    st.hub.push(
                        run,
                        SyncEvent::Aborted {
                            kind: STOP_ABORT_KIND,
                            reason: why.reason().to_string(),
                        },
                    );
                    tracing::warn!(run, "sync {}", why.reason());
                    return true;
                }
            };
            // The sender went with `sync`; the pump drains what is left.
            let _ = pump.await;
            match &outcome {
                Ok(r) => tracing::info!(
                    run,
                    new = r.new_files,
                    changed = r.changed_files,
                    removed = r.removed_files,
                    chunks = r.chunks,
                    elapsed_ms = r.elapsed_ms,
                    "sync done"
                ),
                Err(SyncError::Aborted { kind, reason, .. }) => {
                    tracing::warn!(run, ?kind, "sync stopped: {reason}")
                }
                Err(SyncError::AlreadyRunning) => {
                    tracing::warn!(run, "a sync was already running on this index")
                }
            }
            false
        });
        *slot = Some(RunningSync {
            handle,
            stop: stop_tx,
        });
        Some(run)
    }

    /// Stop the running sync at whatever await point it is at — for the Stop
    /// sync button ([`SyncStop::Owner`]) and for shutdown
    /// ([`SyncStop::Shutdown`]) — and wait until it has stopped. Its run ends
    /// with an `aborted` event: kind [`STOP_ABORT_KIND`], reason
    /// [`SyncStop::reason`]. `true` if a sync was running and this stopped
    /// it; `false` if none was, or it finished on its own first.
    ///
    /// Dropping the sync mid-way leaves the index consistent. The sync stores
    /// a file batch by batch ([`crate::sync::EMBED_BATCH_SIZE`] chunks, each
    /// batch as soon as it is embedded), then deletes the chunks the file no
    /// longer holds, and only then marks it current (its `folder_chat_file`
    /// row, written by [`crate::index::IndexDir::set_file_state`], last).
    /// Wherever the drop lands — after `upsert_document` moved the stored
    /// hash, during an embedding request, between two stored batches, before
    /// the stale chunks are deleted — the file's marker is missing or carries
    /// the old hash, so the next sync sees it as not current and finishes it,
    /// embedding only the chunks not stored yet (chunk ids are
    /// content-derived). Until then such a file can hold some new chunks
    /// beside its old ones. A running `pdftotext` is killed with the future
    /// (`kill_on_drop`).
    pub async fn stop_sync(&self, why: SyncStop) -> bool {
        let running = self
            .shared
            .task
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        match running {
            Some(r) if !r.handle.is_finished() => {
                // A send that fails means the sync already finished its work.
                let _ = r.stop.send(why);
                r.handle.await.unwrap_or(false)
            }
            _ => false,
        }
    }
}

/// Marks a run over when dropped.
struct FinishRun {
    hub: Arc<SyncHub>,
    run: u64,
}

impl Drop for FinishRun {
    fn drop(&mut self) {
        self.hub.finish(self.run);
    }
}

// ---------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------

fn refuse(code: &str, message: String) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

fn json_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message.into() } })),
    )
        .into_response()
}

async fn provenance(State(st): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    let got: Vec<&HeaderValue> = req.headers().get_all("x-forwarded-host").iter().collect();
    match got.as_slice() {
        [one]
            if one
                .to_str()
                .is_ok_and(|h| h.eq_ignore_ascii_case(&st.authority)) =>
        {
            next.run(req).await
        }
        [] => refuse(
            "not_via_lmgw",
            format!(
                "this request did not come through lmgw: it carries no X-Forwarded-Host. The app \
                 answers only on its agent origin, {}/, and on lmgw's MCP face (rule: provenance)",
                st.origin
            ),
        ),
        [one] => refuse(
            "not_via_lmgw",
            format!(
                "X-Forwarded-Host is '{}', not this agent's origin '{}'; the app answers only \
                 through lmgw (rule: provenance)",
                String::from_utf8_lossy(one.as_bytes()),
                st.authority
            ),
        ),
        _ => refuse(
            "not_via_lmgw",
            format!(
                "{} X-Forwarded-Host headers; lmgw sets exactly one (rule: provenance)",
                got.len()
            ),
        ),
    }
}

/// Rule 2: the face lmgw says the request came in on is the one this route is
/// served on.
async fn face(State(st): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    let is_mcp = req.uri().path() == MCP_PATH;
    let want = if is_mcp { "mcp" } else { "app" };
    let got: Vec<&HeaderValue> = req.headers().get_all(FACE_HEADER).iter().collect();
    let saw = match got.as_slice() {
        [one] if one.as_bytes() == want.as_bytes() => return next.run(req).await,
        [one] => format!(
            "it came in on lmgw's '{}' face",
            String::from_utf8_lossy(one.as_bytes())
        ),
        [] => "it carries no X-Lmgw-Face, which lmgw sets on every request it forwards".into(),
        many => format!(
            "it carries {} X-Lmgw-Face headers, and lmgw sets exactly one",
            many.len()
        ),
    };
    if is_mcp {
        refuse(
            "not_mcp_face",
            format!(
                "the MCP tools are served only through lmgw's admin-gated /agents/{}/mcp; {saw} \
                 (rule: face)",
                st.id
            ),
        )
    } else {
        refuse(
            "not_app_face",
            format!(
                "{} is served only on this agent's origin, {}/; {saw} (rule: face)",
                req.uri().path(),
                st.origin
            ),
        )
    }
}

/// Rule 3: on the app face, a client on this machine — or any client, when the
/// owner turned `allow_remote` on.
async fn client_address(State(st): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    // Rule 2 already made `/mcp` lmgw's Admin-gated face, whose peer is lmgw's
    // own MCP client and whose trust is that gate, not an address.
    if req.uri().path() == MCP_PATH {
        return next.run(req).await;
    }
    let got: Vec<&HeaderValue> = req.headers().get_all("x-forwarded-for").iter().collect();
    let ip = match got.as_slice() {
        [one] => match one
            .to_str()
            .ok()
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
        {
            Some(ip) => ip,
            None => {
                return refuse(
                    "not_via_lmgw",
                    format!(
                        "X-Forwarded-For is '{}', which is not one IP address; lmgw sets it to \
                         the address of the client that reached it (rule: client address)",
                        String::from_utf8_lossy(one.as_bytes())
                    ),
                )
            }
        },
        [] => {
            return refuse(
                "not_via_lmgw",
                "this request carries no X-Forwarded-For; lmgw sets it to the address of the \
                 client that reached it on every request to the agent origin (rule: client \
                 address)"
                    .into(),
            )
        }
        many => {
            return refuse(
                "not_via_lmgw",
                format!(
                    "{} X-Forwarded-For headers; lmgw sets exactly one (rule: client address)",
                    many.len()
                ),
            )
        }
    };
    if is_local(ip) || st.app.config().allow_remote {
        return next.run(req).await;
    }
    refuse(
        "remote_client",
        format!(
            "this app serves only the machine lmgw runs on, and this request came from {ip}. The \
             agent's owner can allow other machines with its 'Serve other machines' setting \
             (allow_remote) on the agent's Run tab; there is no login, so every machine that can \
             reach the gateway could then read and chat with the folder (rule: client address)"
        ),
    )
}

/// A loopback address, IPv4 or IPv6 — an IPv4-mapped `::ffff:127.0.0.1`
/// included, which lmgw does not send but a dual-stack socket would.
pub fn is_local(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

fn is_json(ct: Option<&HeaderValue>) -> bool {
    ct.and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
}

async fn csrf(State(st): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        return next.run(req).await;
    }
    let is_mcp = req.uri().path() == MCP_PATH;
    let h = req.headers();
    if !is_mcp && h.get(CSRF_HEADER).and_then(|v| v.to_str().ok()) != Some("1") {
        return refuse(
            "csrf_header",
            format!(
                "a {} request must carry the header 'X-Folder-Chat: 1', which a page on another \
                 site cannot send (rule: CSRF custom header)",
                req.method()
            ),
        );
    }
    if !is_json(h.get(header::CONTENT_TYPE)) {
        return refuse(
            "csrf_content_type",
            format!(
                "a {} request must be 'Content-Type: application/json', which a form or no-cors \
                 request from another site cannot send (rule: CSRF content type)",
                req.method()
            ),
        );
    }
    if let Some(o) = h.get(header::ORIGIN) {
        if o.to_str().ok().map(|o| o.trim_end_matches('/')) != Some(st.origin.as_str()) {
            return refuse(
                "csrf_origin",
                format!(
                    "the request comes from the page origin '{}', not from this app's own origin \
                     '{}' (rule: CSRF origin)",
                    String::from_utf8_lossy(o.as_bytes()),
                    st.origin
                ),
            );
        }
    }
    next.run(req).await
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn healthz() -> &'static str {
    "ok"
}

fn asset(s: &Server, content_type: &'static str, body: &'static str) -> Response {
    let mut r = body.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    // The UI is compiled into the binary: a new image is a new UI, and a
    // cached old one would talk to an API that moved.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CONTENT_SECURITY_POLICY, s.shared.csp.clone());
    r
}

async fn index(State(s): State<Server>) -> Response {
    asset(&s, "text/html; charset=utf-8", INDEX_HTML)
}

async fn css(State(s): State<Server>) -> Response {
    asset(&s, "text/css; charset=utf-8", APP_CSS)
}

async fn js(State(s): State<Server>) -> Response {
    asset(&s, "text/javascript; charset=utf-8", APP_JS)
}

async fn not_found(req: Request) -> Response {
    json_error(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("no route for {} {}", req.method(), req.uri().path()),
    )
}

/// The owner's choices, as the UI prints them.
#[derive(Serialize)]
struct ConfigView {
    embed_model: String,
    chat_model: String,
    rerank_model: Option<String>,
    /// The alias that reads PDF pages from their images; `None`: none is read.
    vision_model: Option<String>,
    vision_every_page: bool,
    chunk_tokens: usize,
}

/// Every named constant whose effect the UI shows, so the page prints the
/// number rather than a copy of it.
#[derive(Serialize)]
struct Constants {
    embed_batch_size: usize,
    max_excerpts: usize,
    fallback_context_tokens: u64,
    default_answer_reserve_tokens: u64,
    sse_keep_alive_seconds: u64,
    max_bytes_per_token: u64,
    /// quickdoc's `SearchParams` as a question runs with them — `k_fts`,
    /// `k_vec`, `rrf_k`, `k_rerank` (the MCP `search` raises `k_fts` and
    /// `k_vec` to its `k`).
    #[serde(flatten)]
    search: Depths,
}

/// The largest `POST /api/chat` body the server reads, and what it is derived
/// from: the chat model's `context_length` × [`MAX_BYTES_PER_TOKEN`], with
/// [`FALLBACK_CONTEXT_TOKENS`] standing in when the model reports none — the
/// context the budget assumes then, so the two refuse the same
/// conversations. Shown in `/api/status` and named in every `413`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatBodyLimit {
    pub bytes: u64,
    /// The chat model's context, in tokens.
    pub context_tokens: u64,
    /// `false` when the model reports no `context_length` and
    /// [`FALLBACK_CONTEXT_TOKENS`] stands in.
    pub context_reported: bool,
    /// [`MAX_BYTES_PER_TOKEN`].
    pub max_bytes_per_token: u64,
    /// The chat model alias the context is read from.
    pub chat_model: String,
}

impl ChatBodyLimit {
    pub fn for_model(info: &ModelInfo) -> Self {
        let (context_tokens, context_reported) = match info.context_length {
            Some(c) => (c, true),
            None => (FALLBACK_CONTEXT_TOKENS, false),
        };
        Self {
            bytes: context_tokens.saturating_mul(MAX_BYTES_PER_TOKEN),
            context_tokens,
            context_reported,
            max_bytes_per_token: MAX_BYTES_PER_TOKEN,
            chat_model: info.alias.clone(),
        }
    }

    /// The `413 body_too_large` for a body of `size` bytes (`None` when it
    /// declared none and ran past the limit while being read).
    fn refuse(&self, size: Option<u64>) -> Response {
        let size = match size {
            Some(n) => format!("{n} bytes"),
            None => format!("more than {} bytes", self.bytes),
        };
        let context = if self.context_reported {
            format!(
                "{}'s context_length of {} tokens",
                self.chat_model, self.context_tokens
            )
        } else {
            format!(
                "FALLBACK_CONTEXT_TOKENS ({}), because {} reports no context_length",
                self.context_tokens, self.chat_model
            )
        };
        json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            format!(
                "this request is {size}, over the limit of {} bytes for one question: \
                 {context} × MAX_BYTES_PER_TOKEN ({}). A conversation that fits the model's \
                 context is never that long; start a new chat",
                self.bytes, self.max_bytes_per_token
            ),
        )
    }
}

/// [`ChatBodyLimit`] as `/api/status` shows it: the limit, or why lmgw could
/// not say what the chat model's context is.
#[derive(Serialize)]
#[serde(untagged)]
enum LimitView {
    Known(ChatBodyLimit),
    Unknown { error: String },
}

#[derive(Serialize)]
struct StatusBody {
    #[serde(flatten)]
    status: Status,
    /// The current (or last) sync run of this process.
    sync: RunInfo,
    config: ConfigView,
    constants: Constants,
    /// The largest `POST /api/chat` body, as it stands for the chat model now.
    chat_body_limit: LimitView,
    /// What each skip reason means, for the skipped-by-reason table.
    skip_reasons: BTreeMap<SkipReason, &'static str>,
}

async fn status(State(s): State<Server>) -> Json<StatusBody> {
    let st = &s.shared;
    let c = st.app.config();
    let search = Depths::from(&default_search_params(c.rerank_model.is_some()));
    let chat_body_limit = match st.app.gateway().model_info(&c.chat_model).await {
        Ok(info) => LimitView::Known(ChatBodyLimit::for_model(&info)),
        Err(e) => LimitView::Unknown {
            error: e.to_string(),
        },
    };
    Json(StatusBody {
        status: st.app.status().await,
        sync: st.hub.info(),
        config: ConfigView {
            embed_model: c.embed_model.clone(),
            chat_model: c.chat_model.clone(),
            rerank_model: c.rerank_model.clone(),
            vision_model: c.vision_model.clone(),
            vision_every_page: c.vision_every_page,
            chunk_tokens: c.chunk_tokens,
        },
        constants: Constants {
            embed_batch_size: EMBED_BATCH_SIZE,
            max_excerpts: MAX_EXCERPTS,
            fallback_context_tokens: FALLBACK_CONTEXT_TOKENS,
            default_answer_reserve_tokens: DEFAULT_ANSWER_RESERVE_TOKENS,
            sse_keep_alive_seconds: SSE_KEEP_ALIVE.as_secs(),
            max_bytes_per_token: MAX_BYTES_PER_TOKEN,
            search,
        },
        chat_body_limit,
        skip_reasons: SkipReason::ALL.iter().map(|r| (*r, r.describe())).collect(),
    })
}

async fn sync_now(State(s): State<Server>) -> Response {
    match s.start_sync(false) {
        Some(run) => (StatusCode::ACCEPTED, Json(json!({ "run": run }))).into_response(),
        None => json_error(
            StatusCode::CONFLICT,
            "sync_running",
            "a sync is already running; its progress is on /api/sync/events",
        ),
    }
}

async fn sync_stop(State(s): State<Server>) -> Response {
    let run = s.shared.hub.info().run;
    if s.stop_sync(SyncStop::Owner).await {
        Json(json!({ "run": run })).into_response()
    } else {
        json_error(
            StatusCode::CONFLICT,
            "no_sync",
            "no sync is running, so there is nothing to stop",
        )
    }
}

/// Retry failed pages: start a sync that first forgets every cached page
/// failure — inside the sync, under its permit (`SyncOptions::retry_failed`),
/// so no other sync can have read the cache between the clearing and its
/// own plan. `409` while a sync runs.
async fn vision_retry(State(s): State<Server>) -> Response {
    match s.start_sync_with(false, true) {
        Some(run) => (StatusCode::ACCEPTED, Json(json!({ "run": run }))).into_response(),
        None => json_error(
            StatusCode::CONFLICT,
            "sync_running",
            "a sync is running; press Retry failed pages again once it has finished",
        ),
    }
}

fn event_json(name: &str, data: &impl Serialize) -> Event {
    Event::default().event(name).data(
        serde_json::to_string(data).unwrap_or_else(|e| {
            json!({ "code": "internal", "message": e.to_string() }).to_string()
        }),
    )
}

async fn sync_events(State(s): State<Server>) -> impl IntoResponse {
    let items = s
        .shared
        .hub
        .subscribe()
        .map(|item| {
            Ok::<_, Infallible>(match item {
                HubItem::Run(info) => event_json("run", &info),
                HubItem::Event(e) => event_json("sync", e.as_ref()),
            })
        })
        .take_until(s.shared.shutdown.clone().cancelled_owned());
    Sse::new(items).keep_alive(KeepAlive::new().interval(SSE_KEEP_ALIVE))
}

/// The `error` event's `code` for a question that could not be answered.
pub fn ask_error_code(e: &AskError) -> &'static str {
    match e {
        AskError::NoIndex => "no_index",
        AskError::IndexModelChanged { .. } => "index_model_changed",
        AskError::EmptyQuestion => "empty_question",
        AskError::Budget(BudgetError::ConversationTooLong { .. }) => "conversation_too_long",
        AskError::Budget(BudgetError::ReserveExceedsContext { .. }) => "context_too_small",
        AskError::Retrieval(_) => "retrieval",
        AskError::Gateway(g) => gateway_code(g),
    }
}

fn gateway_code(g: &GatewayError) -> &'static str {
    if g.is_gpu_hold() {
        "gpu_hold"
    } else {
        "gateway"
    }
}

fn error_event(code: &str, message: String) -> Event {
    event_json("error", &json!({ "code": code, "message": message }))
}

fn chunk_event(c: Result<ChatChunk, GatewayError>) -> Event {
    match c {
        Ok(ChatChunk::Text { text }) => event_json("text", &json!({ "text": text })),
        Ok(ChatChunk::Reasoning { text }) => event_json("reasoning", &json!({ "text": text })),
        Ok(ChatChunk::Finish { reason }) => event_json("finish", &json!({ "reason": reason })),
        Ok(ChatChunk::Usage { usage }) => event_json("usage", &usage),
        Err(e) => error_event(gateway_code(&e), e.to_string()),
    }
}

/// What `POST /api/chat` takes, for the 400 that says so.
const CHAT_BODY_SHAPE: &str = r#"{"history": [{"role", "content"}], "question"}"#;

/// An SSE response of `events`, ended by the shutdown token.
fn sse(s: &Server, events: BoxStream<'static, Event>) -> Response {
    let events = events
        .map(Ok::<_, Infallible>)
        .take_until(s.shared.shutdown.clone().cancelled_owned());
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(SSE_KEEP_ALIVE))
        .into_response()
}

/// Why a body was not read.
enum ReadError {
    TooLarge,
    Io(axum::Error),
}

/// Read `body` whole, refusing past `limit` bytes before holding more than
/// that.
async fn read_limited(body: Body, limit: u64, declared: Option<u64>) -> Result<Vec<u8>, ReadError> {
    let mut out = Vec::with_capacity(declared.unwrap_or(0).min(limit) as usize);
    let mut frames = body.into_data_stream();
    while let Some(frame) = frames.next().await {
        let frame = frame.map_err(ReadError::Io)?;
        if (out.len() + frame.len()) as u64 > limit {
            return Err(ReadError::TooLarge);
        }
        out.extend_from_slice(&frame);
    }
    Ok(out)
}

async fn chat(State(s): State<Server>, req: Request) -> Response {
    let app = s.shared.app.clone();
    // The limit is the chat model's, read now: the model behind the alias can
    // change while the app runs. A gateway that cannot say is the error the
    // question would have met anyway.
    let limit = match app.gateway().model_info(&app.config().chat_model).await {
        Ok(info) => ChatBodyLimit::for_model(&info),
        Err(e) => {
            let ev = error_event(gateway_code(&e), e.to_string());
            return sse(&s, stream::iter([ev]).boxed());
        }
    };
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if let Some(n) = declared.filter(|n| *n > limit.bytes) {
        return limit.refuse(Some(n));
    }
    let bytes = match read_limited(req.into_body(), limit.bytes, declared).await {
        Ok(b) => b,
        Err(ReadError::TooLarge) => return limit.refuse(None),
        Err(ReadError::Io(e)) => {
            let msg = format!("the body could not be read: {e}");
            return json_error(StatusCode::BAD_REQUEST, "bad_request", msg);
        }
    };
    let req: ChatRequest = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("the body must be {CHAT_BODY_SHAPE}: {e}");
            return json_error(StatusCode::BAD_REQUEST, "bad_request", msg);
        }
    };
    let events: BoxStream<'static, Event> = stream::once(async move { app.ask(&req).await })
        .flat_map(|res| match res {
            Err(e) => stream::iter([error_event(ask_error_code(&e), e.to_string())]).boxed(),
            Ok(answer) => stream::once(futures::future::ready(event_json("meta", &answer.meta)))
                .chain(answer.chunks.map(chunk_event))
                .boxed(),
        })
        .boxed();
    sse(&s, events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::MESSAGE_FRAMING_TOKENS;

    #[test]
    fn frame_ancestors_are_self_and_the_loopback_dashboard_at_the_gateway_port() {
        assert_eq!(
            frame_ancestors("http://folder-chat.localhost:8001"),
            "frame-ancestors 'self' http://127.0.0.1:8001 http://localhost:8001 \
             http://[::1]:8001"
        );
        // No port: the scheme's default, which a source without a port means.
        assert_eq!(
            frame_ancestors("https://folder-chat.example"),
            "frame-ancestors 'self' https://127.0.0.1 https://localhost https://[::1]"
        );
    }

    /// The derivation [`MAX_BYTES_PER_TOKEN`] documents, checked against the
    /// budget's own counting: the worst-case JSON body of a conversation is
    /// never more bytes than the tokens the budget counts for it × the
    /// constant. Trips when the estimator's ratio changes.
    #[test]
    fn a_conversation_of_t_budget_tokens_fits_t_times_max_bytes_per_token() {
        let tc = crate::chunk::estimator();
        assert_eq!(
            tc.count(&"\u{1F600}".repeat(4000)),
            1000,
            "4 characters a token"
        );
        // The widest character in JSON: outside the BMP, as an escaped
        // surrogate pair.
        let widest = "\\ud83d\\ude00";
        for n in [0usize, 1, 3, 4, 5, 4000] {
            let c = widest.repeat(n);
            let body = format!(
                r#"{{"history":[{{"role":"assistant","content":"{c}"}},{{"role":"user","content":""}}],"question":"{c}x"}}"#
            );
            let req: ChatRequest = serde_json::from_str(&body).unwrap();
            let history: u64 = req
                .history
                .iter()
                .map(|m| tc.count(&m.content) as u64 + MESSAGE_FRAMING_TOKENS)
                .sum();
            let question = tc.count(&crate::chat::user_message("", req.question.trim())) as u64
                + MESSAGE_FRAMING_TOKENS;
            let tokens = history + question;
            assert!(
                body.len() as u64 <= tokens * MAX_BYTES_PER_TOKEN,
                "{n} characters: {} bytes for {tokens} tokens",
                body.len()
            );
        }
    }

    #[test]
    fn the_body_limit_is_the_context_or_the_fallback_times_the_constant() {
        let reported = ChatBodyLimit::for_model(&ModelInfo {
            alias: "chat-small".into(),
            context_length: Some(4096),
            max_output_tokens: None,
        });
        assert_eq!(reported.bytes, 4096 * MAX_BYTES_PER_TOKEN);
        assert!(reported.context_reported);
        let fallback = ChatBodyLimit::for_model(&ModelInfo {
            alias: "chat-small".into(),
            context_length: None,
            max_output_tokens: None,
        });
        assert_eq!(
            fallback.bytes,
            FALLBACK_CONTEXT_TOKENS * MAX_BYTES_PER_TOKEN
        );
        assert!(!fallback.context_reported);
        let huge = ChatBodyLimit::for_model(&ModelInfo {
            alias: "x".into(),
            context_length: Some(u64::MAX),
            max_output_tokens: None,
        });
        assert_eq!(
            huge.bytes,
            u64::MAX,
            "saturates, never wraps to a tiny limit"
        );
    }
}
