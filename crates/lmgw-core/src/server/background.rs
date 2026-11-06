//! `x-lmgw-background: on|off` (per-request background design §3): one
//! request asks to be background traffic — a guest on the GPU, as if it had
//! come through a background candidate alias.
//!
//! Parsed once by `principal_mw`, and **only on its own routes**
//! (`openapi::takes_background`, the header table's `BACKGROUND_ROUTES`;
//! *changed 2026-10-22*, §11.12), then judged by those routes
//! ([`background_or_400`], and `/tokenize` and `/v1/systemone` in their own
//! error shape). Anywhere else it is not read: a typo on `GET /v1/models`
//! refuses nothing, and `/v1/realtime`, its WebRTC calls, the Chat and `/mcp`
//! — which hand their request's context on to TTS, ASR and the media tools
//! in-process — stay the owner's traffic whatever the client sent (§12).

use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::error::GatewayError;
use crate::proxy::RequestCtx;
use crate::runtime::registry::Origin;

/// Make this one request background traffic: `on` | `off`.
pub const BACKGROUND_HEADER: &str = "x-lmgw-background";

/// Read [`BACKGROUND_HEADER`]: `None` when absent, the [`Origin`] it asks
/// for, or — for any value but `on` and `off`, trimmed and in any case, as
/// `x-lmgw-reasoning` reads — the refusal's message, naming the header.
/// Never guessed at: `x-lmgw-background: yes` leaving a client believing its
/// work stays off the owner's models would be worse than a 400.
pub(super) fn background_from_headers(h: &HeaderMap) -> Option<Result<Origin, String>> {
    let v = h.get(BACKGROUND_HEADER)?;
    let Ok(v) = v.to_str() else {
        return Some(Err(format!(
            "{BACKGROUND_HEADER}: value must be printable ASCII"
        )));
    };
    Some(match v.trim().to_ascii_lowercase().as_str() {
        "on" => Ok(Origin::Background),
        "off" => Ok(Origin::Owner),
        other => Err(format!(
            "{BACKGROUND_HEADER}: expected 'on' or 'off', got '{other}'"
        )),
    })
}

/// The 400 a route that admits a local model owes a client whose
/// [`BACKGROUND_HEADER`] did not parse, in that route's own dialect — the
/// same `to_openai_json` / `to_anthropic_json` choice `reasoning_or_400`
/// makes.
pub(super) fn background_or_400(
    ctx: &RequestCtx,
    dialect: fn(&GatewayError) -> Value,
) -> Option<Response> {
    let e = ctx.background_refusal()?;
    Some((e.http_status(), Json(dialect(&e))).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn read(v: &str) -> Option<Result<Origin, String>> {
        let mut h = HeaderMap::new();
        h.insert(BACKGROUND_HEADER, HeaderValue::from_str(v).unwrap());
        background_from_headers(&h)
    }

    #[test]
    fn on_and_off_in_any_case_and_nothing_else() {
        assert_eq!(background_from_headers(&HeaderMap::new()), None);
        assert_eq!(read("on"), Some(Ok(Origin::Background)));
        assert_eq!(read(" ON "), Some(Ok(Origin::Background)));
        assert_eq!(read("off"), Some(Ok(Origin::Owner)));
        for bad in ["yes", "1", "true", ""] {
            let Some(Err(msg)) = read(bad) else {
                panic!("'{bad}' must be refused");
            };
            assert!(msg.starts_with("x-lmgw-background: "), "{msg}");
        }
    }
}
