//! Service mode's reverse proxy (container-runtime design §3.3, origins §4.2).
//!
//! Two faces of one container, and deliberately not alike (origins §4.8). The
//! **UI face is an origin**: `http://<id>.<suffix>:<port>/`, handed here by
//! [`host_dispatch`] on the `Host` header before the main router sees the
//! request, mounted at `/`, with every method, path, query and upgrade
//! forwarded to the container's published loopback port byte for byte. The
//! **MCP face is a path**: `ANY /agents/{id}/mcp`, aimed at whatever
//! `run.provides.mcp` declares. Its consumer is lmgw's own MCP client,
//! server-side, where a browser origin would buy nothing.
//!
//! **What this proxy does not do is the point.** Nothing in a body is
//! rewritten: a proxy that rewrites HTML breaks on the first thing it did not
//! anticipate, and there is nothing to rewrite on the way in — the app is at
//! the root of its own origin and writes its own URLs. The status is forwarded
//! verbatim. Both bodies are **streamed**, never buffered, so an SSE endpoint
//! arrives event by event and an upload is not held in lmgw's heap. A
//! WebSocket is a byte pipe, not a frame decoder.
//!
//! **Header hygiene**, in both directions: hop-by-hop headers are stripped
//! ([`HOP_BY_HOP`]), and on the way *in* so is everything in [`NEVER_FORWARD`]
//! — the dashboard's credentials, the whole `X-Forwarded-*` family, and
//! `X-Lmgw-Face`. lmgw sets its own four of those (§4.6): the
//! `X-Forwarded-Host`/`-Proto` pair, `X-Forwarded-For` from the TCP peer, and
//! `X-Lmgw-Face` naming which face the request came in on — the two facts an
//! app needs to decide whom it serves, and that only lmgw knows.
//!
//! **The isolation is the browser's** (origins §2). The agent's page is a
//! different site from the dashboard: its JS cannot read `/api/op/*`, the
//! browser never attaches the dashboard's cookie to it, and a cross-origin
//! frame cannot reach the Tauri bridge the shell injects into its main frame.

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Json;
use futures::StreamExt;
use lmgw_api_types as dto;

use crate::agents::{service, Agent};
use crate::state::SharedState;
use crate::store;

/// Stripped in both directions (RFC 9110 §7.6.1): these describe *this* hop's
/// connection and mean nothing to the next one. `upgrade` and `connection` are
/// put back by hand on the WebSocket path, which is the one case where they are
/// the message.
pub const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
];

/// Never forwarded to a container (origins §4.6).
///
/// Two groups, for two reasons. `cookie` and `authorization` are the
/// dashboard's own credentials: the browser no longer attaches them here — the
/// agent origin is a different site — and they are dropped anyway as defence in
/// depth, because a container has the agent token in `secrets.json` and needs
/// nothing of lmgw's, whatever reaches this hop. The `X-Forwarded-*` family is
/// dropped because an app is now *told* to trust it: lmgw sets its own three
/// with `insert`, and a client that supplies its own must not end up first of
/// two. `x-lmgw-face` for the same reason: it tells the app which face a
/// request came in on, and a client naming its own would be choosing the
/// Admin-gated face's answers without passing its gate. `host` is reqwest's to
/// set from the URL it dials.
const NEVER_FORWARD: [&str; 9] = [
    "cookie",
    "authorization",
    "host",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-for",
    "x-forwarded-prefix",
    "forwarded",
    "x-lmgw-face",
];

/// The header naming the face a proxied request came in on ([`Face`]).
const FACE_HEADER: &str = "x-lmgw-face";

/// The WebSocket headers that *are* the message, carried through unchanged.
const WS_HEADERS: [&str; 5] = [
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
    "sec-websocket-accept",
];

