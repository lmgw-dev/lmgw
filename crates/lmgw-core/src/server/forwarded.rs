//! Proxy mode (headless server design §4.5, ruling 8's second step): a
//! request whose TCP peer is inside `trusted_proxies` has its forwarded
//! headers read for the scheme, the host and the client address; from any
//! other peer they are ignored. One layer at the edge of the main router
//! ([`edge`]) reads them and writes what it read where everything after it
//! looks — the connection's `peer` becomes the client, its scheme the
//! forwarded one, the `Host` header the forwarded host — and then strips
//! every forwarded header from every request, trusted or not, so nothing
//! past it can read a client's own.
//!
//! **Which header.** `X-Forwarded-For`, `-Proto` and `-Host`, or RFC 7239's
//! `Forwarded`: one family per request. A request from a trusted peer that
//! carries both is refused, since a proxy that writes one family usually
//! passes a client's own of the other through untouched, and either
//! preference would let a client choose its own address behind one kind of
//! proxy.
//!
//! **The client** is the right-most hop that is not itself a trusted proxy:
//! each proxy appends the peer it saw, so only the hops to the left are the
//! client's to write. The chain is `X-Forwarded-For`'s hops, or the `for` of
//! each `Forwarded` element; when every hop is trusted, the left-most one. A
//! hop the walk reaches that names no address (`unknown`, an obfuscated
//! identifier, text that is no address) ends it with the client unknown,
//! [`UNKNOWN_CLIENT`], never the proxy's address in its place; so does a
//! chain with no hops at all beside the other forwarded headers (only
//! `X-Forwarded-Proto`, or `Forwarded: ,,`): the request crossed the proxy,
//! and its client is not the proxy. A trusted peer that sends no forwarded
//! header at all talks to lmgw directly and stays itself.
//!
//! **The scheme and the host** are `X-Forwarded-Proto` and
//! `X-Forwarded-Host`, each one value (several that differ are refused), or
//! the `proto` and `host` of the client's own `Forwarded` element (the one
//! the walk stopped at, written by the proxy that received the client's
//! request). Absent, the listener's scheme and the `Host` the proxy sent
//! stand.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::config::{trusted_nets, TrustedNet};
use crate::net::Scheme;
use crate::principal::Refusal;
use crate::state::SharedState;

use super::Conn;

/// The header text: the hops, the single values, `Forwarded`'s elements.
mod headers;
/// The first peers and the last refusal since the start or the last save.
mod seen;
pub use seen::{FirstPeer, Line, ProxySeen, Refused, Since};

use headers::Hop;

/// The client address of a forwarded request whose chain names none the
/// walk could read: the unspecified address, port 0 — not loopback, inside
/// no trusted network but `0.0.0.0/0`, and never a proxy's own address.
pub const UNKNOWN_CLIENT: SocketAddr =
    SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0);

/// The code a request whose forwarded headers cannot be read is refused
/// with (`400`).
pub const REFUSED_CODE: &str = "forwarded_headers_invalid";

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");

/// Every forwarded header lmgw knows, stripped from every request at the
/// edge: the three it reads, `Forwarded`, and the family's others that
/// nothing reads (`-Port`, `-Prefix`), so no later reader starts trusting a
/// client's own.
const STRIPPED: [&str; 6] = [
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
    "x-forwarded-port",
    "x-forwarded-prefix",
    "forwarded",
];

/// What a trusted proxy said of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forwarded {
    /// The client: the right-most hop that is not a trusted proxy, or
    /// [`UNKNOWN_CLIENT`].
    pub client: SocketAddr,
    /// The scheme the client used, when the proxy said.
    pub scheme: Option<Scheme>,
    /// The host the client asked for, when the proxy said.
    pub host: Option<String>,
}

/// Whether `ip` is inside one of `nets`.
pub fn is_trusted(nets: &[TrustedNet], ip: IpAddr) -> bool {
    nets.iter().any(|n| n.contains(ip))
}

