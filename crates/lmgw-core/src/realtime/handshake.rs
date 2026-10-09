//! The parts of the handshake that read headers (realtime design §10.1,
//! §10.2, §10.4): the browser's subprotocol credential, the beta refusal, the
//! anonymous cross-origin rule, the HTTP error shape, and the explicit
//! WebSocket size limits.

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::config::RealtimeSettings;

/// The subprotocol a browser carries its key in (§2.1).
const KEY_PROTOCOL_PREFIX: &str = "openai-insecure-api-key.";

/// The subprotocol the 101 selects when the client offered it (§10.1).
pub const REALTIME_PROTOCOL: &str = "realtime";

/// Every subprotocol the client offered, in order.
fn offered(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
}

/// The key a browser offered as `openai-insecure-api-key.<key>` — **on
/// `/v1/realtime` only** (§10.1). `server::principal_mw` asks every request,
/// and on any other path this answers `None`, so the subprotocol is a
/// credential on exactly one route.
pub fn subprotocol_key<'a>(path: &str, headers: &'a HeaderMap) -> Option<&'a str> {
    if path != super::PATH {
        return None;
    }
    offered(headers)
        .find_map(|p| p.strip_prefix(KEY_PROTOCOL_PREFIX))
        .filter(|k| !k.is_empty())
}

/// `OpenAI-Beta: realtime=v1` marks the legacy beta client, whose shapes are
/// not wire-compatible (§2.4).
pub fn is_beta_client(headers: &HeaderMap) -> bool {
    headers
        .get_all("openai-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case("realtime=v1"))
}

/// The `Origin` of a request that came from a page other than this gateway's
/// own (§10.1), or `None` — no `Origin` at all (an SDK client) or one naming
/// this very host.
///
/// Both schemes count as this host: the listener speaks `http`, but a TLS
/// proxy in front of it makes the same page `https://<host>`, and that page is
/// no more foreign for it.
pub fn foreign_origin(headers: &HeaderMap) -> Option<String> {
    let origin = headers.get(header::ORIGIN)?.to_str().unwrap_or("?").trim();
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();
    let same = !host.is_empty()
        && (origin == format!("http://{host}") || origin == format!("https://{host}"));
    (!same).then(|| origin.to_string())
}

/// A handshake refusal in the gateway's OpenAI error shape — before the 101
/// a realtime client is an ordinary HTTP client, and its SDK surfaces this
/// body (§10.2).
pub fn http_error(status: StatusCode, kind: &str, code: &str, message: String) -> Response {
    let body = json!({
        "error": { "message": message, "type": kind, "param": null, "code": code }
    });
    (status, Json(body)).into_response()
}

/// The 426 a request without WebSocket upgrade headers gets (§10.2) — axum's
/// own rejection would be a 400 that does not say what is missing. The
/// `Upgrade` header is the one RFC 9110 §15.5.22 asks a 426 to carry.
pub fn upgrade_required(why: &str) -> Response {
    let mut resp = http_error(
        StatusCode::UPGRADE_REQUIRED,
        "invalid_request_error",
        "upgrade_required",
        format!(
            "{} is a WebSocket route (OpenAI's GA Realtime protocol) and this request is not a \
             WebSocket upgrade: {why}",
            super::PATH
        ),
    );
    resp.headers_mut()
        .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    resp
}

/// The WebSocket size limits, in bytes (§10.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_message: usize,
    pub max_frame: usize,
    /// The settings' own numbers, for the close reason.
    pub message_mb: u32,
    pub frame_mb: u32,
    /// The frame limit is the message limit's: the frame setting is 0 (no
    /// bound of its own) or larger than the message setting.
    frame_by_message: bool,
    /// The two settings' names, as a close reason says them:
    /// `realtime.max_message_mb`, `realtime.max_frame_mb`.
    names: (&'static str, &'static str),
}