pub fn routes(state: &SharedState) -> axum::Router<SharedState> {
    // The MCP face of the container (§4.8). The `agent:<id>` row in
    // `mcp_servers` points here and not at the host port, because the port is
    // ephemeral per start and the row is persistent. `Admin`, the owner's:
    // it is dialled by lmgw's own MCP client, which presents the
    // `owner:dashboard` bearer at dial time rather than storing one on the row.
    let mcp_face = axum::Router::new()
        .route("/agents/{id}/mcp", any(mcp))
        .route("/agents/{id}/mcp/{*rest}", any(mcp))
        .route_layer(crate::server::require(state, crate::principal::Cap::Admin));
    // Where the app used to be (§4.2). `Public` on purpose: the answer is an
    // address, not a secret, and a bookmark from before the origin should be
    // told where the app went rather than asked for a session the agent origin
    // does not want.
    let moved = axum::Router::new()
        .route("/agents/{id}/app", any(app_moved))
        .route("/agents/{id}/app/", any(app_moved))
        .route("/agents/{id}/app/{*rest}", any(app_moved))
        .route_layer(crate::server::require(state, crate::principal::Cap::Public));
    mcp_face
        .merge(moved)
        // A proxied upload is as big as the container is willing to read;
        // axum's silent 2 MiB default would answer it with an unexplained
        // plain-text 413 nobody chose.
        .layer(axum::extract::DefaultBodyLimit::disable())
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn err(status: StatusCode, code: &str, message: String) -> Response {
    (
        status,
        Json(dto::ApiError {
            code: code.into(),
            message,
        }),
    )
        .into_response()
}

/// Which face a proxied request came in on, and therefore who is reading the
/// answer (origins §4.2, §4.8).
///
/// The MCP face is an `Admin` route on the main origin: its caller is lmgw's
/// own client, presenting the owner's bearer. The UI face is the agent origin,
/// which asks for no credential at all — anyone who can reach the port is a
/// reader there. They cannot be told the same things.
///
/// The container is told which one it is answering, in `X-Lmgw-Face`: the
/// same `X-Forwarded-Host` arrives on both, so without it an app could not
/// tell lmgw's Admin-gated MCP call from anyone on the port sending the same
/// path with the agent's host name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Face {
    /// `http://<id>.<suffix>:<port>/` — unauthenticated. `X-Lmgw-Face: app`.
    Origin,
    /// `ANY /agents/{id}/mcp` — `Admin`. `X-Lmgw-Face: mcp`.
    Mcp,
}

impl Face {
    /// What `X-Lmgw-Face` says for this face.
    fn header_value(self) -> HeaderValue {
        HeaderValue::from_static(match self {
            Self::Origin => "app",
            Self::Mcp => "mcp",
        })
    }
}

/// A start that did not finish.
///
/// `503`, not `502`: nothing is broken, the thing simply is not up.
///
/// **What the body says depends on who is reading** (mounts §5.3, principals
/// §3.9). On an `Admin` path it is the whole reason plus the container's own
/// log tail, because "it did not start" with no further detail is the failure
/// mode this whole runtime exists to avoid. On the **agent origin** it is not:
/// the origin is public to whoever can reach the port, and a start refused by
/// the path rules fails with a sentence naming a folder on this machine — so
/// `Host: board.localhost` would have been a way to read a host path with no
/// credential at all. The public answer names the agent, which the reader
/// typed, and points at the one place the reason is: the owner's own App tab.
fn starting(id: &str, e: &service::StartError, face: Face) -> Response {
    if face == Face::Origin {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "code": "agent_service_starting",
                "message": format!(
                    "the app for '{id}' could not be started. Its owner can see why on this \
                     agent's App tab in the lmgw dashboard."
                ),
            })),
        )
            .into_response();
    }
    let mut body = serde_json::json!({
        "code": "agent_service_starting",
        "message": format!("the app for '{id}' could not be started: {}", e.reason),
    });
    if !e.log.trim().is_empty() {
        body["log"] = serde_json::Value::String(e.log.clone());
    }
    (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

// ---------------------------------------------------------------------------
// The `Host` dispatch (origins §4.2)
// ---------------------------------------------------------------------------

/// Second layer from the outside: inside the trace, **outside** CORS (§3.11).
///
/// A host under `agent_origin_suffix` is an agent's address or it is nothing.
/// If it names an agent that declares `run.service`, the request is that
/// container's — no principal is resolved and no capability is required,
/// because the agent origin is the container's own namespace, public to whoever
/// can reach the port, exactly as `/agents/<id>/app/` was. If it names anything
/// else, it gets a `404` and never the SPA: serving the dashboard on a name
/// that is not the dashboard's is how a label becomes a second front door.
/// Every other host falls through to the main router unchanged.
///
/// Being outside `CorsLayer` is the point of the position: a foreign page can
/// navigate a browser to `http://board.localhost:8001/` — every browser
/// resolves `*.localhost` to loopback — and with no `Access-Control-Allow-Origin`
/// on the answer it cannot *read* what comes back.
pub(crate) async fn host_dispatch(
    State(st): State<SharedState>,
    req: Request,
    next: Next,
) -> Response {
    let snap = st.snapshot();
    let Some(host) = request_host(&req) else {
        return next.run(req).await;
    };
    let Some(label) = service::origin_label(&snap.settings, &host) else {
        return next.run(req).await;
    };
    let agent = match origin_agent(&st, &host, &label).await {
        Ok(agent) => agent,
        Err(refusal) => return refusal,
    };
    // The path as the client sent it, percent-encoding and dot segments
    // included: on an origin there is no mount to leave, and a `..` is the
    // container's to interpret (§4.4).
    let path = req.uri().path().to_string();
    proxy_agent(st, agent, &path, req, Face::Origin).await
}

/// The host this request is addressed to, with any port taken off.
///
/// HTTP/1.1 sends an origin-form target and puts the authority in `Host`;
/// HTTP/2 puts it in `:authority`, which hyper renders as the URI's own. Both
/// are read, so the dispatch does not depend on which one the client speaks.
fn request_host(req: &Request) -> Option<String> {
    let authority = match req.uri().host() {
        Some(h) => h.to_string(),
        None => req.headers().get(header::HOST)?.to_str().ok()?.to_string(),
    };
    Some(strip_port(&authority).to_string())
}

/// `board.localhost:8001` → `board.localhost`. A bracketed IPv6 literal is
/// returned whole, brackets and all, which is what makes it fail every suffix
/// test in [`service::origin_label`] instead of being split on a colon of its
/// own.
fn strip_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority;
    }
    authority
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(authority)
}