/// The forwarded headers of a request from `peer`, read when `peer` is
/// inside `nets`: `None` when it is not, or when the request carries none
/// (a trusted address talking to lmgw directly: the health check, the CLI);
/// refused, saying why, when they cannot be read. Once any is there, a
/// chain with no hops is [`UNKNOWN_CLIENT`], never `peer`.
pub fn read(
    headers: &HeaderMap,
    peer: SocketAddr,
    nets: &[TrustedNet],
) -> Result<Option<Forwarded>, String> {
    if !is_trusted(nets, peer.ip()) {
        return Ok(None);
    }
    let x_family = [&X_FORWARDED_FOR, &X_FORWARDED_PROTO, &X_FORWARDED_HOST]
        .iter()
        .any(|h| headers.contains_key(*h));
    let rfc = headers.contains_key(header::FORWARDED);
    match (x_family, rfc) {
        (false, false) => Ok(None),
        (true, true) => Err(
            "the request carries both Forwarded and X-Forwarded-* headers: lmgw reads one \
             family, and the proxy in front of it must write one and drop the other (a \
             client's own of either would otherwise pass through it)"
                .into(),
        ),
        (true, false) => {
            let hops: Vec<Hop> = headers::list(headers, &X_FORWARDED_FOR)
                .iter()
                .map(|h| h.as_deref().map_or(Hop::Unknown, headers::hop))
                .collect();
            let client = match walk(&hops, nets) {
                Walked::At(_, client) => client,
                Walked::Empty => UNKNOWN_CLIENT,
            };
            let scheme = headers::single(headers, &X_FORWARDED_PROTO)?
                .map(|p| headers::proto(&p, "X-Forwarded-Proto"))
                .transpose()?;
            let host = headers::single(headers, &X_FORWARDED_HOST)?
                .map(|h| headers::host(&h, "X-Forwarded-Host"))
                .transpose()?;
            Ok(Some(Forwarded {
                client,
                scheme,
                host,
            }))
        }
        (false, true) => {
            let elements = headers::forwarded(headers)?;
            let hops: Vec<Hop> = elements
                .iter()
                .map(|e| e.for_.as_deref().map_or(Hop::Unknown, headers::hop))
                .collect();
            let (element, client) = match walk(&hops, nets) {
                Walked::At(i, client) => (Some(&elements[i]), client),
                Walked::Empty => (None, UNKNOWN_CLIENT),
            };
            let scheme = element
                .and_then(|e| e.proto.as_deref())
                .map(|p| headers::proto(p, "Forwarded's proto"))
                .transpose()?;
            let host = element
                .and_then(|e| e.host.as_deref())
                .map(|h| headers::host(h, "Forwarded's host"))
                .transpose()?;
            Ok(Some(Forwarded {
                client,
                scheme,
                host,
            }))
        }
    }
}

/// Where a walk of a chain ended.
enum Walked {
    /// At the hop of this index, with this client.
    At(usize, SocketAddr),
    /// The chain has no hops.
    Empty,
}

/// The chain `hops` walked from the right: the first hop that is not
/// inside `nets` is the client; one that names no address ends the walk
/// with the client unknown; with every hop trusted, the left-most.
fn walk(hops: &[Hop], nets: &[TrustedNet]) -> Walked {
    for (i, h) in hops.iter().enumerate().rev() {
        match h {
            Hop::Addr(a) if is_trusted(nets, a.ip()) => continue,
            Hop::Addr(a) => return Walked::At(i, *a),
            Hop::Unknown => return Walked::At(i, UNKNOWN_CLIENT),
        }
    }
    match hops.first() {
        Some(Hop::Addr(a)) => Walked::At(0, *a),
        Some(Hop::Unknown) => Walked::At(0, UNKNOWN_CLIENT),
        None => Walked::Empty,
    }
}

/// The main router's edge (the plain and the TLS listener, never the device
/// listener, which no proxy can stand in front of): a request with more
/// than one `Host` is refused (RFC 9112 §3.2); a trusted peer's forwarded
/// headers are read and written into the connect info and the `Host`; every
/// forwarded header is then stripped. While proxy mode is on, and in
/// container mode, where the line is how the administrator learns the
/// proxy's address, the first requests' peers are noted for the log and
/// the TLS card ([`ProxySeen`]). It stands outside the trace, so a refusal
/// here is logged in a request span of its own ([`refuse`]).
pub async fn edge(State(st): State<SharedState>, mut req: Request, next: Next) -> Response {
    if req.headers().get_all(header::HOST).iter().count() > 1 {
        return refuse(
            &req,
            "host_invalid",
            "the request carries more than one Host header (RFC 9112 §3.2)".into(),
        );
    }
    let conn = req
        .extensions()
        .get::<ConnectInfo<Conn>>()
        .map(|c| c.0.clone());
    if let Some(conn) = conn {
        let snap = st.snapshot();
        let nets = &snap.trusted_nets;
        let trusted = is_trusted(nets, conn.peer.ip());
        if !nets.is_empty() || st.deployment().is_container() {
            if let Some(first) =
                st.proxy_seen
                    .saw(conn.via, conn.peer.ip(), conn.local.ip(), trusted)
            {
                match first.warning() {
                    Some(w) => tracing::warn!("proxy mode: {w}"),
                    None => tracing::info!("{}", first.line()),
                }
            }
        }
        match read(req.headers(), conn.peer, nets) {
            Ok(None) => {}
            Ok(Some(f)) => {
                if let Some(h) = f.host.as_deref() {
                    set_host(&mut req, h);
                }
                if let Some(ci) = req.extensions_mut().get_mut::<ConnectInfo<Conn>>() {
                    ci.0.proxy = Some(conn.peer);
                    ci.0.peer = f.client;
                    ci.0.forwarded_scheme = f.scheme;
                }
            }
            Err(why) => {
                if st.proxy_seen.refused(conn.via, conn.peer.ip(), &why) {
                    tracing::warn!(
                        "proxy mode: a request from the trusted proxy {} was refused: {why} \
                         (later refusals are counted on Settings → Network & access → TLS)",
                        conn.peer.ip()
                    );
                }
                return refuse(&req, REFUSED_CODE, why);
            }
        }
    }
    for h in STRIPPED {
        req.headers_mut().remove(h);
    }
    next.run(req).await
}