impl Limits {
    /// From the settings; `0` = no bound, which tungstenite spells as the
    /// largest size it can count.
    ///
    /// **A frame is always bounded.** tungstenite reserves a frame's
    /// *declared* length before it reads the payload, so with no frame bound
    /// one 14-byte header declaring 2^62 bytes aborts the process. A frame can
    /// never be larger than the message it belongs to, so the message limit
    /// is the frame's real bound whenever the frame setting is 0 (or larger).
    /// Both at 0 leaves nothing to bound a frame with: the settings loader
    /// refuses that pair and falls back to the defaults, saying so in the log
    /// (`store::load_settings`), and the same fallback applies here for any
    /// caller that bypassed it.
    pub fn from_settings(s: &RealtimeSettings) -> Self {
        let d = RealtimeSettings::default();
        Self::from_mb(
            (s.max_message_mb, s.max_frame_mb),
            (d.max_message_mb, d.max_frame_mb),
            ("realtime.max_message_mb", "realtime.max_frame_mb"),
        )
    }

    /// The device MCP host link's (client-apps design §5.1): the same rules,
    /// from `mcp.host_max_message_mb` and `mcp.host_max_frame_mb`.
    pub fn of_host_link(s: &crate::config::McpSettings) -> Self {
        let d = crate::config::McpSettings::default();
        Self::from_mb(
            (s.host_max_message_mb, s.host_max_frame_mb),
            (d.host_max_message_mb, d.host_max_frame_mb),
            ("mcp.host_max_message_mb", "mcp.host_max_frame_mb"),
        )
    }

    /// From a `(message, frame)` pair of settings in MiB, `defaults` for the
    /// pair both at 0, and the settings' `names`.
    fn from_mb(
        (message_mb, frame_mb): (u32, u32),
        defaults: (u32, u32),
        names: (&'static str, &'static str),
    ) -> Self {
        let (message_mb, frame_mb) = match (message_mb, frame_mb) {
            (0, 0) => defaults,
            pair => pair,
        };
        let bytes = |mb: u32| match mb {
            0 => usize::MAX,
            mb => usize::try_from(mb as u64 * 1024 * 1024).unwrap_or(usize::MAX),
        };
        let max_message = bytes(message_mb);
        let frame_by_message = frame_mb == 0 || (message_mb != 0 && frame_mb > message_mb);
        Self {
            max_message,
            max_frame: if frame_by_message {
                max_message
            } else {
                bytes(frame_mb)
            },
            message_mb,
            frame_mb,
            frame_by_message,
            names,
        }
    }

    /// The close reason for a read that tripped one of the limits, or `None`
    /// for any other read failure.
    ///
    /// tungstenite reports both as the same `MessageTooLong { size,
    /// max_size }`; which limit it was is read off `max_size`. A frame over
    /// the frame limit is caught as its header arrives, before its payload is
    /// buffered. The reason stays well under the 123 bytes a close frame
    /// allows.
    pub fn close_reason(&self, e: axum::Error) -> Option<String> {
        let inner = e.into_inner();
        let Some(tungstenite::Error::Capacity(tungstenite::error::CapacityError::MessageTooLong {
            size,
            max_size,
        })) = inner.downcast_ref::<tungstenite::Error>()
        else {
            return None;
        };
        Some(format!(
            "{size} bytes exceeds {}",
            self.setting_for(*max_size)
        ))
    }

    /// Which setting a `max_size` tungstenite reported is.
    fn setting_for(&self, max_size: usize) -> String {
        let (message_name, frame_name) = self.names;
        let message = format!("{message_name} ({} MiB)", self.message_mb);
        if self.frame_by_message || max_size != self.max_frame {
            // The frame limit is the message setting's, so that is the one
            // to raise.
            message
        } else if self.max_frame == self.max_message {
            // Equal limits cannot be told apart, and either one raised alone
            // would leave the other in the way — so both are named.
            format!(
                "{message_name} and {} ({} MiB)",
                frame_name.rsplit('.').next().unwrap_or(frame_name),
                self.message_mb
            )
        } else {
            format!("{frame_name} ({} MiB)", self.frame_mb)
        }
    }
}

/// RFC 6455's "message too big" close code.
pub const CLOSE_TOO_BIG: u16 = 1009;