/// The agent an origin names, or what that origin is answered with.
///
/// The row is read by id the way the MCP face reads one — [`store::get_agent`]
/// and [`Agent::from_row`] — because an agent origin *is* an id and nothing
/// else (§4.1).
async fn origin_agent(st: &SharedState, host: &str, label: &str) -> Result<Agent, Response> {
    let row = match store::get_agent(&st.db, label).await {
        Ok(Some(row)) => row,
        Ok(None) => return Err(not_an_origin(host, label)),
        Err(e) => {
            return Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "op_failed",
                e.to_string(),
            ))
        }
    };
    let agent =
        Agent::from_row(row).map_err(|e| err(StatusCode::BAD_REQUEST, "agent_unreadable", e))?;
    if service::service_of(&agent).is_none() {
        return Err(not_an_origin(host, label));
    }
    Ok(agent)
}

/// The `404` a host under the suffix gets when no app answers on it — JSON
/// naming the label, never the SPA (§4.2).
///
/// **One body for both reasons.** An agent origin needs no credential, so this
/// answer goes to anyone who can reach the port, and "no agent has that id"
/// and "that agent declares no run.service" would between them let a stranger
/// enumerate which agents are installed and which of them serve a UI. The
/// label is named because the reader typed it and the address is not the
/// secret; what the label *is* here is not said.
fn not_an_origin(host: &str, label: &str) -> Response {
    err(
        StatusCode::NOT_FOUND,
        "not_found",
        format!(
            "'{host}' is an agent origin and nothing answers on it. An agent's UI answers on \
             '<id>.<suffix>' for exactly as long as an agent with that id declares run.service, \
             and '{label}' does not — whether it is an agent here at all is not this answer's to \
             say."
        ),
    )
}

// ---------------------------------------------------------------------------
// Routes on the main origin
// ---------------------------------------------------------------------------

/// The MCP face (§4.8): `/agents/{id}/mcp` and everything under it.
async fn mcp(State(st): State<SharedState>, req: Request) -> Response {
    let raw = req.uri().path().to_string();
    let (id, rest) = match split_mcp(&raw) {
        Ok(split) => split,
        Err(refusal) => return refusal.response(&raw),
    };
    let agent = match load(&st, &id).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    // The path inside the container is the **manifest's**, not the URL's:
    // `/agents/<id>/mcp` maps to `provides.mcp`, and anything under it hangs
    // off that.
    let Some(declared) = service::provides_mcp(&agent) else {
        return err(
            StatusCode::NOT_FOUND,
            "agent_provides_no_mcp",
            format!("agent '{id}' declares no run.provides.mcp, so it serves no MCP endpoint here"),
        );
    };
    let path = match rest.as_str() {
        "/" => declared.to_string(),
        more => format!("{}{more}", declared.trim_end_matches('/')),
    };
    proxy_agent(st, agent, &path, req, Face::Mcp).await
}