/// The edge's `400` refusal of `req`, with its line in a request span as
/// the trace writes one ([`super::http_span`], debug, as every request's
/// is), so a refused request is in the log like any other and a client
/// cannot fill it at the default level.
fn refuse(req: &Request, code: &'static str, message: String) -> Response {
    super::http_span(req).in_scope(|| {
        tracing::debug!(status = 400, code, "refused at the edge: {message}");
    });
    Refusal {
        status: StatusCode::BAD_REQUEST,
        code,
        message,
    }
    .into_response()
}

/// The forwarded host `host` (checked by [`headers::host`]) where the
/// request's readers look: the `Host` header, and the URI's authority when
/// it has one (HTTP/2, whose `:authority` hyper renders there and which the
/// agent origins' dispatch reads first).
fn set_host(req: &mut Request, host: &str) {
    if let Ok(v) = HeaderValue::from_str(host) {
        req.headers_mut().insert(header::HOST, v);
    }
    if req.uri().authority().is_some() {
        let mut parts = req.uri().clone().into_parts();
        if let Ok(a) = host.parse::<axum::http::uri::Authority>() {
            parts.authority = Some(a);
            if let Ok(uri) = axum::http::Uri::from_parts(parts) {
                *req.uri_mut() = uri;
            }
        }
    }
}

/// What the TLS card says of proxy mode under `s` (§4.5): the first peers
/// and their warnings, a public URL left empty, a list that trusts
/// everyone, an entry that does not parse, the last refusal.
pub fn status(s: &crate::config::Settings, seen: &ProxySeen) -> lmgw_api_types::ProxyStatus {
    use lmgw_api_types::settings_labels::{PUBLIC_URL_AT, TRUSTED_PROXIES_AT};
    let nets = trusted_nets(&s.trusted_proxies);
    let first = seen.first_peers();
    let mut warnings: Vec<String> = first.iter().filter_map(FirstPeer::warning).collect();
    for bad in crate::config::unreadable_trusted_proxies(&s.trusted_proxies) {
        warnings.push(format!(
            "the stored trusted proxy '{bad}' does not parse and trusts nothing: correct it on \
             {TRUSTED_PROXIES_AT}"
        ));
    }
    if let Some(all) = nets.iter().find(|n| n.is_everyone()) {
        warnings.push(format!(
            "every address is a trusted proxy ({all}): any client can set the scheme, host and \
             client address lmgw sees. List only the proxy's address or network"
        ));
    }
    if !nets.is_empty() && crate::net::public_url(s).is_none() {
        warnings.push(format!(
            "proxy mode is on and no public URL is set: login links, snippets and agent origins \
             are built from lmgw's own listener, which people who come through the proxy may \
             not reach. Set {PUBLIC_URL_AT} to the address the proxy serves"
        ));
    }
    let since = seen.since();
    let refused = seen.last_refused().map(|r| {
        format!(
            "{} request(s) refused {since} for their forwarded headers; the last at {} UTC from \
             {} on the {} listener: {}",
            r.count,
            r.at.format("%Y-%m-%d %H:%M:%S"),
            r.peer,
            r.via,
            r.why
        )
    });
    lmgw_api_types::ProxyStatus {
        on: !nets.is_empty(),
        first_peers: first.iter().map(FirstPeer::line).collect(),
        warnings,
        refused,
    }
}

/// The start's line of proxy mode, `None` while it is off.
pub fn start_line(s: &crate::config::Settings) -> Option<String> {
    let nets = trusted_nets(&s.trusted_proxies);
    (!nets.is_empty()).then(|| {
        format!(
            "proxy mode: forwarded headers are read from {} (the scheme, the host and the \
             client address); from any other peer they are ignored",
            nets.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

#[cfg(test)]
mod tests;
