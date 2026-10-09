//! `GET /mcp/host`: a paired device's MCP host link (client-apps design
//! §5.1–§5.5, L11, L12).
//!
//! **An lmgw transport binding, not an MCP transport.** The device opens a
//! WebSocket to this route with its device key, and the roles reverse at the
//! message level: lmgw is the MCP **client** on it, the device the server.
//! One JSON-RPC message per text frame, no batches. lmgw sends `initialize`
//! with the newest protocol version it speaks, the device answers with its
//! capabilities, lmgw sends `notifications/initialized` and lists the tools,
//! paging as for any server.
//!
//! **One mechanism.** The WebSocket is wrapped as an rmcp transport
//! ([`transport`]), so the session is the same `RunningService` with the same
//! [`GatewayClientHandler`](super::handler::GatewayClientHandler) every other
//! server gets, and the aggregate, the per-tool switches, hide and rename and
//! the call log are the manager's as for any row. What differs is who dials:
//! the manager's `Device` arm never connects a device row, it waits for this
//! route ([`conn`]).
//!
//! Children:
//! - [`transport`]: the rmcp transport over the link's two channels, and the
//!   calls open on it (cancelled before lmgw closes the link);
//! - [`link`]: the task that owns the socket — frames in and out, the size
//!   limits, the pings, the close and why;
//! - [`links`]: the open links by server, and the takeover;
//! - [`conn`]: the manager's side — the row's status, the peer the calls go
//!   to, the list;
//! - [`calls`]: one forwarded call — `_meta`, the timeout, the cancel;
//! - [`caller`]: who a call runs as, for `_meta`.
//!
//! The refusals come **before the 101**, as plain HTTP errors (`{code,
//! message}`, as the Chat routes answer): an `Origin` header is
//! `403 cross_origin_refused` (devices are native clients, §5.6), a principal
//! that is no device with a hosting grant `403 host_not_granted`.

mod caller;
mod calls;
mod conn;
mod link;
mod links;
mod transport;

pub use caller::CallFrom;
pub(crate) use conn::{offline_detail, DeviceLink};
pub(crate) use links::HostLinks;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};

use lmgw_api_types::mcp_host as wire;

use crate::config::{ApiKeyKind, McpServer};
use crate::principal::{Cap, Principal};
use crate::proxy::RequestCtx;
use crate::state::SharedState;

/// The route, behind the `Chat` capability (§5.1): an owner or a device gets
/// to the handler, which then wants a device with a hosting grant.
pub fn routes(state: &SharedState) -> Router<SharedState> {
    // The literal, not `wire::PATH`: the route walk reads the path here.
    Router::new().route(
        "/mcp/host",
        get(upgrade).route_layer(crate::server::require(state, Cap::Chat)),
    )
}

/// A refusal before the 101, in the Chat routes' flat shape.
fn refuse(status: StatusCode, code: &str, message: String) -> Response {
    (
        status,
        Json(lmgw_api_types::ApiError {
            code: code.to_string(),
            message,
        }),
    )
        .into_response()
}

/// `GET /mcp/host` — the handshake.
async fn upgrade(
    State(state): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    // First, and for every principal: a page in a browser never opens a
    // host link, whatever credential it was handed (§5.6).
    if let Some(origin) = headers.get(header::ORIGIN) {
        return refuse(
            StatusCode::FORBIDDEN,
            wire::CROSS_ORIGIN_REFUSED,
            format!(
                "{} is for a paired device's native client, and this upgrade carries an Origin \
                 header ('{}'): a browser page never hosts tools here",
                wire::PATH,
                origin.to_str().unwrap_or("?")
            ),
        );
    }
    let server = match hosted_row(&state, &ctx.principal) {
        Ok(s) => s,
        Err(why) => return refuse(StatusCode::FORBIDDEN, wire::HOST_NOT_GRANTED, why),
    };
    let ws = match ws {
        Ok(ws) => ws,
        Err(why) => {
            let mut resp = refuse(
                StatusCode::UPGRADE_REQUIRED,
                "upgrade_required",
                format!(
                    "{} is a WebSocket route (a device's MCP host link) and this request is not \
                     a WebSocket upgrade: {}",
                    wire::PATH,
                    why.body_text()
                ),
            );
            resp.headers_mut().insert(
                header::UPGRADE,
                header::HeaderValue::from_static("websocket"),
            );
            return resp;
        }
    };
    let snap = state.snapshot();
    let limits = crate::realtime::Limits::of_host_link(&snap.settings.mcp);
    let init = link::LinkInit {
        // Before the 101, so a stop that comes during the upgrade counts it
        // (the realtime handshake's review F-3).
        running: state.stops.running_at(state.stops.at_or_now(ctx.served_at)),
        state,
        ctx,
        server,
        limits,
    };
    ws.max_message_size(limits.max_message)
        .max_frame_size(limits.max_frame)
        .on_failed_upgrade(|e| tracing::info!("mcp host link: the WebSocket upgrade failed: {e}"))
        .on_upgrade(move |socket| link::run(socket, init))
}

/// The device row `principal` hosts, or why there is none for it (§1.5,
/// §5.1): it must be a device key with a hosting grant, and the grant's row
/// must be enabled.
fn hosted_row(state: &SharedState, principal: &Principal) -> Result<McpServer, String> {
    let Principal::Key {
        id,
        kind: ApiKeyKind::Device,
        name,
        ..
    } = principal
    else {
        return Err(format!(
            "{} is a paired device's MCP host link: it needs a device key with a hosting grant \
             (Usage → Keys), and this request carries {}",
            wire::PATH,
            crate::devices::who(principal)
        ));
    };
    let snap = state.snapshot();
    let device = crate::devices::short_name(name);
    let granted = snap
        .api_keys
        .iter()
        .find(|k| k.id == *id)
        .and_then(|k| k.hosts_label.clone());
    if granted.is_none() {
        return Err(format!(
            "device '{device}' has no hosting grant: the owner grants it a label to host tools \
             under on Usage → Keys"
        ));
    }
    let row = snap
        .mcp_servers
        .values()
        .find(|s| s.device_key_id == Some(*id))
        .cloned()
        .ok_or_else(|| {
            // A row of its name that is not its own keeps it from having one
            // (migration 0072's backfill skipped it): say which, since no
            // wait fixes that.
            match snap.mcp_servers.values().find(|s| s.name == *name) {
                Some(taken) => crate::store::blocked_grant(name, taken.id),
                None => format!(
                    "device '{device}' has a hosting grant but no MCP server row for it yet; \
                     try again in a moment"
                ),
            }
        })?;
    if !row.enabled {
        return Err(format!(
            "device '{device}''s hosted tools are switched off on this gateway (MCP page, \
             '{}')",
            row.name
        ));
    }
    Ok(row)
}