/// The agent id and the part of the **raw** request path that belongs to the
/// container, with its percent-encoding untouched.
///
/// Read off the raw path rather than through axum's `Path` extractor, which
/// hands `{*rest}` over **percent-decoded**. Re-assembling a URL from a decoded
/// path is four bugs in one (all verified against this proxy):
/// `a%3Fevil=1?real=1` injects a query and drops the real one, `a%23frag?real=1`
/// drops the query at a `#` that was never a fragment, `%2e%2e%2f` becomes a
/// walk up the path, and `a%2Fb` loses the encoded slash. The raw
/// [`Uri::path`](axum::http::Uri::path) is what the client actually sent, so it
/// is what gets forwarded — byte for byte, with the query attached through
/// [`reqwest::Url::set_query`] rather than by concatenation.
///
/// **A `.` or `..` segment is refused here**, with the `bad_path` code, and
/// only here. The UI face is a whole origin now and a dot segment in one of its
/// paths is the container's to interpret (§4.4) — but this face still has a
/// real prefix in front of it, and the request goes out through `reqwest`,
/// which resolves dot segments in the URL it is handed. So
/// `/agents/x/mcp/../admin` would arrive at the container as `/admin`: a path
/// outside the one the manifest declared, reached by a segment lmgw never
/// looked at. lmgw forwards a proxied path verbatim and does not resolve dot
/// segments, so it refuses one rather than guessing what leaving the prefix was
/// supposed to mean.
fn split_mcp(raw: &str) -> Result<(String, String), McpPathRefusal> {
    let parsed = (|| {
        let after = raw.strip_prefix("/agents/")?;
        let (id, tail) = after.split_once('/')?;
        // An agent id is `[a-z0-9-]` (`manifest::validate_id`), so a
        // percent-encoded one cannot name a real agent and is left to fail the
        // lookup as "no agent with that id" rather than being decoded here.
        let rest = tail.strip_prefix("mcp")?;
        (rest.is_empty() || rest.starts_with('/')).then_some((id, rest))
    })();
    let Some((id, rest)) = parsed else {
        return Err(McpPathRefusal::NotAnMcpPath);
    };
    if rest.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(McpPathRefusal::DotSegment);
    }
    Ok((
        id.to_string(),
        if rest.is_empty() {
            "/".to_string()
        } else {
            rest.to_string()
        },
    ))
}

/// What [`split_mcp`] refused, as the two words rather than as a whole
/// `Response`: an answer is 128 bytes of `Err` on every call that succeeds,
/// and this is the path every one of lmgw's own MCP calls to an agent takes.
enum McpPathRefusal {
    NotAnMcpPath,
    DotSegment,
}

impl McpPathRefusal {
    fn response(self, raw: &str) -> Response {
        match self {
            Self::NotAnMcpPath => err(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("'{raw}' is not an agent's MCP path"),
            ),
            Self::DotSegment => err(
                StatusCode::BAD_REQUEST,
                "bad_path",
                format!(
                    "'{raw}' carries a '.' or '..' path segment. The MCP face is a path under \
                     lmgw's own origin, and lmgw forwards a proxied path verbatim rather than \
                     resolving dot segments out of it, so it refuses one rather than guessing \
                     what leaving '/agents/<id>/mcp' was supposed to mean."
                ),
            ),
        }
    }
}

/// `/agents/{id}/app*` — the mount that became an origin (§4.2).
///
/// The refusal carries the address as well as the reason, so an old bookmark
/// says where to go instead of `session_required`.
///
/// **The store is not consulted at all here, and that is the point.** This
/// route is `Public` — an address is not a secret — so a differential answer
/// would be an unauthenticated oracle for which ids exist and which of them
/// serve a UI. `http://<id>.<suffix>:<port>/` is derivable from the id, the
/// settings and nothing else, so every id gets the same sentence with its own
/// name in it, and an id nobody installed is told where its app *would* be.
async fn app_moved(
    State(st): State<SharedState>,
    Path(captures): Path<HashMap<String, String>>,
) -> Response {
    let id = captures.get("id").cloned().unwrap_or_default();
    let origin = service::agent_origin(&st.snapshot().settings, &id);
    let body = serde_json::json!({
        "code": "agent_app_moved",
        "origin": origin,
        "message": format!(
            "/agents/{id}/app is gone: an agent's UI is served on an origin of its own now, at \
             {origin} for '{id}'. A page under the dashboard's own host name was same-origin \
             with the dashboard, and only a different host name keeps two pages apart in a \
             browser."
        ),
    });
    (StatusCode::NOT_FOUND, Json(body)).into_response()
}

