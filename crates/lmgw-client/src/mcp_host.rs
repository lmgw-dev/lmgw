//! A device's MCP host link, `GET /mcp/host` (client-apps design §5): the
//! wire facts a client that hosts tools reads — open it with
//! [`crate::requests::mcp_host`].
//!
//! - **The refusals before the 101**: `403 host_not_granted` (no hosting
//!   grant, or no device key) and `403 cross_origin_refused` (an `Origin`
//!   header), read with [`crate::requests::read_refusal`]; the key's own
//!   refusals ([`crate::requests::key_refused`]) as on every route.
//! - **The closes** read as a realtime session's do ([`close_kind`]): 4000
//!   another link of the same device took over ([`CloseKind::TakenOver`]),
//!   4003 a revocation with its kind, 1001 a stopping gateway; 1000 the
//!   grant cleared or the row switched off, 1002 a frame the link does not
//!   take or a failed handshake, 1009 a frame over a size setting, 1011 no
//!   pong in time — each [`CloseKind::Other`], its reason the sentence.
//! - **`_meta` on every `tools/call`**: [`CallMeta::from_meta`] reads who
//!   the call runs as, the approval that decided it and when lmgw stops
//!   waiting. A missing caller is an unknown one; a kind this build does
//!   not know ([`CallerKind::Unknown`]) and `anonymous` are the least
//!   trusted callers there are.
//! - **Tasks** (MCP revision 2025-11-25): a `tools/call` that carries
//!   `task` also carries `_meta["lmgw/task"]` ([`TaskMeta`], read by
//!   [`CallMeta::from_meta`]): whether its result enters a thread later or a
//!   caller waits for it now.
//! - **The link's limits in `initialize`**: [`HostLimits::from_meta`] reads
//!   `params._meta["lmgw/host_limits"]` — the largest message and frame lmgw
//!   reads, its ping interval and the row's call timeout — so a device need
//!   not guess them.
//!
//! The link is an lmgw transport binding, not an MCP transport: the client
//! dials, and is the MCP **server** on the socket — one JSON-RPC message
//! per text frame, no batches.

pub use crate::realtime::{close_kind, CloseKind};
pub use lmgw_api_types::mcp_host::{
    taken_over_reason, Approval, CallMeta, CallerKind, HostLimits, Principal, TaskDelivery,
    TaskMeta, CLOSE_GOING_AWAY, CLOSE_NO_PONG, CLOSE_PROTOCOL_ERROR, CLOSE_REVOKED,
    CLOSE_TAKEN_OVER, CLOSE_TOO_BIG, CROSS_ORIGIN_REFUSED, HOST_NOT_GRANTED, META_APPROVAL,
    META_CALLER, META_HOST_LIMITS, META_TASK, META_TIMEOUT_MS, PATH,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requests;

    #[test]
    fn the_link_s_closes_read_as_a_session_s() {
        assert_eq!(
            close_kind(CLOSE_TAKEN_OVER, &taken_over_reason("desktop")),
            CloseKind::TakenOver
        );
        assert!(matches!(
            close_kind(CLOSE_REVOKED, "key_unknown: device 'desktop' was rotated"),
            CloseKind::Revoked { .. }
        ));
        assert_eq!(
            close_kind(CLOSE_TOO_BIG, "x"),
            CloseKind::Other {
                code: CLOSE_TOO_BIG
            }
        );
        let r = requests::mcp_host();
        assert_eq!(r.path, PATH);
        assert_eq!(
            r.ws_url("https://gw.example:8001/").unwrap(),
            "wss://gw.example:8001/mcp/host"
        );
        let e = requests::read_refusal(
            403,
            r#"{"code":"host_not_granted","message":"device 'desktop' has no hosting grant"}"#,
        );
        assert_eq!(e.code, HOST_NOT_GRANTED);
    }
}
