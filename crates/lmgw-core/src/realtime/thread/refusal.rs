//! A bind's refusal, typed (realtime calls design §2.5, §2.8): the
//! WebSocket answers it before the 101 in the handshake's OpenAI error
//! shape ([`Refused::into_response`]); a call answers it at its POST the
//! same way, and at its channel's open as `lmgw.session.closed` or an
//! `error` event ([`Refused::close_kind`], [`Refused::error_object`]).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::realtime::ClosedKind;
use lmgw_api_types::OpenAiError;

use super::super::protocol::ErrorObject;
use crate::ingress::ClientProto;
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::web::chat_voice::bound::Caller;

/// A refused bind: what the handshake answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    pub status: StatusCode,
    /// The error's `type`: `invalid_request_error`, `server_error`, or a
    /// gateway refusal's own.
    pub kind: String,
    pub code: String,
    pub message: String,
    /// Who holds the thread, on `409 chat_thread_bound` only (§2.8): named
    /// as the feed's `voice.bound.by` names a client.
    pub held_by: Option<String>,
}

impl Refused {
    /// A refusal of lmgw's own: `invalid_request_error`, or `server_error`
    /// for a 5xx.
    pub(crate) fn new(status: StatusCode, code: &str, message: String) -> Self {
        let kind = if status.is_server_error() {
            "server_error"
        } else {
            "invalid_request_error"
        };
        Self {
            status,
            kind: kind.into(),
            code: code.into(),
            message,
            held_by: None,
        }
    }

    /// A gateway refusal — a key's scope, budget or rate — with the status,
    /// type, code and words its HTTP form has.
    pub(crate) fn gateway(e: &crate::error::GatewayError) -> Self {
        Self {
            status: e.http_status(),
            kind: e.openai_type().into(),
            code: e.code().into(),
            message: e.to_string(),
            held_by: None,
        }
    }

    /// The handshake's answer: `{"error": {message, type, param, code}}`,
    /// with `held_by` beside them on a `409 chat_thread_bound`.
    pub(crate) fn into_response(self) -> Response {
        let mut body = OpenAiError::new(self.message, &self.kind, &self.code);
        body.error.held_by = self.held_by;
        (self.status, Json(body)).into_response()
    }

    /// The refusal as an open session's `error` event says it: its type,
    /// code and words.
    pub(crate) fn error_object(&self) -> ErrorObject {
        ErrorObject {
            kind: self.kind.clone(),
            code: Some(self.code.clone()),
            message: self.message.clone(),
            param: None,
            event_id: None,
        }
    }

    /// How a bound call that this refuses when its channel opens ends
    /// (§2.8, §8.2): `thread_bound` for a held thread, `out_of_reach` for a
    /// thread or folder that is not there for the caller; `None` for every
    /// other refusal, which an `error` event explains before
    /// `lmgw.session.closed {kind: "closed"}`.
    pub(crate) fn close_kind(&self) -> Option<ClosedKind> {
        match self.code.as_str() {
            "chat_thread_bound" => Some(ClosedKind::ThreadBound),
            "chat_thread_not_found" | "chat_folder_not_found" => Some(ClosedKind::OutOfReach),
            _ => None,
        }
    }
}

/// A refusal of lmgw's own (module doc of `bind`): a device's writes its
/// request row first, as realtime's own handshake refusals do (client-apps
/// design §1.3); the owner's is the dashboard's own request, and writes none
/// (chat-voice §8.8, NIT 12).
pub(super) async fn refuse(
    state: &SharedState,
    ctx: &RequestCtx,
    (status, code): (StatusCode, &'static str),
    message: String,
) -> Refused {
    if Caller::of(ctx).is_device() {
        let e = crate::error::GatewayError::Refused {
            status: status.as_u16(),
            code,
            message: message.clone(),
        };
        crate::proxy::record_middleware_refusal(state, ctx, &e, ClientProto::Realtime).await;
    }
    Refused::new(status, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_handshake_shape_carries_held_by_on_the_409_only() {
        let mut r = Refused::new(
            StatusCode::CONFLICT,
            "chat_thread_bound",
            "voice is in use on device 'desktop': this bind asked not to take it over \
             (takeover=never)"
                .into(),
        );
        r.held_by = Some("device 'desktop'".into());
        assert_eq!(r.close_kind(), Some(ClosedKind::ThreadBound));
        let resp = r.clone().into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"error": {"message": r.message, "type": "invalid_request_error",
                "param": null, "code": "chat_thread_bound", "held_by": "device 'desktop'"}})
        );
        let plain = Refused::new(StatusCode::NOT_FOUND, "chat_folder_not_found", "x".into());
        assert_eq!(plain.close_kind(), Some(ClosedKind::OutOfReach));
        let body = axum::body::to_bytes(plain.into_response().into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["error"].get("held_by").is_none(), "{v}");
        let internal = Refused::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "x".into());
        assert_eq!(internal.kind, "server_error");
        assert_eq!(internal.close_kind(), None);
        assert_eq!(
            internal.error_object().code.as_deref(),
            Some("internal"),
            "an error event carries the code"
        );
    }
}