async fn load(st: &SharedState, id: &str) -> Result<Agent, Response> {
    let row = match store::get_agent(&st.db, id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Err(err(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no agent with id '{id}'"),
            ))
        }
        Err(e) => {
            return Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "op_failed",
                e.to_string(),
            ))
        }
    };
    Agent::from_row(row).map_err(|e| err(StatusCode::BAD_REQUEST, "agent_unreadable", e))
}

// ---------------------------------------------------------------------------
// The proxy itself
// ---------------------------------------------------------------------------

async fn proxy_agent(
    st: SharedState,
    agent: Agent,
    path: &str,
    mut req: Request,
    face: Face,
) -> Response {
    let id = agent.row.id.clone();
    // The server this request came in on: an app's streamed answer ends at
    // its stop (review F-4), as every other long-lived stream does.
    let served_at = req
        .extensions()
        .get::<crate::server::ServedAt>()
        .map(|crate::server::ServedAt(at)| *at);
    if service::service_of(&agent).is_none() {
        return err(
            StatusCode::NOT_FOUND,
            "agent_service_not_declared",
            format!(
                "agent '{id}' declares no run.service, so it has no app to serve. Add a \
                 `service` block to its manifest to give it one."
            ),
        );
    }
    // The first request pays for the start; N concurrent first requests share
    // one (§3.3). With a `dev_url` on the row there is nothing to start: the
    // request goes to the server the owner is already running (§4.7).
    let target = match service::target(&st, &agent).await {
        Ok(t) => t,
        Err(e) => return starting(&id, &e, face),
    };
    // Claimed before the request goes out and released only when the response
    // body is finished, so the idle sweep can never tear a stream down
    // mid-flight. `None` for a dev server: lmgw did not start it, does not
    // idle-stop it, and has no counter it would be honest to hold.
    let guard = match &target {
        service::Target::Container(live) => Some(live.guard()),
        service::Target::Dev(_) => None,
    };
    let upstream_base = target.base();

    // The path goes on as the bytes the client sent and the query is attached
    // as a query, never concatenated: see [`split_mcp`].
    let mut url = match reqwest::Url::parse(&format!("{upstream_base}{path}")) {
        Ok(u) => u,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                "bad_path",
                format!("'{path}' is not a path this proxy can forward: {e}"),
            )
        }
    };
    url.set_query(req.uri().query());
    let url = url.to_string();
    // Where the browser reaches this container: the one address a `Location`
    // naming the published loopback port is rewritten to, and the one lmgw puts
    // in `X-Forwarded-Host` (§4.5, §4.6). Computed per request, because a
    // suffix change renames every origin at once.
    let snap = st.snapshot();
    let origin = service::agent_origin_base(&snap.settings, &id);
    let authority = service::origin_authority(&snap.settings, &id);
    let method = req.method().clone();
    let upgrading = wants_upgrade(req.headers());
    let mut headers = forwarded_request_headers(req.headers(), upgrading);
    // The standard pair (§4.6), from which a server-side framework derives its
    // public URL. `insert`, never `append`: the client's own were dropped by
    // [`NEVER_FORWARD`], and exactly one of each goes on.
    if let Ok(v) = HeaderValue::from_str(&authority) {
        headers.insert(HeaderName::from_static("x-forwarded-host"), v);
    }
    headers.insert(
        HeaderName::from_static("x-forwarded-proto"),
        HeaderValue::from_static("http"),
    );
    // The two facts an app needs to decide whom it serves (§4.6), and that
    // only lmgw knows — both `insert`, both dropped from the client's own
    // headers by [`NEVER_FORWARD`]. Set here, before the upgrade branch, so a
    // WebSocket carries them too.
    headers.insert(HeaderName::from_static(FACE_HEADER), face.header_value());
    // The TCP peer of the connection that reached lmgw. On the agent origin
    // that is the reader: loopback for a browser on this box, a LAN address
    // for another machine — and for another container on this box too, which
    // reaches the gateway through host.containers.internal. On the MCP face it
    // is lmgw's own MCP client dialling itself (loopback, or this host's own
    // address when the gateway is bound to one): that face's trust is the
    // `Admin` gate in front of it, not this address.
    if let Some(peer) = peer_ip(&req) {
        if let Ok(v) = HeaderValue::from_str(&peer.to_string()) {
            headers.insert(HeaderName::from_static("x-forwarded-for"), v);
        }
    }

    if upgrading {
        let inbound = req.extensions_mut().remove::<hyper::upgrade::OnUpgrade>();
        return upgrade(
            st,
            &id,
            &url,
            method,
            headers,
            inbound,
            guard,
            (&origin, &upstream_base),
            served_at,
        )
        .await;
    }

    let body = reqwest::Body::wrap_stream(req.into_body().into_data_stream());
    // The **proxy's** client: `redirect::Policy::none()`, so a 3xx the app
    // returns is forwarded as a 3xx instead of being followed — possibly
    // off-origin — and answered as if the app had sent it.
    let sent = st
        .proxy_http
        .request(method, &url)
        .headers(headers)
        .body(body)
        .send()
        .await;
    let upstream = match sent {
        Ok(r) => r,
        Err(e) => {
            return match &target {
                // The container was `Ready` a moment ago and is not answering
                // now: it died after its health probe passed. Leaving the entry
                // in place would 502 every later request for as long as the
                // entry lives — with `idle_seconds = 0`, for good. Drop it here
                // so the *next* request starts a fresh one, and say so (§3.3).
                service::Target::Container(live) => {
                    let container = live.container.clone();
                    let evicted = service::stop(&st, &id, "the container stopped answering").await;
                    drop(guard);
                    unreachable(&id, &container, e, evicted.is_some())
                }
                // A dev server lmgw never started is a dev server lmgw cannot
                // restart: there is nothing to evict and nothing to collect,
                // so the 502 says who did not answer and leaves it alone.
                service::Target::Dev(url) => err(
                    StatusCode::BAD_GATEWAY,
                    "agent_dev_url_unreachable",
                    format!(
                        "the dev server at {url} did not answer: {e}. '{id}' has a dev_url set, \
                         so its app is served from there and no container was started — start \
                         the dev server, or clear the dev_url to go back to the image."
                    ),
                ),
            };
        }
    };

    let status = upstream.status();
    let mut out = Response::builder().status(status);
    if let Some(h) = out.headers_mut() {
        *h = forwarded_response_headers(upstream.headers(), false, &origin, &upstream_base);
    }
    // Chunk by chunk, flushed as it arrives: an SSE stream must reach the
    // browser event by event, and the guard rides along so the container is not
    // idle-stopped while it is still talking.
    let stream = upstream.bytes_stream().map(move |chunk| {
        if let Some(g) = &guard {
            g.touch();
        }
        chunk
    });
    // Until the server stops, then the end: an app's event stream open in a
    // frame or a tab does not hold the drain (review F-4).
    let stream = crate::server::until_stopped(&st.stops, served_at, stream, None);
    out.body(Body::from_stream(stream)).unwrap_or_else(|e| {
        err(
            StatusCode::BAD_GATEWAY,
            "agent_service_unreachable",
            e.to_string(),
        )
    })
}

