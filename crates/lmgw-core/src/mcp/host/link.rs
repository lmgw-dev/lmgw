//! The task that owns one host link's socket (§5.1, §5.3, §5.4).
//!
//! It runs the rmcp client session's handshake beside it (`initialize`, then
//! the tool list), passes frames between the socket and the session, pings,
//! and closes — and says why:
//!
//! | Close | Why |
//! |---|---|
//! | 4000 | another link of the same device took over |
//! | 4003 | the device key was revoked (its token, as a realtime session's) |
//! | 1001 | lmgw is stopping or restarting |
//! | 1009 | a frame or message over `mcp.host_max_frame_mb` / `host_max_message_mb` |
//! | 1011 | no pong within `mcp.host_ping_interval_s` |
//! | 1002 | a frame the link does not take (binary, a batch, no JSON-RPC), or a failed handshake |
//! | 1000 | the row was switched off or its grant cleared, or the session ended |
//!
//! **Before lmgw closes a link** it sends `notifications/cancelled` for every
//! call still open on it (R14), then the close; those calls report the
//! close's reason. A link the device drops has nobody left to tell: its open
//! calls end with the abandoned wording (`calls`).
//!
//! **Every write is bounded**: by `mcp.host_ping_interval_s` while pings are
//! on, by the row's `timeout_ms` when they are off (0). A device that stops
//! reading fails the write and the link ends, so it cannot hold a close or a
//! revocation open.

use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use rmcp::model::{JsonRpcMessage, RequestId};
use rmcp::service::{RunningService, RxJsonRpcMessage};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};

use lmgw_api_types::mcp_host as wire;

use crate::config::McpServer;
use crate::devices::LinkKind;
use crate::proxy::RequestCtx;
use crate::realtime::Limits;
use crate::state::SharedState;

use super::super::handler::GatewayClientHandler;
use super::calls::device_name;
use super::links::{CloseOrder, LinkDevice};
use super::transport::{self, OpenCalls};

/// What the handshake hands the link: the session and its tools.
type Handshake = Result<
    (
        RunningService<RoleClient, GatewayClientHandler>,
        Vec<rmcp::model::Tool>,
    ),
    String,
>;

/// What a link starts with, read before the 101.
pub(super) struct LinkInit {
    pub(super) state: SharedState,
    pub(super) ctx: RequestCtx,
    pub(super) server: McpServer,
    pub(super) limits: Limits,
    /// Held to the link's last line: a stopping server waits for it.
    pub(super) running: crate::server::Running,
}

