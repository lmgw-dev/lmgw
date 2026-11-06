//! JSON client for the gateway's HTTP planes.
//!
//! Paths are given as the gateway serves them; [`configure`] sets, once at
//! boot, the prefix put before every request (lmgw's own dashboard is served
//! from the root, so it sets none) and the hook run on a `401 session_required`.
//!
//! Every request can go through a transport its host hands the page
//! ([`transport`]): the free functions here are the browser's `fetch`
//! ([`Http::fetch`]); a page that may run in another host (the Chat) takes
//! the [`Http`] from context ([`use_http`]) and calls the same helpers on it.

pub mod bytes;
pub mod fake;
mod fetch;
pub mod sse;
pub mod transport;

pub use bytes::file_of;
pub use fetch::blob_bytes;
pub use transport::{
    use_http, Body, ByteStream, Http, Method, Payload, Read, Request, Response, Transport,
};

use lmgw_api_types::ApiError;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::fmt;
use std::sync::OnceLock;

/// What a host sets once at boot.
#[derive(Clone, Default)]
pub struct Config {
    /// Put before every path (`""` for a page served from the gateway's root).
    pub base: String,
    /// Run when the gateway answers `401 session_required`.
    pub on_unauthorized: Option<fn()>,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Set the client's base and unauthorized hook. Call once, before the first
/// request; a second call is ignored (the first wins).
pub fn configure(config: Config) {
    let _ = CONFIG.set(config);
}

fn config() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}

fn url(path: String) -> String {
    format!("{}{path}", config().base)
}

#[derive(Debug, Clone)]
pub enum Error {
    /// Server answered non-2xx with a structured [`ApiError`] body.
    Api(ApiError),
    /// Transport failure or a body that was not the expected shape.
    Transport(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api(error) => write!(f, "{}", error.message),
            Error::Transport(msg) => write!(f, "connection failed: {msg}"),
        }
    }
}

impl Error {
    /// The server answered `not_found`: what was asked for is not there
    /// (deleted, or never was). Any other failure is not that.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::Api(e) if e.code == "not_found")
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The non-2xx branch of [`checked`].
async fn refusal(resp: gloo_net::http::Response) -> Error {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    refused(status, &text)
}

/// A non-2xx answer's body read as the refusal it is, whatever the
/// transport.
///
/// One refusal is not about the request at all: `401 session_required` says
/// this browser holds no principal (principals §3.9). It raises the global
/// gate, and the shell swaps the page for the login card (§8) — every caller
/// still gets its `Err`, so nothing has to learn about sessions to be correct.
pub(crate) fn refused(status: u16, text: &str) -> Error {
    let error = parse_error_body(status, text);
    if status == 401 && error.code == "session_required" {
        session_required();
    }
    Error::Api(error)
}

/// Run the host's `401 session_required` hook ([`Config::on_unauthorized`]),
/// for a caller that reads a refusal itself rather than through the helpers
/// (the Chat's dictation upload, whose answer carries a header they drop).
pub fn session_required() {
    if let Some(hook) = config().on_unauthorized {
        hook();
    }
}

/// A response to a request the caller built itself with `gloo_net` (the
/// Audio lab's run, whose answer it reads as audio, JSON or a stream):
/// itself when it is a 2xx, else its refusal as every helper here reads it,
/// the `401 session_required` hook included. A request on the transport
/// reads its refusal with [`Http::ok`].
pub async fn checked(resp: gloo_net::http::Response) -> Result<gloo_net::http::Response> {
    if (200..300).contains(&resp.status()) {
        Ok(resp)
    } else {
        Err(refusal(resp).await)
    }
}

/// Best-effort parse of a non-2xx body into the house [`ApiError`] shape.
/// The `/api` admin plane always answers with the flat `{code, message}`
/// object, but the chat routes (`/chat/api/...`) are not all on the admin plane
/// yet: a refusal there can still arrive as a nested `{"error": {"code",
/// "message"}}` (proxied-upstream shapes) or a bare plain-text body. Every
/// caller still gets a human message instead of a bare status code — a live
/// check before this fix showed chips reading just "HTTP 413"/"HTTP 415".
pub fn parse_error_body(status: u16, text: &str) -> ApiError {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return ApiError {
            code: "http_error".into(),
            message: format!("HTTP {status}"),
            ..Default::default()
        };
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if let Ok(e) = serde_json::from_value::<ApiError>(v.clone()) {
            return e;
        }
        if let Some(e) = v
            .get("error")
            .and_then(|inner| serde_json::from_value::<ApiError>(inner.clone()).ok())
        {
            return e;
        }
    }
    ApiError {
        code: "http_error".into(),
        message: trimmed.to_string(),
        ..Default::default()
    }
}

pub async fn get<T: DeserializeOwned>(path: impl Into<String>) -> Result<T> {
    Http::fetch().get(path).await
}

/// GET a plain-text body rather than JSON — `/chat/api/attachments/{id}`,
/// which serves a text attachment's bytes as `text/plain` (chat-archive-
/// pin-attachments §2), not wrapped in an envelope. Failures are still the
/// house [`ApiError`] JSON everything else uses.
pub async fn get_text(path: impl Into<String>) -> Result<String> {
    Http::fetch().get_text(path).await
}

pub async fn post<T: DeserializeOwned, B: Serialize>(
    path: impl Into<String>,
    body: &B,
) -> Result<T> {
    Http::fetch().post(path, body).await
}

/// POST whose success is a bare `204` (the dashboard session route), which
/// sets the session cookie and has nothing else to say. The failure side is
/// the same [`ApiError`] as everywhere.
pub async fn post_no_content<B: Serialize>(path: impl Into<String>, body: &B) -> Result<()> {
    Http::fetch().post_no_content(path, body).await
}

/// POST a raw browser body — a `File` picked from an `<input type=file>`,
/// a `FormData`, a string — through the browser's `fetch`
/// ([`Http::fetch`]'s, so under the same base, with the same refusals).
///
/// Exists for the docs import route, which takes a corpus **file** rather than
/// JSON: the SQLite database is the unit of portability (quickdoc §3), so it
/// goes up as itself and not wrapped in an envelope. The answer is still the
/// house JSON, so failures arrive as the same [`ApiError`] everything else uses.
/// A page that may run in another host posts [`Body`] through
/// [`Http::post_body`] instead.
pub async fn post_raw<T: DeserializeOwned>(
    path: impl Into<String>,
    body: impl Into<wasm_bindgen::JsValue>,
) -> Result<T> {
    let resp = fetch::send_raw(Request::post(path), body.into())
        .await
        .map_err(Error::Transport)?;
    transport::json_of(transport::admit(resp).await?).await
}

/// POST JSON and take the answer as bytes with its content type — a route
/// whose success is audio (`/chat/api/profiles/speak`). A refusal is still
/// the house [`ApiError`] JSON.
pub async fn post_bytes<B: Serialize>(
    path: impl Into<String>,
    body: &B,
) -> Result<(Vec<u8>, String)> {
    Http::fetch().post_bytes(path, body).await
}