fn unreachable(id: &str, container: &str, e: reqwest::Error, evicted: bool) -> Response {
    let tail = if evicted {
        " It has been collected, so the next request to this agent's app starts a fresh one."
    } else {
        " Nothing was left to collect."
    };
    err(
        StatusCode::BAD_GATEWAY,
        "agent_service_unreachable",
        format!(
            "the container '{container}' serving '{id}' did not answer: {e}. It was started and \
             passed its health probe, so it has stopped answering since.{tail}"
        ),
    )
}

/// The IP address of the TCP peer this request arrived from.
///
/// Read from the `ConnectInfo` [`server::run`](crate::server::run) attaches
/// to every connection (`into_make_service_with_connect_info`). A router
/// served without it has no address to tell, and then none is told: an app
/// that checks `X-Forwarded-For` reads a missing one as "not through lmgw" and
/// refuses, which is the right way for that mistake to fail.
///
/// An IPv4 client of a dual-stack `[::]` listener arrives as `::ffff:a.b.c.d`;
/// it is reported as the IPv4 address it is, so `127.0.0.1` reads as loopback
/// to an app whatever the bind address.
fn peer_ip(req: &Request) -> Option<std::net::IpAddr> {
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().to_canonical())
}

/// A `Connection: Upgrade` + `Upgrade: websocket` request (case-insensitive,
/// and `Connection` may list more than one token).
fn wants_upgrade(h: &HeaderMap) -> bool {
    let upgrade_to_ws = h
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let connection_says_upgrade = h
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
        });
    upgrade_to_ws && connection_says_upgrade
}