/// Run one link until it closes, from either end.
pub(super) async fn run(socket: WebSocket, init: LinkInit) {
    let LinkInit {
        state,
        ctx,
        server,
        limits,
        running: _running,
    } = init;
    let device = device_name(&server).to_string();
    // One of the device's connections: online while it lives, `last_seen_at`
    // as it opens and closes (L15), and closed with 4003 on a revocation.
    let mut conn =
        crate::devices::connect(&state, &ctx.principal, ctx.revocation_mark, LinkKind::Tools).await;
    let ping_s = state.snapshot().settings.mcp.host_ping_interval_s;
    let (transport, mut side) = transport::pair();
    let link = state.mcp.link_opened(&server, side.open.clone()).await;
    let mut order = Some(state.mcp.host.open(
        server.id,
        link,
        LinkDevice {
            key_id: server.device_key_id.unwrap_or_default(),
            who: crate::devices::who(&ctx.principal),
        },
        (wire::CLOSE_TAKEN_OVER, wire::taken_over_reason(&device)),
    ));
    tracing::info!("mcp host link {link}: device '{device}' connected");

    let mut handler = state.mcp.handler_for(&server);
    // The link's real limits, so the device never guesses them (§5.1).
    handler.init_meta = Some(host_limits_meta(&limits, ping_s, server.timeout_ms));
    let bound = Duration::from_millis(server.timeout_ms);
    let mut handshake = Some(tokio::spawn(async move {
        let session = async {
            let running = handler
                .serve(transport)
                .await
                .map_err(|e| format!("initialize: {e}"))?;
            let tools = running
                .peer()
                .list_all_tools()
                .await
                .map_err(|e| format!("tools/list: {e}"))?;
            Ok((running, tools))
        };
        tokio::time::timeout(bound, session)
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "no answer to initialize and tools/list within {} ms (the server's timeout_ms)",
                    bound.as_millis()
                ))
            })
    }));
    let mut session: Option<RunningService<RoleClient, GatewayClientHandler>> = None;

    let (mut sink, mut stream) = socket.split();
    let stopped = state
        .stops
        .stopped_after(state.stops.at_or_now(ctx.served_at));
    tokio::pin!(stopped);
    let mut live = Live::new(ping_s);
    // A write that takes longer than a ping interval is a device that
    // stopped reading; with pings off, longer than the row's timeout_ms (the
    // longest lmgw waits on the device for anything). Every write is bounded,
    // so a device that stops reading cannot hold a close or a revocation.
    let write_bound = live
        .every
        .unwrap_or(Duration::from_millis(server.timeout_ms.max(1)));

    let close: Option<CloseOrder> = loop {
        // Biased, the revocation first: a key deleted also deletes its row,
        // and both arms can be ready at once — the revocation's 4003 is the
        // one to say (L18; `McpManager::close_unwanted_links` says the same).
        tokio::select! {
            biased;
            reason = crate::devices::revoked(conn.as_mut()) => {
                let c = conn.as_ref().expect("a revocation comes from a key's watch");
                break Some((
                    wire::CLOSE_REVOKED,
                    crate::devices::close_reason(reason, c.name(), c.is_device()),
                ));
            }
            got = async { order.as_mut().expect("guarded").await }, if order.is_some() => {
                order = None;
                // An order lost (the table forgot this link without one: it
                // was replaced or closed elsewhere) comes no more: go on until
                // something else ends it.
                if let Ok(o) = got {
                    break Some(o);
                }
            }
            () = &mut stopped => break Some((
                wire::CLOSE_GOING_AWAY,
                crate::server::STOPPING.to_string(),
            )),
            joined = async { handshake.as_mut().expect("guarded").await }, if handshake.is_some() => {
                handshake = None;
                let result: Handshake = joined.unwrap_or_else(|e| Err(format!("the handshake failed: {e}")));
                match result {
                    Ok((running, tools)) => {
                        let peer = running.peer().clone();
                        let n = tools.len();
                        session = Some(running);
                        if !state.mcp.link_ready(server.id, link, Ok((peer, tools))).await {
                            break Some(take_order(&mut order).unwrap_or_else(|| {
                                (1000, "this device's hosted tools went away on the gateway".into())
                            }));
                        }
                        tracing::info!("mcp host link {link}: device '{device}' listed {n} tools");
                    }
                    Err(e) => {
                        tracing::info!("mcp host link {link}: device '{device}': {e}");
                        state.mcp.link_ready(server.id, link, Err(e.clone())).await;
                        break Some((wire::CLOSE_PROTOCOL_ERROR, format!("the MCP handshake failed: {e}")));
                    }
                }
            }
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => match parse(text.as_str()) {
                    Ok(msg) => {
                        answered(&side.open, &msg);
                        // The session is gone only when the link ends.
                        let _ = side.incoming.send(msg);
                    }
                    Err(why) => break Some((wire::CLOSE_PROTOCOL_ERROR, why)),
                },
                Some(Ok(Message::Binary(_))) => break Some((
                    wire::CLOSE_PROTOCOL_ERROR,
                    "binary frames are not part of the host link: one JSON-RPC message per text \
                     frame".into(),
                )),
                Some(Ok(Message::Pong(_))) => live.pong(),
                // tungstenite answers pings itself.
                Some(Ok(Message::Ping(_))) => {}
                Some(Ok(Message::Close(_))) | None => break None,
                Some(Err(e)) => match limits.close_reason(e) {
                    Some(reason) => break Some((wire::CLOSE_TOO_BIG, reason)),
                    None => break None,
                },
            },
            out = side.outgoing.recv() => match out {
                Some(text) => {
                    if write(&mut sink, Message::Text(text.into()), write_bound).await.is_err() {
                        break None;
                    }
                }
                None => break Some((1000, "the MCP session ended".into())),
            },
            beat = live.beat() => match beat {
                Beat::Ping => {
                    if write(&mut sink, Message::Ping(Default::default()), write_bound).await.is_err() {
                        break None;
                    }
                }
                Beat::Dead(why) => break Some((wire::CLOSE_NO_PONG, why)),
            },
        }
    };

    if let Some((code, reason)) = &close {
        let reason = cut(reason);
        // Every call still open is cancelled before the close (R14), and
        // reports this reason (`calls`).
        for id in side.open.close_with(&reason) {
            let _ = write(&mut sink, cancelled(&id, &reason), write_bound).await;
        }
        let frame = CloseFrame {
            code: *code,
            reason: reason.clone().into(),
        };
        let _ = write(&mut sink, Message::Close(Some(frame)), write_bound).await;
        tracing::info!("mcp host link {link}: device '{device}': closed {code} — {reason}");
    } else {
        tracing::info!("mcp host link {link}: device '{device}' disconnected");
    }
    state.mcp.host.ended(server.id, link);
    // The session hears its transport end: a call still waiting on the
    // device ends abandoned (`calls`).
    drop(side);
    if let Some(h) = handshake {
        h.abort();
    }
    if let Some(s) = session {
        let _ = tokio::time::timeout(Duration::from_secs(5), s.cancel()).await;
    }
    state.mcp.link_closed(server.id, link).await;
    drop(conn);
}

