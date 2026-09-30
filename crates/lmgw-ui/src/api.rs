//! JSON client for the `/api` admin plane. Same origin as the SPA, and both
//! live at the server root, so paths here are plain `/api/...`.

use lmgw_api_types::ApiError;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::fmt;

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

pub type Result<T> = std::result::Result<T, Error>;

async fn decode<T: DeserializeOwned>(resp: gloo_net::http::Response) -> Result<T> {
    let status = resp.status();
    if (200..300).contains(&status) {
        resp.json::<T>()
            .await
            .map_err(|e| Error::Transport(format!("bad response body: {e}")))
    } else {
        Err(refusal(resp).await)
    }
}

/// The non-2xx branch, shared by [`decode`] and [`post_no_content`].
///
/// One refusal is not about the request at all: `401 session_required` says
/// this browser holds no principal (principals §3.9). It raises the global
/// gate, and the shell swaps the page for the login card (§8) — every caller
/// still gets its `Err`, so nothing has to learn about sessions to be correct.
async fn refusal(resp: gloo_net::http::Response) -> Error {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let error = parse_error_body(status, &text);
    if status == 401 && error.code == "session_required" {
        crate::session::lock();
    }
    Error::Api(error)
}

/// Best-effort parse of a non-2xx body into the house [`ApiError`] shape.
/// The `/api` admin plane always answers with the flat `{code, message}`
/// object, but the chat routes (`/chat/api/...`) are not all on that plane
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
    }
}

pub async fn get<T: DeserializeOwned>(path: impl Into<String>) -> Result<T> {
    let resp = gloo_net::http::Request::get(&path.into())
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    decode(resp).await
}

/// GET a plain-text body rather than JSON — `/chat/api/attachments/{id}`,
/// which serves a text attachment's bytes as `text/plain` (chat-archive-
/// pin-attachments §2), not wrapped in an envelope. Failures are still the
/// house [`ApiError`] JSON everything else uses.
pub async fn get_text(path: impl Into<String>) -> Result<String> {
    let resp = gloo_net::http::Request::get(&path.into())
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    let status = resp.status();
    if (200..300).contains(&status) {
        resp.text()
            .await
            .map_err(|e| Error::Transport(format!("bad response body: {e}")))
    } else {
        Err(refusal(resp).await)
    }
}

pub async fn post<T: DeserializeOwned, B: Serialize>(
    path: impl Into<String>,
    body: &B,
) -> Result<T> {
    let resp = gloo_net::http::Request::post(&path.into())
        .json(body)
        .map_err(|e| Error::Transport(e.to_string()))?
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    decode(resp).await
}

/// POST whose success is a bare `204`: `/api/session`, which sets the session
/// cookie and has nothing else to say. [`decode`] insists on a body, so this
/// is its own path — the failure side is the same [`ApiError`] as everywhere.
pub async fn post_no_content<B: Serialize>(path: impl Into<String>, body: &B) -> Result<()> {
    let resp = gloo_net::http::Request::post(&path.into())
        .json(body)
        .map_err(|e| Error::Transport(e.to_string()))?
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    if (200..300).contains(&resp.status()) {
        Ok(())
    } else {
        Err(refusal(resp).await)
    }
}

/// POST a raw browser body — a `File` picked from an `<input type=file>`.
///
/// Exists for `/api/docs/import`, which takes a corpus **file** rather than
/// JSON: the SQLite database is the unit of portability (quickdoc §3), so it
/// goes up as itself and not wrapped in an envelope. The answer is still the
/// house JSON, so failures arrive as the same [`ApiError`] everything else uses.
pub async fn post_raw<T: DeserializeOwned>(
    path: impl Into<String>,
    body: impl Into<wasm_bindgen::JsValue>,
) -> Result<T> {
    let resp = gloo_net::http::Request::post(&path.into())
        .body(body)
        .map_err(|e| Error::Transport(e.to_string()))?
        .send()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    decode(resp).await
}