/// Everything the container should see of the browser's request.
fn forwarded_request_headers(h: &HeaderMap, upgrading: bool) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in h {
        let name = k.as_str().to_ascii_lowercase();
        if NEVER_FORWARD.contains(&name.as_str()) {
            continue;
        }
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        out.append(k.clone(), v.clone());
    }
    if upgrading {
        // Put the two back: on this path they are not connection bookkeeping,
        // they are the request.
        out.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        out.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        for name in WS_HEADERS {
            if let Some(v) = h.get(name) {
                out.insert(HeaderName::from_static(name), v.clone());
            }
        }
    }
    out
}

/// Everything the browser should see of the container's response.
///
/// Two headers get more than a copy:
///
/// - **`location`** is rewritten in exactly one case (§4.5): an absolute URL
///   naming the container's own published loopback origin — or the `dev_url`'s
///   — which the browser cannot reach and must never be told about. It becomes
///   the same path on the agent origin. Everything else passes untouched:
///   relative, origin-relative (`/login` is right as it is, now that the app is
///   mounted at `/`), and absolute somewhere else entirely, which is the app
///   entitled to send its user away.
/// - **`set-cookie`** goes through as-is, and lands on the **agent's** origin
///   rather than the dashboard's — which is the whole point of giving the app a
///   host name of its own (origins §2). A proxy that stripped them would break
///   every session the app keeps, and one that scoped them would be guessing.
fn forwarded_response_headers(
    h: &HeaderMap,
    upgrading: bool,
    origin: &str,
    container_base: &str,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in h {
        let name = k.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        // `content-length` describes a body this hop is re-framing as a
        // stream; letting it through would contradict the chunked encoding
        // hyper writes.
        if name == "content-length" {
            continue;
        }
        if name == "location" {
            if let Some(rewritten) = rewrite_location(v, origin, container_base) {
                out.append(k.clone(), rewritten);
                continue;
            }
        }
        out.append(k.clone(), v.clone());
    }
    if upgrading {
        out.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        out.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        for name in WS_HEADERS {
            if let Some(v) = h.get(name) {
                out.insert(HeaderName::from_static(name), v.clone());
            }
        }
    }
    out
}

/// The container's own origin swapped for the agent origin, or `None` to pass
/// the original `Location` through.
///
/// The remainder has to be empty or start a path of its own: `http://127.0.0.1:81`
/// is not a prefix of `http://127.0.0.1:8123/x` in any sense worth acting on.
fn rewrite_location(v: &HeaderValue, origin: &str, container_base: &str) -> Option<HeaderValue> {
    let raw = v.to_str().ok()?;
    let rest = match raw.strip_prefix(container_base)? {
        "" => "/",
        rest if rest.starts_with('/') => rest,
        _ => return None,
    };
    HeaderValue::from_str(&format!("{origin}{rest}")).ok()
}