/// `initialize`'s `_meta`: the limits this link enforces
/// ([`wire::HostLimits`]) — the WebSocket's sizes as set on the upgrade, the
/// ping interval it was opened with, the row's call timeout.
fn host_limits_meta(limits: &Limits, ping_s: u32, timeout_ms: u64) -> rmcp::model::Meta {
    let bytes = |n: usize| (n != usize::MAX).then(|| u64::try_from(n).unwrap_or(u64::MAX));
    let host = wire::HostLimits {
        max_message_bytes: bytes(limits.max_message),
        max_frame_bytes: bytes(limits.max_frame),
        ping_interval_s: Some(u64::from(ping_s)),
        call_timeout_ms: Some(timeout_ms),
    };
    let mut meta = serde_json::Map::new();
    meta.insert(
        wire::META_HOST_LIMITS.into(),
        serde_json::to_value(&host).unwrap_or(Value::Null),
    );
    rmcp::model::Meta(meta)
}

/// The order a takeover or a close already left for this link, if any.
fn take_order(
    order: &mut Option<tokio::sync::oneshot::Receiver<CloseOrder>>,
) -> Option<CloseOrder> {
    order.as_mut().and_then(|o| o.try_recv().ok())
}

/// One frame, as the session reads it: one JSON-RPC message, never a batch.
fn parse(text: &str) -> Result<RxJsonRpcMessage<RoleClient>, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| format!("a frame is not JSON ({e}): one JSON-RPC message per text frame"))?;
    if value.is_array() {
        return Err(
            "JSON-RPC batches are not taken on the host link: one message per frame".into(),
        );
    }
    serde_json::from_value(value).map_err(|e| format!("a frame is not a JSON-RPC message: {e}"))
}

/// A response or an error the device sent closes the call it answers.
fn answered(open: &OpenCalls, msg: &RxJsonRpcMessage<RoleClient>) {
    match msg {
        JsonRpcMessage::Response(r) => open.closed(&r.id),
        JsonRpcMessage::Error(e) => {
            if let Some(id) = &e.id {
                open.closed(id);
            }
        }
        _ => {}
    }
}

/// `notifications/cancelled` for `id`, as a frame.
fn cancelled(id: &RequestId, reason: &str) -> Message {
    let note = json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": id, "reason": reason},
    });
    Message::Text(note.to_string().into())
}

/// Send one frame, bounded by `within`.
async fn write(
    sink: &mut SplitSink<WebSocket, Message>,
    msg: Message,
    within: Duration,
) -> Result<(), ()> {
    match tokio::time::timeout(within, sink.send(msg)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

/// A close frame's reason fits 123 bytes (RFC 6455 §5.5), cut at a
/// character boundary.
fn cut(reason: &str) -> String {
    if reason.len() <= 123 {
        return reason.to_string();
    }
    let mut end = 120;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &reason[..end])
}

/// What a tick of the ping clock asks for.
enum Beat {
    Ping,
    Dead(String),
}

/// The link's pings (§5.1): one every `mcp.host_ping_interval_s`; a ping
/// with no pong by the next tick closes the link.
struct Live {
    clock: Option<tokio::time::Interval>,
    every: Option<Duration>,
    unanswered: bool,
}

impl Live {
    fn new(seconds: u32) -> Self {
        let every = (seconds > 0).then(|| Duration::from_secs(u64::from(seconds)));
        let clock = every.map(|d| {
            let mut c = tokio::time::interval_at(tokio::time::Instant::now() + d, d);
            c.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            c
        });
        Self {
            clock,
            every,
            unanswered: false,
        }
    }

    async fn beat(&mut self) -> Beat {
        let Some(clock) = &mut self.clock else {
            return std::future::pending().await;
        };
        clock.tick().await;
        if std::mem::replace(&mut self.unanswered, true) {
            Beat::Dead(format!(
                "no pong within {} s, setting mcp.host_ping_interval_s",
                self.every.map_or(0, |d| d.as_secs())
            ))
        } else {
            Beat::Ping
        }
    }

    fn pong(&mut self) {
        self.unanswered = false;
    }
}
