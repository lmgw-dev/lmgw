//! Anthropic's `request-id` on `/v1/messages` (gateway design §6.2).
//!
//! Anthropic answers every request with a `request-id` response header
//! (`req_…`), and its error body carries the same id as `request_id`. A
//! Messages answer an Anthropic-protocol upstream gave relays that id: the
//! header on the answer, whole or streamed (the upstream's own header, else
//! the error body's id when that is a valid header value), and on an error
//! that is the upstream's own answer the body's `request_id` too
//! ([`crate::error::GatewayError::to_anthropic_json_with`]). It rides to
//! the response in the [`crate::gate::GateHeaders`], which every Messages
//! answer is stamped with.
//!
//! Not yet: lmgw makes no request id of its own, so an answer from any other
//! upstream and an error lmgw raises itself carry none.

use axum::http::HeaderMap;
use serde_json::Value;

use crate::config::Protocol;
use crate::gate::UpstreamIds;
use crate::ingress::ClientProto;

/// The ids an answer of an upstream of `protocol` carries, for a client on
/// `proto`: only a `/v1/messages` answer an Anthropic-protocol upstream gave.
/// The `request-id` header is the header's id; `error_body` is an error
/// answer's body, whose `request_id` is the body's id. The response relays
/// the header's id when there is one, else the body's
/// ([`crate::gate::GateHeaders::stamp`]); the error body carries the body's
/// id, else the header's.
pub(crate) fn of_answer(
    proto: ClientProto,
    protocol: Protocol,
    headers: &HeaderMap,
    error_body: Option<&[u8]>,
) -> UpstreamIds {
    if proto != ClientProto::AnthropicMessages || protocol != Protocol::Anthropic {
        return UpstreamIds::default();
    }
    let some = |id: &str| (!id.is_empty()).then(|| id.to_string());
    UpstreamIds {
        header: headers
            .get(crate::proxy::REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(some),
        body: error_body
            .and_then(|b| serde_json::from_slice::<Value>(b).ok())
            .and_then(|v| some(v.get("request_id")?.as_str()?)),
    }
}

#[cfg(test)]
mod tests;