/// The WebSocket tunnel: a byte pipe, not a frame decoder (§3.3).
///
/// `hyper::upgrade::on` on the inbound (axum) half, `reqwest::Response::upgrade`
/// on the outbound one, `tokio::io::copy_bidirectional` between them. Nothing
/// here parses a frame, which is exactly right for a proxy: masking,
/// fragmentation, ping/pong, close and any negotiated extension are between the
/// two endpoints, and a proxy that decoded them would be a second WebSocket
/// implementation to keep correct.
#[allow(clippy::too_many_arguments)]
async fn upgrade(
    st: SharedState,
    id: &str,
    url: &str,
    method: axum::http::Method,
    headers: HeaderMap,
    inbound: Option<hyper::upgrade::OnUpgrade>,
    // `None` when the upstream is a `dev_url`: there is no container to keep
    // awake and no counter to hold (§4.7).
    guard: Option<service::InFlight>,
    (origin, container_base): (&str, &str),
    served_at: Option<u64>,
) -> Response {
    let Some(inbound) = inbound else {
        return err(
            StatusCode::BAD_REQUEST,
            "upgrade_unavailable",
            "this request asked to upgrade but the server connection cannot be upgraded"
                .to_string(),
        );
    };
    let upstream = st
        .proxy_http
        .request(method, url)
        .headers(headers)
        // An upgrade is HTTP/1.1 by definition; asking for anything else here
        // would get a 400 from a server that does the right thing.
        .version(axum::http::Version::HTTP_11)
        .send()
        .await;
    let upstream = match upstream {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                "agent_service_unreachable",
                format!("the upstream serving '{id}' refused the WebSocket upgrade: {e}"),
            )
        }
    };
    if upstream.status() != StatusCode::SWITCHING_PROTOCOLS {
        // Not an upgrade after all — hand the answer back verbatim rather than
        // inventing one. The container is entitled to refuse.
        let status = upstream.status();
        let mut out = Response::builder().status(status);
        if let Some(h) = out.headers_mut() {
            *h = forwarded_response_headers(upstream.headers(), false, origin, container_base);
        }
        let stream = upstream.bytes_stream().map(move |chunk| {
            if let Some(g) = &guard {
                g.touch();
            }
            chunk
        });
        let stream = crate::server::until_stopped(&st.stops, served_at, stream, None);
        return out.body(Body::from_stream(stream)).unwrap_or_else(|e| {
            err(
                StatusCode::BAD_GATEWAY,
                "agent_service_unreachable",
                e.to_string(),
            )
        });
    }

    let mut out = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    if let Some(h) = out.headers_mut() {
        *h = forwarded_response_headers(upstream.headers(), true, origin, container_base);
    }
    let agent_id = id.to_string();
    // The tunnel ends at its server's stop too: both sockets are dropped, as
    // when either end goes. A byte pipe has no frame of its own to say why.
    let stopped = st.stops.stopped_after(st.stops.at_or_now(served_at));
    // Both halves are awaited in the task, not here: hyper hands over the
    // inbound socket only once this 101 has actually been written, so awaiting
    // it before returning would deadlock.
    tokio::spawn(async move {
        // The guard lives as long as the tunnel: a WebSocket that is open but
        // quiet is in flight, not idle.
        let _guard = guard;
        let outbound = match upstream.upgrade().await {
            Ok(io) => io,
            Err(e) => {
                tracing::warn!(agent = %agent_id, "the container's half of the WebSocket never upgraded: {e}");
                return;
            }
        };
        let inbound = match inbound.await {
            Ok(io) => io,
            Err(e) => {
                tracing::warn!(agent = %agent_id, "the browser's half of the WebSocket never upgraded: {e}");
                return;
            }
        };
        // Two copies raced rather than `copy_bidirectional`, deliberately.
        // `copy_bidirectional` returns only when **both** halves have shut
        // down, and a half-closed WebSocket is not a thing: a TCP FIN from
        // either endpoint means that endpoint is gone. A container that does
        // not answer the FIN it was sent would otherwise hold this task — and
        // with it the in-flight guard, and with that the container itself —
        // open for good, which is exactly the leak the idle stop exists to
        // prevent. (Measured: node's `server.on("upgrade")` socket does not
        // close on a half shutdown.)
        let (mut browser_r, mut browser_w) =
            tokio::io::split(hyper_util::rt::TokioIo::new(inbound));
        let (mut container_r, mut container_w) = tokio::io::split(outbound);
        let to_container = tokio::io::copy(&mut browser_r, &mut container_w);
        let to_browser = tokio::io::copy(&mut container_r, &mut browser_w);
        let ended = tokio::select! {
            r = to_container => r.map(|n| format!("the browser sent {n} bytes and then EOF")),
            r = to_browser => r.map(|n| format!("the container sent {n} bytes and then EOF")),
            () = stopped => Ok("the gateway stopped".to_string()),
        };
        match ended {
            Ok(why) => {
                tracing::debug!(agent = %agent_id, "the WebSocket tunnel closed: {why}")
            }
            Err(e) => tracing::debug!(agent = %agent_id, "the WebSocket tunnel ended: {e}"),
        }
    });
    out.body(Body::empty()).unwrap_or_else(|e| {
        err(
            StatusCode::BAD_GATEWAY,
            "agent_service_unreachable",
            e.to_string(),
        )
    })
}
