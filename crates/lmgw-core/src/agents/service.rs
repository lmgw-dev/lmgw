//! Service mode — layer 3 of the container runtime (design §3.3, §6.5).
//!
//! An agent that declares `run.service` serves its **own UI** (and, with
//! `run.provides.mcp`, its own tools) out of the same container image its run
//! and apply phases use. lmgw starts that container **on demand** — the first
//! proxied request pays for the start, every later one finds it warm — health-
//! probes it, reverse-proxies it on the agent's own origin (§4.1), and
//! idle-stops it the way [`McpManager::reap_idle`](crate::mcp::McpManager::reap_idle)
//! idle-stops an MCP connection, in-flight guard and `0 = never` included.
//!
//! **What is different from a phase run, and only this:**
//!
//! - `-d` instead of foreground, and **not** `--rm`: `podman logs` is the only
//!   account of a container that died during its health probe, and `--rm`
//!   would delete the evidence before the 503 could quote it.
//! - A published port. The host must reach the container here, which is the
//!   mirror image of [`container::gateway_access`]' problem and has the same
//!   answer — measured, not assumed: the container's own IP is not reachable
//!   from the host under rootless pasta, so the port is **published on
//!   loopback** (`-p 127.0.0.1:<host>:<port>`) on a host port lmgw picks per
//!   start with [`ephemeral_port`](crate::runtime::registry::ephemeral_port),
//!   and once more on a fresh one when podman finds it taken
//!   ([`PortRetry`](crate::runtime::registry::PortRetry)).
//!   The chosen port is in the run log and in `AgentDetail.service`.
//! - `LMGW_PHASE=service`, `LMGW_APP_ORIGIN=http://<id>.<suffix>:<port>`, and no ledger
//!   URL, run id or deadline: a service has no run to report to and is bounded
//!   by its idle window, not by a clock.
//!
//! Everything else is [`container`] unchanged — the same argv hygiene, the same
//! cgroup limits, the same `input.json`/`secrets.json` on a host tmpfs, the
//! same agent token, and the same stop ladder
//! ([`container::stop_ladder_inner`]).
//!
//! **Nothing here invents a bound.** The start is bounded by the visible
//! `service.start_timeout_seconds` (`0` = wait as long as it takes), the idle
//! window by the visible `service.idle_seconds` (`0` = never stop), and the
//! probe's sampling rate is exactly [`Registry`](crate::runtime::registry)'s
//! own `HEALTH_POLL` reasoning: a rate, not a bound — the poll runs until the
//! visible timeout is spent and this only says how often it looks.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::watch;

use crate::agents::container::{self, Mount, Published, RunDir, RunSpecArgs};
use crate::agents::manifest::{Manifest, RunSpec, Service};
use crate::agents::{mounts, token, Agent};
use crate::config::{McpServer, McpTransport, Settings};
use crate::runtime::registry::{ephemeral_port, PortRetry};
use crate::runtime::slug;
use crate::state::SharedState;
use crate::store::{self, NewMcpServer};

/// How often a starting service container is probed.
///
/// A **sampling rate, not a bound** — the same distinction (and the same
/// 250 ms) `runtime::registry::HEALTH_POLL` documents for a starting model:
/// the poll runs until the caller's visible timeout
/// (`service.start_timeout_seconds`) is spent, and this only says how often it
/// looks. That constant is private to the registry, so it is restated here
/// rather than made public across the seam.
const HEALTH_POLL: Duration = Duration::from_millis(250);

/// How many lines of `podman logs` a failed start quotes back, and how many the
/// App tab shows.
///
/// [`container::STDERR_EXCERPT_LINES`] itself, not a second number that agrees
/// with it today: the excerpt shape the rest of the runtime uses. Only the
/// *message* is trimmed — the container's full log is still one `podman logs`
/// away — and the App tab prints the count beside the tail so nobody has to
/// guess whether they are seeing all of it.
pub const LOG_EXCERPT_LINES: usize = container::STDERR_EXCERPT_LINES;

// ---------------------------------------------------------------------------
// The manifest half
// ---------------------------------------------------------------------------

/// The `service` block, if this agent declares one.
pub fn service_of(agent: &Agent) -> Option<&Service> {
    service_in(&agent.manifest)
}

/// The same, one step earlier: a manifest on its way in, before there is a row
/// for it. The origin checks at manifest write (§4.1) run here.
pub fn service_in(m: &Manifest) -> Option<&Service> {
    match &m.run {
        RunSpec::Container { service, .. } => service.as_ref(),
        _ => None,
    }
}

/// The same question asked of a **row**, by id: does `<id>` name an agent that
/// serves an app?
///
/// Read the way the proxy reads a row by id — [`store::get_agent`] then
/// [`Agent::from_row`] — and false for all three ways of not being one: no such
/// row, a row whose manifest no longer parses, and a manifest with no
/// `run.service`. The two callers ask it about a host name (the `/mcp` origin
/// guard) and about a path (the moved app mount), and neither has anything
/// different to say about the three.
pub async fn is_service_agent(state: &SharedState, agent_id: &str) -> bool {
    match store::get_agent(&state.db, agent_id).await {
        Ok(Some(row)) => Agent::from_row(row).is_ok_and(|a| service_of(&a).is_some()),
        _ => false,
    }
}

/// The `provides.mcp` path, if this agent declares one. Implies
/// [`service_of`] — `manifest::validate` refuses `provides` without `service`.
pub fn provides_mcp(agent: &Agent) -> Option<&str> {
    match &agent.manifest.run {
        RunSpec::Container { provides, .. } => provides.as_ref()?.mcp.as_deref(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The agent origin (origins design §4.1, §4.6, §4.9)
// ---------------------------------------------------------------------------

/// `<id>.<agent_origin_suffix>` — the host name this agent's UI answers on.
///
/// The label **is** the id: [`validate_id`](crate::agents::manifest::validate_id) already restricts it to
/// `[a-z0-9-]`, so there is no slug, no lossy mapping and no second namespace
/// to keep unique. The two rules a DNS label adds on top (63 characters, no
/// trailing `-`) are checked where a manifest is written
/// ([`origin_label_refusal`]), not here, because this has to render the origin
/// of a row that is already stored whatever it says.
pub fn origin_host(settings: &Settings, agent_id: &str) -> String {
    format!("{agent_id}.{}", settings.agent_origin_suffix)
}

/// [`origin_host`] read backwards: the agent id a host name is the origin of,
/// or `None` for every host that is not one (§4.2).
///
/// A bare host — the caller has already taken the port off whichever header it
/// parsed. Three shapes never match, whatever the suffix is spelled as: a
/// bracketed IPv6 literal, a bare IP address (a suffix of `1` must not make
/// `127.0.0.1` an origin for the agent `127.0.0`), and the suffix on its own,
/// which is a host the gateway may well answer on itself. DNS is
/// case-insensitive, so the label comes back lower-cased and ready to look up.
///
/// **One trailing dot comes off first.** `board.localhost.` is the absolute
/// form of the same name, it is what a reader who knows DNS types, and every
/// resolver and browser treats the two as one host — so a match that failed on
/// the dot would hand `board.localhost.` to the main router and serve the
/// dashboard, login card and all, on a name inside the agent namespace.
/// Exactly one: `board.localhost..` is not a host name in any form and stays
/// unmatched.
///
/// It says nothing about whether that agent exists or serves an app: that is a
/// row, read by the caller ([`is_service_agent`] for the guards that only need
/// the verdict). Nor does it vouch for the label being an id — `evil.board` and
/// `user@board` come back as they are, and the id lookup is what refuses them,
/// because an id is `[a-z0-9-]` and neither can be one.
pub fn origin_label(settings: &Settings, host: &str) -> Option<String> {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.starts_with('[') || host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let host = host.to_ascii_lowercase();
    let suffix = settings.agent_origin_suffix.to_ascii_lowercase();
    let label = host.strip_suffix(&suffix)?.strip_suffix('.')?;
    (!label.is_empty()).then(|| label.to_string())
}

/// `<id>.<suffix>:<bind port>` — the authority, for a `Host` header or a
/// resolver call.
///
/// The port is `bind_addr`'s: the agent origin answers on the same socket the
/// dashboard does and only the `Host` header differs (§4.1). A `bind_addr`
/// that names no port at all cannot happen through the settings plane (it is
/// parsed as a `SocketAddr` on the way in) and renders without one here rather
/// than having a number invented for it.
pub fn origin_authority(settings: &Settings, agent_id: &str) -> String {
    let host = origin_host(settings, agent_id);
    match own_port(&settings.bind_addr) {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

/// `http://board.localhost:8001` — the origin **without** a trailing slash.
///
/// Exactly the string `LMGW_APP_ORIGIN` and `input.json`'s `service.origin`
/// carry (§4.6): an origin, which is what a server-side framework concatenates
/// a path onto. [`agent_origin`] is the same origin as a *URL*, with the
/// slash, and the two forms are deliberately not interchangeable — the design
/// table prints both.
pub fn agent_origin_base(settings: &Settings, agent_id: &str) -> String {
    format!("http://{}", origin_authority(settings, agent_id))
}

/// `http://board.localhost:8001/` — the origin as a URL to put in an `href` or
/// an iframe `src`, which is what `dto::AgentService.origin` is (§4.9).
pub fn agent_origin(settings: &Settings, agent_id: &str) -> String {
    format!("{}/", agent_origin_base(settings, agent_id))
}

/// How long the App tab's `origin_resolves` probe waits for the resolver.
///
/// A **bound on one lookup, not a verdict**: NSS answers `*.localhost` out of
/// systemd-resolved without touching the network, and a configured zone answers
/// from the local cache, so half a second is a resolver that is not going to
/// answer. What the reader sees when it expires is the same line an `NXDOMAIN`
/// produces — "does not resolve on this machine", with the hosts-file
/// instruction beside it — never a spinner and never an HTTP error.
pub const ORIGIN_LOOKUP_TIMEOUT: Duration = Duration::from_millis(500);

/// Does `<id>.<suffix>` resolve **on this box**, for the App tab's verdict
/// (§4.9)?
///
/// The same NSS the WebKitGTK window goes through, which is the point: Chrome
/// and Firefox resolve `*.localhost` internally whatever the system says, so
/// this answers for the shell and for `curl`. A resolver error is `false`, and
/// so is a timeout — never an error on the detail route, which has a dozen
/// other things to report.
pub async fn origin_resolves(settings: &Settings, agent_id: &str) -> bool {
    let authority = origin_authority(settings, agent_id);
    match tokio::time::timeout(ORIGIN_LOOKUP_TIMEOUT, tokio::net::lookup_host(authority)).await {
        Ok(Ok(mut addrs)) => addrs.next().is_some(),
        _ => false,
    }
}

/// The DNS-label half of a service-declaring manifest's id (§4.1), as the
/// refusal message or nothing.
///
/// [`validate_id`](crate::agents::manifest::validate_id) allows 64 characters and a trailing `-`; a DNS
/// label allows neither. The rule applies **only** to a manifest that declares
/// `run.service`, because only that agent gets an origin — an id nobody serves
/// is not a host name.
///
/// Its own function rather than a branch inside the writer for the reason
/// `ops::owner_key_refusal` is: the `/api` plane answers it with the
/// `origin_label_invalid` code (design §7), and a code is attached in `web`,
/// where every other code is.
pub fn origin_label_refusal(m: &Manifest) -> Option<String> {
    service_in(m)?;
    let id = &m.id;
    if id.len() > 63 {
        return Some(format!(
            "'{id}' is {} characters, and this agent declares run.service — its id is also the \
             DNS label its UI is served under (http://{id}.<suffix>/), and a label is at most 63. \
             Shorten the id, or drop run.service.",
            id.len()
        ));
    }
    if id.ends_with('-') {
        return Some(format!(
            "'{id}' ends in '-', and this agent declares run.service — its id is also the DNS \
             label its UI is served under (http://{id}.<suffix>/), and a label cannot end in '-'. \
             Rename it, or drop run.service."
        ));
    }
    None
}

/// The other half (§4.1): a service agent whose `<id>.<suffix>` is a name the
/// gateway itself answers on would capture the dashboard's own address.
/// Refused at manifest write with `origin_shadows_gateway`.
///
/// **It can only ever fire against a multi-label name.** `<id>.<suffix>` always
/// carries a dot, and in production `bind_addr` is always a `SocketAddr` (the
/// settings plane parses it as one on the way in), so the addresses it
/// contributes to [`own_host_names`](crate::net::own_host_names) are IP
/// literals that no label can equal. What is left to collide with is the FQDN
/// half of that list: an `/etc/hostname` holding a full name, the box's
/// `<host>.local`, its `<host>.<search domain>`, or a `bind_addr` that is a
/// name rather than an address — which only a hand-written settings row is.
/// That is the intended reach and it stays: the suffix rule
/// ([`origin_suffix_refusal`]) is the one that has to hold for every agent at
/// once, and this one is the per-agent backstop.
pub fn origin_shadows_refusal(settings: &Settings, m: &Manifest) -> Option<String> {
    service_in(m)?;
    let host = origin_host(settings, &m.id);
    let own = crate::net::own_host_names(&settings.bind_addr);
    own.iter().find(|h| **h == host).map(|h| {
        format!(
            "'{}' declares run.service, so its UI would be served at http://{host}/ — and \
                 '{h}' is this gateway's own address. The dashboard would lose it to the agent. \
                 Rename the agent, or set a different agent origin suffix under Settings → \
                 Agents & tools.",
            m.id
        )
    })
}

/// The suffix half, from the other end (§4.1): `agent_origin_suffix` may not be
/// a suffix of any name the gateway answers on, because an agent named like
/// that host's first label would then capture the dashboard — and it may not
/// share a cookie-able parent domain with one either.
///
/// Shape is checked where the setting is written (`api_settings`); this is the
/// rule that earns the `origin_suffix_shadows_gateway` code.
pub fn origin_suffix_refusal(suffix: &str, bind_addr: &str) -> Option<String> {
    suffix_refusal_against(suffix, &crate::net::own_host_names(bind_addr))
}

/// The stored suffix re-asked at boot and at every read of the settings page
/// (§4.1, F3): the rule is checked when the *suffix* is written, but a
/// `bind_addr` change, a new host name or a new search domain can make a suffix
/// that was fine start shadowing without anyone touching it.
///
/// A warning, never a reset: the owner set that value and a gateway that
/// silently renamed every agent's origin at boot would be a worse surprise than
/// the one it is reporting.
pub fn origin_suffix_warning(settings: &Settings) -> Option<String> {
    origin_suffix_refusal(&settings.agent_origin_suffix, &settings.bind_addr)
}

/// [`origin_suffix_refusal`] against an explicit list of own host names — the
/// whole rule, with the one environment read lifted out so each branch is
/// testable on a box whose name and search domain are whatever they are.
fn suffix_refusal_against(suffix: &str, own: &[String]) -> Option<String> {
    let suffix = suffix.trim().trim_matches('.').to_ascii_lowercase();
    if suffix.is_empty() {
        return None;
    }
    // `local` is the mDNS zone, and nothing lmgw can do makes it answer
    // `<id>.local` for an arbitrary id: the responder on this box answers for
    // *its own* name only. Refused whatever the host name happens to be,
    // because the names it would collide with are the ones a laptop acquires
    // by being on a network at all.
    if suffix == "local" {
        return Some(
            "'local' is the mDNS zone: it is answered by the responder on each machine for that \
             machine's own name, so nothing here can make it resolve '<id>.local' for an agent — \
             and this box already answers on its own '<host>.local', which an agent named like \
             the host would take. Pick a suffix of your own (the default, 'localhost', resolves \
             to this machine in every browser)."
                .to_string(),
        );
    }
    let dotted = format!(".{suffix}");
    if let Some(h) = own.iter().find(|h| **h == suffix || h.ends_with(&dotted)) {
        return Some(format!(
            "'{suffix}' is part of '{h}', which is an address this gateway answers on. An agent \
             whose id is that name's first label would be served at the dashboard's own address \
             and take it. Pick a suffix of your own (the default, 'localhost', resolves to this \
             machine in every browser)."
        ));
    }
    // And the cookie direction (F8): a page under the suffix cannot *read* the
    // dashboard's `lmgw_session` — that is one origin's cookie jar and a
    // different host name — but it can set one with `Domain=<shared parent>`,
    // which the browser then sends to the dashboard as well and which
    // overwrites the session the owner is logged in with. Only a parent of two
    // labels or more is reachable that way: a browser refuses a single-label
    // cookie domain, so `.lan` and `.localhost` cannot be written to and the
    // default under a bare host name or an IP bind stays legal.
    for h in own {
        if h.starts_with('[') || h.parse::<std::net::IpAddr>().is_ok() {
            continue;
        }
        for parent in cookie_parents(h) {
            if suffix == parent || suffix.ends_with(&format!(".{parent}")) {
                return Some(format!(
                    "'{suffix}' and '{h}', which is an address this gateway answers on, are both \
                     under '{parent}'. A page served on an agent origin could set a cookie with \
                     Domain={parent}; the browser would send it to the dashboard too, and it \
                     would overwrite the session cookie the owner is logged in with. Pick a \
                     suffix outside the dashboard's own domain (the default, 'localhost', \
                     resolves to this machine in every browser)."
                ));
            }
        }
    }
    None
}

/// Every domain a page on `host` could set a cookie for: the name itself and
/// each of its parents down to two labels.
///
/// Two is where it stops because that is where the browser stops — a `Domain`
/// of one label (`lan`, `localhost`, a TLD) is refused by every engine, which
/// is the only reason the shipped default is safe next to a host called `myhost`.
fn cookie_parents(host: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = host;
    while rest.matches('.').count() >= 1 {
        out.push(rest);
        match rest.split_once('.') {
            Some((_, tail)) => rest = tail,
            None => break,
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The dev override (§3.4)
// ---------------------------------------------------------------------------

/// The row's `dev_url`, if one is set: the hot-reload loop outside podman.
///
/// **On the row, never in the manifest** (§3.4) — a manifest naming
/// `localhost:5173` would ship a broken agent — and it overrides *service mode
/// only*: a run or an apply still starts the image, because their dev loop is a
/// rebuild.
pub fn dev_url_of(agent: &Agent) -> Option<String> {
    agent
        .row
        .dev_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
}

/// Check a `dev_url` before it is stored, and return it in the one shape the
/// proxy concatenates (`scheme://host[:port]`, no path and no trailing slash).
///
/// **Loopback only** (WP5 review decision). The proxy that reads this sits on
/// the dashboard plane: `CorsLayer::permissive()`, same origin as `/api/op/*`,
/// and the owner's own session behind it (§2, §3.3). Pointing it at a *LAN*
/// address would turn that
/// plane into an open reverse proxy for another machine — anything that can
/// reach lmgw could reach that host through it, on lmgw's own origin — and the
/// posture §3.3 accepted was for **the agent's own container**, not for an
/// arbitrary host on the network. The use case does not need it: a `trunk
/// serve` runs on the box the dashboard is open on. So `localhost`,
/// `127.0.0.0/8`, `[::1]` and nothing else; widening this to a LAN would be an
/// explicit, visible opt-in and the owner's call, not a default.
///
/// **No userinfo**: `http://user:pass@127.0.0.1` would store a credential on a
/// row that is read back onto the page, and the proxy would send it upstream on
/// every request.
///
/// **Not lmgw's own address.** `bind_addr`'s port makes `/agents/<id>/app/`
/// proxy to lmgw itself: each request re-enters the router, which proxies
/// again, until the file descriptors run out. Refused by naming the loop.
///
/// No query and no fragment: a base URL is a base URL, and the proxy appends
/// the request's own path and attaches the request's own query to it — a stored
/// `?x=1` would be dropped on the first request and nobody would know why.
pub fn validate_dev_url(raw: &str, bind_addr: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("a dev_url is an http(s) URL; pass null to clear it".to_string());
    }
    let url = reqwest::Url::parse(raw).map_err(|e| format!("'{raw}' is not a URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "a dev_url must be http or https; '{raw}' is {}",
            url.scheme()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "a dev_url must not carry userinfo: lmgw would store that credential on the row and \
             send it upstream on every proxied request"
                .to_string(),
        );
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "a dev_url is a base URL with no query and no fragment — the proxy appends the \
             request's own path and query to it; got '{raw}'"
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| format!("'{raw}' names no host"))?
        .to_string();
    if !is_loopback_host(&host) {
        return Err(format!(
            "a dev_url has to be on this box: '{host}' is not loopback (localhost, 127.0.0.0/8, \
             ::1). The app proxy sits on the dashboard plane, which carries no authentication \
             and permissive CORS, so pointing it at another host would make lmgw an open \
             reverse proxy for that host."
        ));
    }
    if let Some(port) = url.port_or_known_default() {
        if own_port(bind_addr) == Some(port) {
            return Err(format!(
                "a dev_url must not be lmgw itself: '{host}:{port}' is where this gateway is \
                 listening, so /agents/<id>/app/ would proxy into its own router and loop until \
                 the file descriptors ran out"
            ));
        }
    }
    // **No path** (origins §4.7). The agent's app is served at the root of its
    // own origin now, so there is no prefix left for anything to strip: a
    // stored `/base` would be prepended to every request's own path and the
    // dev server would answer `/base/base/...`. A bare `/` is the same origin
    // written with its slash and is normalised away below rather than refused.
    if !url.path().trim_matches('/').is_empty() {
        return Err(format!(
            "a dev_url is an origin; it cannot carry a path — got '{}' in '{raw}'. Run `trunk \
             serve` / `vite` with no --public-url or --base: the app is served at the root of \
             its own agent origin.",
            url.path()
        ));
    }
    Ok(raw.trim_end_matches('/').to_string())
}

/// `podman logs --tail <lines>` for one agent container, with **this agent's
/// own token taken back out** (§3.1, final review).
///
/// The one way a service container's log reaches a reader — the App tab's
/// block, `lmgw__agent_get`, the 503 body of a failed start — so the redaction
/// lives here rather than at each of them. A container that prints its
/// `/lmgw/secrets.json` is doing something lmgw cannot stop; republishing the
/// result on an unauthenticated page is something it can.
pub async fn log_tail(
    state: &SharedState,
    agent_id: &str,
    container_name: &str,
    lines: usize,
) -> String {
    let text = container::logs_tail(&state.agent_spawner(), container_name, lines).await;
    token::redact(
        token::plaintext_of(&state.snapshot(), agent_id).as_deref(),
        &text,
    )
}

/// Re-check every stored `dev_url` against the rules as they stand now,
/// clearing the ones that are no longer legal (§3.4, origins §4.7).
///
/// A `dev_url` is validated once, when it is entered — and one of the things it
/// is validated against is `bind_addr`, because a dev URL pointing at lmgw's
/// own port makes the app proxy carry the dashboard into its own router until
/// the file descriptors run out. Nothing re-asked the question when the *bind
/// address moved onto a stored dev port*, so a settings save could arm that
/// loop from the other side, permanently and silently.
///
/// Since the agent origin the rules themselves move too: a row stored before
/// that part may carry a path, which nothing strips any more. So this also
/// runs **at boot** (`AppState::init`, after the seed and `resync_all`), where
/// a stored `/base` is cleared once rather than proxied to a prefix the dev
/// server never sees.
///
/// Offenders are **cleared**, not disabled: a dev_url is a developer's
/// temporary override, the developer is right there, and leaving a stored value
/// that is refused on use would be a row that says it is served from a dev
/// server and is not. What was dropped is recorded in
/// [`DEV_URL_CLEARED_KEY`](crate::agents::DEV_URL_CLEARED_KEY) so the row can
/// say so after the restart the new bind address needs.
///
/// Returns `(agent id, the url that was dropped, why)`, in id order.
pub async fn revalidate_dev_urls(
    state: &SharedState,
    bind_addr: &str,
) -> Vec<(String, String, String)> {
    let urls = match store::agent_dev_urls(&state.db).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("stored dev_urls could not be re-checked against the bind address: {e}");
            return Vec::new();
        }
    };
    let mut ids: Vec<&String> = urls.keys().collect();
    ids.sort();
    let mut cleared = Vec::new();
    for id in ids {
        let url = &urls[id];
        let Err(why) = validate_dev_url(url, bind_addr) else {
            continue;
        };
        if let Err(e) = store::set_agent_dev_url(&state.db, id, None).await {
            tracing::warn!("agent '{id}': its dev_url could not be cleared: {e}");
            continue;
        }
        cleared.push((id.clone(), url.clone(), why));
    }
    if !cleared.is_empty() {
        let mut map = cleared_dev_urls(state).await;
        for (id, url, why) in &cleared {
            map.insert(
                id.clone(),
                Cleared {
                    url: url.clone(),
                    why: why.clone(),
                },
            );
        }
        let value = serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, json!({ "url": v.url, "why": v.why })))
                .collect(),
        );
        if let Err(e) = store::set_kv(
            &state.db,
            crate::agents::DEV_URL_CLEARED_KEY,
            &value.to_string(),
        )
        .await
        {
            tracing::warn!("the cleared dev_url note could not be recorded: {e}");
        }
    }
    cleared
}

/// One agent's cleared-dev_url note: what was dropped, and the refusal that
/// dropped it.
///
/// The reason travels with the url because there are two of them now — a bind
/// address that moved onto the dev port, and a path a dev_url may no longer
/// carry (§4.7) — and a row that says only "it was cleared" sends the owner
/// looking in the wrong place.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cleared {
    pub url: String,
    pub why: String,
}

/// What [`DEV_URL_CLEARED_KEY`](crate::agents::DEV_URL_CLEARED_KEY) holds right
/// now: agent id → [`Cleared`].
///
/// A note written before the reason existed is a bare string, and is read as a
/// url with no reason rather than dropped — an upgrade does not get to lose the
/// one record of why an agent is back on its image.
pub async fn cleared_dev_urls(state: &SharedState) -> std::collections::BTreeMap<String, Cleared> {
    let raw = store::get_kv(&state.db, crate::agents::DEV_URL_CLEARED_KEY)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let stored: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&raw).unwrap_or_default();
    stored
        .into_iter()
        .filter_map(|(id, v)| match v {
            serde_json::Value::String(url) => Some((
                id,
                Cleared {
                    url,
                    why: String::new(),
                },
            )),
            serde_json::Value::Object(o) => Some((
                id,
                Cleared {
                    url: o.get("url")?.as_str()?.to_string(),
                    why: o
                        .get("why")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                },
            )),
            _ => None,
        })
        .collect()
}

/// Forget the cleared-dev_url note for one agent — the owner has answered it,
/// by setting a dev_url again or by clearing the field themselves.
pub async fn forget_cleared_dev_url(state: &SharedState, agent_id: &str) {
    let mut map = cleared_dev_urls(state).await;
    if map.remove(agent_id).is_none() {
        return;
    }
    let value = serde_json::Value::Object(
        map.into_iter()
            .map(|(k, v)| (k, json!({ "url": v.url, "why": v.why })))
            .collect(),
    )
    .to_string();
    if let Err(e) = store::set_kv(&state.db, crate::agents::DEV_URL_CLEARED_KEY, &value).await {
        tracing::warn!("the cleared dev_url note could not be updated: {e}");
    }
}

/// `localhost`, `127.0.0.0/8` or `::1`. Nothing else — see [`validate_dev_url`].
fn is_loopback_host(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

/// The port lmgw is listening on, from `settings.bind_addr`.
///
/// The **port** is the whole comparison, and the host is deliberately not part
/// of it: everything that reaches this check is already loopback, and
/// `127.0.0.1`, `localhost`, `127.0.0.2` and `::1` all land on the same
/// listener whether lmgw is bound to `0.0.0.0` or to `127.0.0.1`. Comparing
/// host strings would wave `http://localhost:8787` through against a
/// `bind_addr` of `127.0.0.1:8787` — the loop this exists to stop.
///
/// Public because the MCP ingress' rebinding guard asks the same question of
/// an `Origin`'s port (§4.2): a page on `http://localhost:9999` is a different
/// origin served by a different local process, and the one number that tells
/// it from the dashboard is this one.
pub fn own_port(bind_addr: &str) -> Option<u16> {
    bind_addr.rsplit(':').next()?.parse::<u16>().ok()
}

/// What serves this agent's app right now.
///
/// The proxy asks for this rather than for a container, because with a
/// `dev_url` set there is no container to ask for and starting one would be a
/// container nothing routes to.
pub enum Target {
    /// The row's `dev_url`. Nothing was started, nothing is idle-stopped, and
    /// no in-flight guard is held: lmgw did not start that server and does not
    /// get to stop it.
    Dev(String),
    Container(Arc<Live>),
}

impl Target {
    /// The origin (and optional path prefix) the request is forwarded to, with
    /// no trailing slash — the shape `format!("{base}{path}")` needs.
    pub fn base(&self) -> String {
        match self {
            Self::Dev(url) => url.trim_end_matches('/').to_string(),
            Self::Container(l) => l.base(),
        }
    }

    /// What an error message calls it.
    pub fn describe(&self) -> String {
        match self {
            Self::Dev(url) => format!("the dev server at {url}"),
            Self::Container(l) => format!("the container '{}'", l.container),
        }
    }
}

/// [`ensure`], except that a row with a `dev_url` needs nothing started.
///
/// The one entry point the proxy uses, so "is there a dev override?" is asked
/// in exactly one place and cannot be forgotten by one of the two mounts.
pub async fn target(state: &SharedState, agent: &Agent) -> Result<Target, Arc<StartError>> {
    match dev_url_of(agent) {
        Some(url) => Ok(Target::Dev(url)),
        None => ensure(state, agent).await.map(Target::Container),
    }
}

// ---------------------------------------------------------------------------
// The live map (§6.5)
// ---------------------------------------------------------------------------

/// One running service container.
///
/// Held as an `Arc` by the map **and** by every request in flight, which is what
/// makes [`RunDir`]'s `Drop` correct: the secrets file goes when the last
/// reader is done with it, not when the map entry is removed.
#[derive(Debug)]
pub struct Live {
    pub agent_id: String,
    pub container: String,
    /// The host side of `-p 127.0.0.1:<host>:<service.port>`, picked per start.
    pub host_port: u16,
    pub started_at: Instant,
    pub started_at_utc: chrono::DateTime<chrono::Utc>,
    /// `service.idle_seconds` as this start read it. `<= 0` is never.
    pub idle_seconds: i64,
    /// `limits.stop_grace_seconds` as this start read it.
    pub stop_grace_seconds: u64,
    last_used: Mutex<Instant>,
    in_flight: Arc<AtomicUsize>,
    /// The run directory holding `input.json` and `secrets.json`. Never read
    /// here; owned so that stopping the service deletes the token with it.
    _dir: RunDir,
}

impl Live {
    /// `http://127.0.0.1:<host port>` — what the proxy and the health probe
    /// dial. **Never the container's own IP**: it is not reachable from the
    /// host under rootless pasta.
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.host_port)
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    pub fn idle_for(&self) -> Duration {
        self.last_used.lock().unwrap().elapsed()
    }

    pub fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    /// Claim the guard the idle sweep skips (§3.3).
    ///
    /// Held for the **whole** life of a proxied request, streaming body
    /// included — the same discipline `McpManager::call`'s `InFlightGuard`
    /// uses, and for the same reason: `last_used` only advances when a request
    /// *finishes*, so an SSE stream open for an hour would otherwise look idle
    /// the entire time and be torn down mid-flight.
    pub fn guard(self: &Arc<Self>) -> InFlight {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlight(self.clone())
    }
}

/// The in-flight counter, released on drop — including when the request future
/// is dropped by a client that went away.
pub struct InFlight(Arc<Live>);

impl InFlight {
    /// Keep the idle window open while a stream is still delivering. Called per
    /// chunk, so a long SSE response is not idle merely because it started a
    /// while ago.
    pub fn touch(&self) {
        self.0.touch();
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        // The request is over *now*: the idle window starts here, not when it
        // began.
        self.0.touch();
    }
}

/// Why a start did not produce a reachable container.
#[derive(Debug, Clone)]
pub struct StartError {
    pub reason: String,
    /// `podman logs --tail` of the container that failed, when there was one.
    pub log: String,
    /// The container this start had created before it failed, if it got that
    /// far. What a cancelling [`stop`] reports back, so "Stop" can say what it
    /// actually removed instead of "nothing was running".
    pub container: Option<String>,
    /// The start was cancelled by a [`stop`] rather than failing on its own.
    pub cancelled: bool,
}

impl StartError {
    fn plain(reason: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            reason: reason.into(),
            log: String::new(),
            container: None,
            cancelled: false,
        })
    }
}

type Outcome = Result<Arc<Live>, Arc<StartError>>;

/// The handle a [`stop`] uses to interrupt a start that is already running.
///
/// A `watch` rather than a `Notify`: a canceller that fires between a waiter's
/// "is it cancelled yet" check and its `await` must not be missed, and
/// `borrow_and_update` is what closes that window.
#[derive(Clone)]
struct Cancel(Arc<watch::Sender<bool>>);

impl Cancel {
    fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }
    fn fire(&self) {
        let _ = self.0.send(true);
    }
    /// Resolves as soon as [`fire`](Self::fire) has been called — now or later.
    /// **Never** resolves otherwise, so it is only ever the losing branch of a
    /// `select!`.
    async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        loop {
            if *rx.borrow_and_update() {
                return;
            }
            if rx.changed().await.is_err() {
                // The sender is gone, which can only happen once the start that
                // owned it is over. Park forever rather than reporting a cancel
                // nobody asked for.
                std::future::pending::<()>().await;
            }
        }
    }
}

/// A start in flight: what waiters park on, and what cancels it.
struct Starting {
    answer: watch::Sender<Option<Outcome>>,
    cancel: Cancel,
}

enum Slot {
    /// A start is in flight; every other caller parks on this, and a [`stop`]
    /// can interrupt it (§3.3). Without the cancel half, the five paths that
    /// stop a container — Stop, delete, disable, rotate, a manifest replace —
    /// all answered "nothing was running" while the container came up
    /// afterwards as an orphan nothing was tracking.
    Starting(Starting),
    Ready(Arc<Live>),
    /// A stop is in flight. Held rather than removed so that a request
    /// arriving mid-stop **waits** instead of racing a `podman run --replace`
    /// against the `podman rm -f` that is still going: the waiter parks until
    /// the entry is dropped, then starts a clean one. The sender carries
    /// nothing — the *drop* is the signal.
    Stopping(watch::Sender<()>),
}

/// The service containers this gateway has running, one entry per agent.
///
/// Beside the model registry's map rather than in it (§6.5): a service
/// container is keyed by agent id, not by `(class, model)`, and it is started
/// by the agent spawner seam rather than the registry's `CommandRunner`.
#[derive(Default)]
pub struct Services {
    slots: Mutex<HashMap<String, Slot>>,
}

impl Services {
    /// The live entry for an agent, if it is running.
    pub fn get(&self, agent_id: &str) -> Option<Arc<Live>> {
        match self.slots.lock().unwrap().get(agent_id) {
            Some(Slot::Ready(l)) => Some(l.clone()),
            _ => None,
        }
    }

    /// Every running service, for the sweep and for a shutdown.
    pub fn live(&self) -> Vec<Arc<Live>> {
        self.slots
            .lock()
            .unwrap()
            .values()
            .filter_map(|s| match s {
                Slot::Ready(l) => Some(l.clone()),
                _ => None,
            })
            .collect()
    }

    /// Is a start in flight for this agent?
    pub fn starting(&self, agent_id: &str) -> bool {
        matches!(
            self.slots.lock().unwrap().get(agent_id),
            Some(Slot::Starting(_))
        )
    }

    /// Every agent that has an entry at all — running, starting or stopping.
    pub fn keys(&self) -> Vec<String> {
        self.slots.lock().unwrap().keys().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// On-demand start (§3.3)
// ---------------------------------------------------------------------------

/// The container for this agent, started if it is not already up.
///
/// **One start, many waiters**: N concurrent first requests produce exactly one
/// `podman run`, the shape `Registry::acquire` uses. The claim is made under
/// the map lock and the start itself runs in a spawned task, so a client that
/// disconnects halfway through a 20-second image start cannot abort it and
/// leave a container nobody is tracking.
pub async fn ensure(state: &SharedState, agent: &Agent) -> Outcome {
    let id = agent.row.id.clone();
    loop {
        enum Act {
            Ready(Arc<Live>),
            Wait(watch::Receiver<Option<Outcome>>),
            WaitForStop(watch::Receiver<()>),
        }
        let act = {
            let mut slots = state.agent_services.slots.lock().unwrap();
            match slots.get(&id) {
                Some(Slot::Ready(l)) => Act::Ready(l.clone()),
                Some(Slot::Starting(s)) => Act::Wait(s.answer.subscribe()),
                Some(Slot::Stopping(tx)) => Act::WaitForStop(tx.subscribe()),
                None => {
                    let (tx, rx) = watch::channel(None);
                    let cancel = Cancel::new();
                    slots.insert(
                        id.clone(),
                        Slot::Starting(Starting {
                            answer: tx.clone(),
                            cancel: cancel.clone(),
                        }),
                    );
                    let st = state.clone();
                    let agent = agent.clone();
                    let key = id.clone();
                    tokio::spawn(async move {
                        let outcome = start_once(&st, &agent, &cancel).await;
                        {
                            let mut slots = st.agent_services.slots.lock().unwrap();
                            match &outcome {
                                Ok(live) => {
                                    slots.insert(key, Slot::Ready(live.clone()));
                                }
                                Err(_) => {
                                    slots.remove(&key);
                                }
                            }
                        }
                        // After the map, so a waiter that wakes and re-reads
                        // the map finds the same answer the channel gave it.
                        let _ = tx.send(Some(outcome));
                    });
                    Act::Wait(rx)
                }
            }
        };
        match act {
            Act::Ready(live) => {
                live.touch();
                return Ok(live);
            }
            Act::Wait(rx) => match wait_for(rx).await {
                Some(outcome) => return outcome,
                // The starter's task vanished without answering (only possible
                // if the runtime is shutting down). Ask again rather than
                // inventing a failure.
                None => continue,
            },
            // The stop finishes by dropping its sender, which is what ends
            // this wait; then the loop finds an empty slot and starts fresh.
            Act::WaitForStop(mut rx) => {
                let _ = rx.changed().await;
                continue;
            }
        }
    }
}

/// [`ensure`] for a caller that has an agent id and not a parsed agent — the
/// MCP plane, which reaches a service container by its `agent:<id>` row.
///
/// Returns `()` rather than the entry, because both callers want the side
/// effect and nothing else: "this agent's app is up now, or here is why it is
/// not". That is also what makes the `dev_url` case expressible — a row served
/// from a dev server has **nothing to start**, and a `tools/call` against it
/// must succeed and go through the proxy to that server rather than fail
/// because there was no container to bring up.
pub async fn ensure_by_id(state: &SharedState, agent_id: &str) -> Result<(), Arc<StartError>> {
    let row = match store::get_agent(&state.db, agent_id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Err(StartError::plain(format!("no agent with id '{agent_id}'")));
        }
        Err(e) => return Err(StartError::plain(e.to_string())),
    };
    let agent = Agent::from_row(row).map_err(StartError::plain)?;
    // The target itself is discarded: both arms mean "it is reachable now" — a
    // container that is up, or a dev server lmgw never started and does not
    // own. Both callers want the side effect and, when there is not one, the
    // reason.
    target(state, &agent).await.map(|_| ())
}

async fn wait_for(mut rx: watch::Receiver<Option<Outcome>>) -> Option<Outcome> {
    loop {
        {
            let seen = rx.borrow_and_update();
            if let Some(v) = seen.as_ref() {
                return Some(v.clone());
            }
        }
        if rx.changed().await.is_err() {
            return None;
        }
    }
}

/// `podman run -d`, then the health probe. One invocation; the caller has
/// already claimed the slot.
async fn start_once(state: &SharedState, agent: &Agent, cancel: &Cancel) -> Outcome {
    let id = agent.row.id.clone();
    let Some(service) = service_of(agent).cloned() else {
        return Err(StartError::plain(format!(
            "'{id}' declares no run.service, so it has no app to serve"
        )));
    };
    if !agent.row.enabled {
        return Err(StartError::plain(format!(
            "agent '{id}' is disabled; enable it on the catalog before its app can start"
        )));
    }
    let RunSpec::Container {
        image,
        pull,
        entrypoint,
        args,
        ..
    } = &agent.manifest.run
    else {
        return Err(StartError::plain(format!(
            "'{id}' is a {} agent, not a container one",
            agent.manifest.kind()
        )));
    };
    let Some(image) = image.as_deref().filter(|i| !i.trim().is_empty()) else {
        return Err(StartError::plain(format!(
            "'{id}' declares no run.image, so there is nothing to start"
        )));
    };

    let snap = state.snapshot();
    let limits = agent.manifest.limits();
    let prefix = snap.settings.container_prefix.clone();
    let name = container::service_container_name(&prefix, &id);
    let (base, network) = match container::gateway_access(&snap.settings.bind_addr) {
        Ok(v) => v,
        Err(e) => return Err(StartError::plain(e)),
    };
    // The path rules again, at use time (mounts §5.3). A service container
    // binds what the **stored** config says, and the folder may have gone
    // since it was saved: the refusal is the start's failure message, with its
    // code, and podman is never reached.
    let bound = match agent.manifest.declares_mounts() {
        false => Vec::new(),
        true => {
            let mctx = mounts::ctx(state).await;
            match mounts::check_values(
                &id,
                &agent.manifest,
                &agent.config_values(),
                &mctx,
                mounts::Moment::Use,
            ) {
                Ok(b) => b,
                Err(r) => {
                    return Err(StartError::plain(format!("{} ({})", r.message, r.code)));
                }
            }
        }
    };
    let token = match token::ensure(state, agent).await {
        Ok(t) => t,
        Err(e) => return Err(StartError::plain(e)),
    };
    let (root, secrets_on_disk) = container::runs_root(&state.data_dir, &prefix);
    let dir = match RunDir::create_named(&root, &format!("service-{}", slug(&id))) {
        Ok(d) => d,
        Err(e) => return Err(StartError::plain(e)),
    };
    if secrets_on_disk {
        tracing::warn!(
            agent = %id,
            "XDG_RUNTIME_DIR is unset, so this service's secrets are on persistent storage at {}",
            dir.path().display()
        );
    }
    // The same two files a phase run is handed, with `phase: "service"` and no
    // rows: an image reads its config the one way, whichever half of itself is
    // running (§6.2).
    let input = json!({
        "phase": "service",
        "agent": { "id": id, "name": agent.manifest.name },
        "config": container::public_config(agent),
        // The same array a phase run is handed (mounts §5.6): a container reads
        // what it was given the one way, whichever half of itself is running.
        "mounts": container::mounts_document(agent),
        "service": {
            "port": service.port,
            // The public origin, byte for byte the `LMGW_APP_ORIGIN` below
            // (§4.6): an image reads it whichever way it looks.
            "origin": agent_origin_base(&snap.settings, &id),
            "health_path": service.health_path,
        },
    });
    let write = |name: &str, body: String| dir.write(name, &body);
    let input_path = match write("input.json", input.to_string()) {
        Ok(p) => p,
        Err(e) => return Err(StartError::plain(e)),
    };
    let secrets_path = match write(
        "secrets.json",
        container::secrets_document(agent, &token, true).to_string(),
    ) {
        Ok(p) => p,
        Err(e) => return Err(StartError::plain(e)),
    };

    let mut env = container::env_for(&id, &base, "service", None, 0);
    // The origin **without** its trailing slash (§4.6): an app concatenates a
    // path onto this to build an absolute URL — an OAuth redirect URI, a share
    // link — and `dto::AgentService.origin` is the same origin *with* the
    // slash, for an `href`. Both forms are in the design's table and neither is
    // derived from the other here.
    env.push((
        "LMGW_APP_ORIGIN".to_string(),
        agent_origin_base(&snap.settings, &id),
    ));
    // The port the image is expected to listen on, so it does not have to
    // hardcode what the manifest already says.
    env.push(("LMGW_PORT".to_string(), service.port.to_string()));
    let mut mounts = vec![
        Mount::lmgw_file(input_path, "/lmgw/input.json"),
        Mount::lmgw_file(secrets_path, "/lmgw/secrets.json"),
    ];
    mounts.extend(bound.iter().map(Mount::bound));
    // A service has no run log to print into, so the lines §5.6 puts there go
    // to the gateway's own log — same sentences, same order, and the start
    // summary on the App tab lists the mounts beside them.
    let keep_id = agent.manifest.declares_mounts();
    for b in &bound {
        tracing::info!(agent = %id, "{}", container::mount_note(b));
    }
    if keep_id {
        tracing::info!(agent = %id, "{}", container::keep_id_note());
    }
    let spawner = state.agent_spawner();
    // At most two attempts, and the second only for a host port taken between
    // `ephemeral_port` releasing it and podman binding it — the race a model's
    // start retries the same way (per-model containers §10.4).
    let mut retry = PortRetry::default();
    let host_port = loop {
        let host_port = match ephemeral_port() {
            Ok(p) => p,
            Err(e) => {
                return Err(StartError::plain(format!(
                    "lmgw could not get a free host port to publish '{id}' on: {e}"
                )))
            }
        };
        let argv = container::run_argv(&RunSpecArgs {
            name: &name,
            prefix: &prefix,
            agent_id: &id,
            // A service has no run; the label says so in words rather than in
            // a number that would collide with a job id (§6.5).
            run: container::RUN_LABEL_SERVICE,
            image,
            pull: *pull,
            entrypoint: entrypoint.as_deref(),
            args,
            limits: &limits,
            env: &env,
            mounts: &mounts,
            keep_id,
            network: &network,
            service: Some(Published {
                host: host_port,
                container: service.port,
            }),
        });

        tracing::info!(agent = %id, container = %name, port = host_port, "starting the service container");
        // Cancellable from here on (§3.3): a Stop, a delete, a disable, a
        // rotate or a manifest replace arriving mid-start interrupts it rather
        // than being told "nothing was running" while the container comes up
        // behind them. `podman run -d` is short, but a cold `--pull=missing`
        // is not.
        let started = tokio::select! {
            r = spawner.run("podman", &argv) => Some(r),
            _ = cancel.cancelled() => None,
        };
        match started {
            Some(Ok(out)) if out.ok() => break host_port,
            Some(Ok(out)) if retry.again(&out.stderr) => {
                // The failed run leaves its container behind in `created`
                // (§10.4, measured): collected before the run on a fresh port,
                // as a model's start collects it.
                tracing::info!(
                    agent = %id,
                    container = %name,
                    "host port {host_port} was taken before podman bound it; starting again on \
                     a fresh port"
                );
                let rm = ["rm", "-f", name.as_str()].map(String::from);
                let _ = spawner.run("podman", &rm).await;
            }
            Some(Ok(out)) => {
                let log = log_tail(state, &id, &name, LOG_EXCERPT_LINES).await;
                return Err(Arc::new(StartError {
                    reason: format!(
                        "podman run -d for '{id}' failed (exit {}): {}",
                        out.status,
                        out.stderr.trim()
                    ),
                    log,
                    container: Some(name.clone()),
                    cancelled: false,
                }));
            }
            Some(Err(e)) => {
                return Err(StartError::plain(format!(
                    "podman is required for container agents and could not be run: {e}"
                )))
            }
            // Dropping the `run` future kills the `podman` process
            // (`kill_on_drop`), but `podman run -d` may already have created
            // the container, so the collect below runs either way.
            None => {
                return Err(cancelled_start(
                    &spawner,
                    &id,
                    &name,
                    &limits,
                    "the start was cancelled",
                )
                .await)
            }
        }
    };

    match probe(state, host_port, &service, cancel).await {
        Ok(()) => {}
        Err(Probe::Cancelled) => {
            return Err(cancelled_start(
                &spawner,
                &id,
                &name,
                &limits,
                "the start was cancelled while its container was being health-probed",
            )
            .await)
        }
        Err(Probe::Failed(reason)) => {
            let log = log_tail(state, &id, &name, LOG_EXCERPT_LINES).await;
            // Nothing half-started is left behind: a container that came up but
            // never answered is gone before the 503 is written.
            container::stop_service_container(
                &spawner,
                &name,
                limits.stop_grace_seconds,
                &|line| tracing::info!(agent = %id, "{line}"),
            )
            .await;
            return Err(Arc::new(StartError {
                reason,
                log,
                container: Some(name.clone()),
                cancelled: false,
            }));
        }
    }

    Ok(Arc::new(Live {
        agent_id: id,
        container: name,
        host_port,
        started_at: Instant::now(),
        started_at_utc: chrono::Utc::now(),
        idle_seconds: service.idle_seconds,
        stop_grace_seconds: limits.stop_grace_seconds,
        last_used: Mutex::new(Instant::now()),
        in_flight: Arc::new(AtomicUsize::new(0)),
        _dir: dir,
    }))
}

/// Collect the container a cancelled start had already created, and phrase the
/// refusal its waiters get.
async fn cancelled_start(
    spawner: &Arc<dyn container::Spawner>,
    id: &str,
    name: &str,
    limits: &crate::agents::manifest::Limits,
    reason: &str,
) -> Arc<StartError> {
    container::stop_service_container(
        spawner,
        name,
        limits.stop_grace_seconds,
        &|line| tracing::info!(agent = %id, "{line}"),
    )
    .await;
    Arc::new(StartError {
        reason: format!("{reason}; the next request to this agent's app starts it again"),
        log: String::new(),
        container: Some(name.to_string()),
        cancelled: true,
    })
}

/// Why a probe stopped: the container never answered, or someone stopped it.
enum Probe {
    Failed(String),
    Cancelled,
}

/// Wait for the container to answer on its published port.
///
/// Bounded by the visible `service.start_timeout_seconds`; **`0` waits as long
/// as the container takes**, which is the same "`0` means no limit" the rest of
/// the runtime means and is spelled out in the manifest's own description.
///
/// Two probes, chosen by the manifest: a `health_path` (the default, `/`) is an
/// HTTP GET, and a **blank** one is a TCP connect — for an image that speaks
/// something HTTP cannot introduce itself to. An HTTP status below 500 counts
/// as up: the question is "is this process listening and speaking HTTP", and
/// an app whose `/` answers `302 → /login` or `404` is up by any reading that
/// matters (a deliberate widening of §3.3's "a 200", recorded there).
async fn probe(
    state: &SharedState,
    host_port: u16,
    service: &Service,
    cancel: &Cancel,
) -> Result<(), Probe> {
    let path = service.health_path.trim().to_string();
    let bounded = service.start_timeout_seconds > 0;
    let deadline = Instant::now() + Duration::from_secs(service.start_timeout_seconds.max(1));
    let what = if path.is_empty() {
        format!("a TCP connect to 127.0.0.1:{host_port}")
    } else {
        format!("http://127.0.0.1:{host_port}{path}")
    };
    let expired = || {
        Probe::Failed(format!(
            "the container did not answer {what} within run.service.start_timeout_seconds ({}s)",
            service.start_timeout_seconds
        ))
    };
    // `start_timeout_seconds = 0` is unbounded and therefore has no per-probe
    // timeout either — but **Stop still works**, because every await in this
    // loop races the cancel. An unbounded wait that could not be interrupted
    // would be a wedge, not a setting.
    let body = async {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if bounded && left.is_zero() {
                return Err(expired());
            }
            let ok = if path.is_empty() {
                tokio::net::TcpStream::connect(("127.0.0.1", host_port))
                    .await
                    .is_ok()
            } else {
                // The remaining budget *is* the per-probe timeout, so a hung
                // probe costs exactly the wall clock the start was allowed
                // anyway and there is no second, invented bound to explain (the
                // reasoning `Registry::wait_healthy` writes out). Unbounded
                // means no per-probe timeout at all; the cancel is the way out.
                let url = format!("http://127.0.0.1:{host_port}{path}");
                let mut req = state.proxy_http.get(&url);
                if bounded {
                    req = req.timeout(left);
                }
                // The **proxy's** client, not the shared one: a health path
                // that answers `302` must read as "up", not be followed to
                // wherever it points (§3.3).
                matches!(req.send().await, Ok(r) if r.status().as_u16() < 500)
            };
            if ok {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if bounded && left.is_zero() {
                return Err(expired());
            }
            let nap = if bounded {
                HEALTH_POLL.min(left)
            } else {
                HEALTH_POLL
            };
            tokio::time::sleep(nap).await;
        }
    };
    tokio::select! {
        outcome = body => outcome,
        _ = cancel.cancelled() => Err(Probe::Cancelled),
    }
}

// ---------------------------------------------------------------------------
// Stop and the idle sweep (§3.3)
// ---------------------------------------------------------------------------

/// What a [`stop`] did, in the words its caller prints.
#[derive(Debug, Clone, PartialEq)]
pub struct Stopped {
    /// The container that was stopped or collected, when there was one.
    pub container: Option<String>,
    /// A start was in flight and was interrupted, rather than a running
    /// container being stopped.
    pub cancelled_start: bool,
}

impl Stopped {
    pub fn describe(&self) -> String {
        match (&self.container, self.cancelled_start) {
            (Some(c), true) => format!("a start was cancelled and {c} collected"),
            (None, true) => "a start was cancelled".to_string(),
            (Some(c), false) => format!("stopped {c}"),
            (None, false) => "nothing was running".to_string(),
        }
    }
}

/// Stop this agent's service container now, whatever its idle window says.
///
/// `None` when there was nothing running and nothing starting — "make sure
/// this is not running" is what every caller means, so an absent container is
/// success, not an error (the reading `Registry::stop` already makes).
///
/// **A start in flight is stopped too** (§3.3): it is cancelled, its container
/// collected by the starter, and the waiters parked on it get the refusal that
/// says so. Without that, the five paths that stop a container — the App tab's
/// Stop, delete, disable, token rotation and a manifest replace — all answered
/// "nothing was running" whenever they arrived a second too early, and the
/// container came up behind them with nothing tracking it.
pub async fn stop(state: &SharedState, agent_id: &str, why: &str) -> Option<Stopped> {
    stop_inner(state, agent_id, why, None).await
}

/// [`stop`], with the idle sweep's extra condition re-checked **under the map
/// lock**.
///
/// The sweep decides on a snapshot of `in_flight` and `last_used` taken outside
/// the lock; a request that claimed its guard in between would otherwise be
/// torn down by a decision made before it existed. `require_idle` re-asks at
/// the moment the entry is claimed, which is the only moment that counts.
async fn stop_inner(
    state: &SharedState,
    agent_id: &str,
    why: &str,
    require_idle: Option<Duration>,
) -> Option<Stopped> {
    enum Claim {
        Ready(Arc<Live>, StopGuard),
        Cancel(Cancel, watch::Receiver<Option<Outcome>>),
    }
    // The slot is **replaced**, not removed: a request that arrives while the
    // ladder is climbing must wait for it rather than start a container with
    // the same name into the `podman rm -f` that is about to run.
    let claim = {
        let mut slots = state.agent_services.slots.lock().unwrap();
        match slots.get(agent_id) {
            Some(Slot::Ready(l)) => {
                if let Some(window) = require_idle {
                    if l.in_flight() > 0 || l.idle_for() < window {
                        return None;
                    }
                }
                let (tx, _rx) = watch::channel(());
                match slots.insert(agent_id.to_string(), Slot::Stopping(tx)) {
                    Some(Slot::Ready(l)) => Some(Claim::Ready(
                        l,
                        StopGuard(state.clone(), agent_id.to_string()),
                    )),
                    // Unreachable: matched as Ready under this same lock.
                    _ => None,
                }
            }
            // A start in flight is never "idle": the sweep leaves it alone and
            // only an explicit stop cancels it.
            Some(Slot::Starting(st)) if require_idle.is_none() => {
                Some(Claim::Cancel(st.cancel.clone(), st.answer.subscribe()))
            }
            _ => None,
        }
    }?;

    match claim {
        Claim::Cancel(cancel, rx) => {
            tracing::info!(agent = %agent_id, "cancelling the start in flight: {why}");
            cancel.fire();
            // The starter owns the cleanup — it is the only thing that knows
            // whether `podman run` got as far as creating a container — so this
            // waits for its answer rather than guessing at a name.
            let container = match wait_for(rx).await {
                Some(Err(e)) => e.container.clone(),
                _ => None,
            };
            drop_conn(state, agent_id).await;
            Some(Stopped {
                container,
                cancelled_start: true,
            })
        }
        Claim::Ready(live, _guard) => {
            tracing::info!(agent = %agent_id, container = %live.container, "stopping the service container: {why}");
            let spawner = state.agent_spawner();
            let id = agent_id.to_string();
            container::stop_service_container(
                &spawner,
                &live.container,
                live.stop_grace_seconds,
                &|line| tracing::info!(agent = %id, "{line}"),
            )
            .await;
            // The `agent:<id>` MCP connection is holding an `mcp-session-id`
            // that belonged to the container that just went (§3.3). Reusing it
            // against the next one would be a session the new container has
            // never heard of, so the conn is torn down here — every stop path
            // comes through this function, including the idle sweep and the
            // proxy's 502 eviction.
            drop_conn(state, agent_id).await;
            Some(Stopped {
                container: Some(live.container.clone()),
                cancelled_start: false,
            })
        }
    }
}

/// Tear down the live MCP connection to this agent's own row, if there is one.
async fn drop_conn(state: &SharedState, agent_id: &str) {
    let id = state
        .snapshot()
        .mcp_servers
        .values()
        .find(|s| s.agent_id.as_deref() == Some(agent_id))
        .map(|s| s.id);
    if let Some(id) = id {
        state.mcp.stop_server(id).await;
    }
}

/// Drops the `Stopping` slot when the stop returns, however it returns.
///
/// A panic or an early return between the slot being claimed and the ladder
/// finishing would otherwise leave every later request parked on a stop that
/// is never coming.
struct StopGuard(SharedState, String);

impl Drop for StopGuard {
    fn drop(&mut self) {
        let mut slots = self.0.agent_services.slots.lock().unwrap();
        if matches!(slots.get(&self.1), Some(Slot::Stopping(_))) {
            slots.remove(&self.1);
        }
    }
}

/// Stop **every** app container this process is holding, for a reason that
/// applies to all of them at once.
///
/// Today there is one: a changed `agent_origin_suffix` (origins §4.10). Origins
/// are computed per request, so nothing needs resyncing — but a running
/// container is holding the old `LMGW_APP_ORIGIN` in its environment, and an
/// app that builds an absolute URL from it would keep writing the address the
/// owner has just renamed away.
///
/// Every entry, not only the `Ready` ones: a start in flight would come up
/// holding the same stale environment, and [`stop`] cancels it the way every
/// other explicit stop does. Returns what was stopped, for the settings note.
pub async fn stop_all(state: &SharedState, why: &str) -> Vec<String> {
    let mut stopped = Vec::new();
    let mut ids = state.agent_services.keys();
    ids.sort();
    for id in ids {
        if let Some(s) = stop(state, &id, why).await {
            stopped.extend(s.container);
        }
    }
    stopped
}

/// Stop every service whose idle window has passed with nothing in flight
/// (§3.3).
///
/// Mirrors [`McpManager::reap_idle`](crate::mcp::McpManager::reap_idle) exactly,
/// including the two rules that matter: **`idle_seconds <= 0` is a true no-op
/// warm-keep**, and a service with a request in flight is skipped however stale
/// `last_used` looks — `last_used` only advances when a request finishes, so a
/// streaming response would otherwise be torn down mid-flight.
///
/// Rides the 5 s status tick the MCP reaper already rides (`server::
/// spawn_background_tasks`), so a stop lands within one tick of the window
/// passing rather than on a cadence invented for this.
pub async fn sweep_idle(state: &SharedState) -> Vec<String> {
    let mut stopped = Vec::new();
    for live in state.agent_services.live() {
        if live.idle_seconds <= 0 {
            continue;
        }
        let window = Duration::from_secs(live.idle_seconds as u64);
        if live.in_flight() > 0 || live.idle_for() < window {
            continue;
        }
        let why = format!(
            "idle for more than service.idle_seconds ({}s)",
            live.idle_seconds
        );
        // The two conditions are re-asked under the map lock: a request that
        // claimed its guard between the read above and the claim below must not
        // be torn down by a decision made before it existed.
        if let Some(s) = stop_inner(state, &live.agent_id, &why, Some(window)).await {
            stopped.extend(s.container);
        }
    }
    stopped
}

// ---------------------------------------------------------------------------
// `provides.mcp` — the agent's own MCP row (§3.3)
// ---------------------------------------------------------------------------

/// The reserved `mcp_servers.name` prefix an agent's own registration lives
/// under. `mcp_server_set` refuses it for an owner-created row, because the
/// agent lifecycle adopts — and deletes — anything named this way.
pub const MCP_NAME_PREFIX: &str = "agent:";

pub fn mcp_row_name(agent_id: &str) -> String {
    format!("{MCP_NAME_PREFIX}{agent_id}")
}

/// The URL the `agent:<id>` row points at: **lmgw's own proxy**, not the
/// container's host port.
///
/// The host port is ephemeral per start and an `mcp_servers` row is persistent,
/// so a row carrying the port would be stale the moment the service restarted —
/// and would have to be rewritten (and the registry reloaded) on every start.
/// `/agents/<id>/mcp` is stable, shares the on-demand start and the idle stop
/// with the app proxy, and is what makes `ops::reject_self_loop`'s narrowing
/// necessary. The host part comes from [`net::primary_base_url`](crate::net::primary_base_url),
/// which already resolves a wildcard bind to loopback.
pub fn mcp_url(bind_addr: &str, agent_id: &str) -> String {
    format!(
        "{}/agents/{agent_id}/mcp",
        crate::net::primary_base_url(bind_addr)
    )
}

/// Is `url` **this** gateway's own `/agents/<agent_id>/mcp` — the one address
/// an `agent:<id>` row is allowed to name?
///
/// [`mcp_url`] read backwards, and the backstop under the refusal
/// `ops::mcp_server_set` answers a `url` on an agent's row with (principals
/// §10 Part 1). The dial of such a row carries the `owner:dashboard` bearer,
/// which is the door key to everything: it must leave this process only
/// towards this process. A row that says otherwise — hand-edited in sqlite3,
/// or written by some later op that forgets the refusal — is dialled, but
/// without the key.
///
/// Host and port are compared against every address this bind is actually
/// reachable at ([`net::reachable_urls`](crate::net::reachable_urls), which
/// expands a wildcard), plus the loopback spellings that resolve to the same
/// socket under another name; the path is compared exactly, and `https` is not
/// a scheme the listener speaks.
pub fn is_own_mcp_url(bind_addr: &str, agent_id: &str, url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "http" || parsed.path() != format!("/agents/{agent_id}/mcp") {
        return false;
    }
    let (Some(host), Some(port)) = (parsed.host_str(), parsed.port_or_known_default()) else {
        return false;
    };
    let loopback = |h: &str| matches!(h, "localhost" | "127.0.0.1" | "::1");
    crate::net::reachable_urls(bind_addr).iter().any(|r| {
        let Ok(own) = reqwest::Url::parse(&r.url) else {
            return false;
        };
        own.port_or_known_default() == Some(port)
            && match own.host_str() {
                Some(h) => h == host || (loopback(h) && loopback(host)),
                None => false,
            }
    })
}

/// The `agent:<id>` row a manifest declaring `provides.mcp` gets **when it is
/// first created**.
///
/// Every value that is not derived from the manifest is the same default the
/// MCP page gives a hand-made row: [`ops::DEFAULT_MCP_TIMEOUT_MS`] is the one
/// constant both creation paths read, never a second number invented here.
fn new_row(state: &SharedState, agent: &Agent) -> Option<NewMcpServer> {
    provides_mcp(agent)?;
    let service = service_of(agent)?;
    let snap = state.snapshot();
    Some(NewMcpServer {
        name: mcp_row_name(&agent.row.id),
        enabled: agent.row.enabled,
        transport: McpTransport::Http,
        command: None,
        args: Vec::new(),
        env: Vec::new(),
        cwd: None,
        container_image: None,
        extra_run_args: Vec::new(),
        url: Some(mcp_url(&snap.settings.bind_addr, &agent.row.id)),
        headers: Vec::new(),
        // Already `[a-z0-9-]` by `manifest::validate_id`, so
        // `validate_tool_prefix` passes; a reserved id (`lmgw`, `docs`) is
        // refused by the manifest before it reaches here.
        tool_prefix: agent.row.id.clone(),
        timeout_ms: crate::ops::DEFAULT_MCP_TIMEOUT_MS,
        // Never autostart: the proxy starts the container when something asks
        // for a tool, which is the whole point of layer 3.
        autostart: false,
        // Inherited at creation, so "how long is this agent kept warm" starts
        // as one number; the owner may then tune the connection's window on the
        // MCP page independently of the container's.
        idle_seconds: service.idle_seconds,
        allow_sampling: false,
        sampling_alias: None,
        agent_id: Some(agent.row.id.clone()),
    })
}

/// An existing row, moved back onto the three fields lmgw owns and **nothing
/// else**.
///
/// A manifest write must not revert what the owner set on the MCP page: only
/// `url` (the bind address may have moved), `tool_prefix` (the agent id) and
/// `agent_id` are lmgw's to keep true. `enabled` is deliberately not among
/// them — the catalog's disable is enforced where it cannot be worked around,
/// by [`ensure`] refusing to start a disabled agent's container, rather than by
/// a flag the owner is also allowed to hold.
fn updated_row(state: &SharedState, agent: &Agent, cur: &McpServer) -> NewMcpServer {
    NewMcpServer {
        name: cur.name.clone(),
        enabled: cur.enabled,
        transport: cur.transport,
        command: cur.command.clone(),
        args: cur.args.clone(),
        env: cur.env.clone(),
        cwd: cur.cwd.clone(),
        container_image: cur.container_image.clone(),
        extra_run_args: cur.extra_run_args.clone(),
        url: Some(mcp_url(&state.snapshot().settings.bind_addr, &agent.row.id)),
        headers: cur.headers.clone(),
        tool_prefix: agent.row.id.clone(),
        timeout_ms: cur.timeout_ms,
        autostart: cur.autostart,
        idle_seconds: cur.idle_seconds,
        allow_sampling: cur.allow_sampling,
        sampling_alias: cur.sampling_alias.clone(),
        agent_id: Some(agent.row.id.clone()),
    }
}

/// The row this agent owns, if there is one.
async fn existing_row(state: &SharedState, agent_id: &str) -> Option<McpServer> {
    store::list_mcp_servers(&state.db)
        .await
        .ok()?
        .into_iter()
        .find(|s| s.agent_id.as_deref() == Some(agent_id) || s.name == mcp_row_name(agent_id))
}

/// Create, update or remove the `agent:<id>` row so it matches the manifest
/// (§3.3). Called after every write to the catalog row.
///
/// Errors are returned, never swallowed: an agent whose tools did not register
/// is an agent whose chat threads will silently see nothing.
pub async fn sync_mcp_registration(state: &SharedState, agent: &Agent) -> Result<(), String> {
    let existing = existing_row(state, &agent.row.id).await;
    let wanted = provides_mcp(agent).is_some() && service_of(agent).is_some();
    match (wanted, existing) {
        (false, None) => Ok(()),
        // `provides.mcp` was taken out of the manifest: the row goes with it.
        (false, Some(row)) => {
            store::delete_mcp_server(&state.db, row.id)
                .await
                .map_err(|e| e.to_string())?;
            reload(state).await
        }
        (true, None) => {
            let Some(new) = new_row(state, agent) else {
                return Ok(());
            };
            // The agent's id is its tools' prefix: never a device's hosting
            // label, as `mcp_server_set` refuses for a hand-made row
            // (client-apps design §1.1, review W2-8).
            crate::ops::reject_device_label(&state.snapshot(), &new.tool_prefix).map_err(
                |why| {
                    format!(
                        "agent '{}': its tools cannot register — {why}",
                        agent.row.id
                    )
                },
            )?;
            store::insert_mcp_server(&state.db, &new)
                .await
                .map_err(|e| e.to_string())?;
            reload(state).await
        }
        (true, Some(cur)) => {
            let new = updated_row(state, agent, &cur);
            // Nothing lmgw owns has moved: do not write, and above all do not
            // reload the snapshot, which would reconcile the live MCP
            // connection on every manifest save for no reader.
            if new.url == cur.url && new.tool_prefix == cur.tool_prefix {
                return Ok(());
            }
            store::update_mcp_server(&state.db, cur.id, &new)
                .await
                .map_err(|e| e.to_string())?;
            reload(state).await
        }
    }
}

/// Re-point every agent-owned row at the current `bind_addr` (§3.3).
///
/// The row's URL is built from `net::primary_base_url(bind_addr)` and is
/// written when the *manifest* is written, so a bind address that moves
/// afterwards leaves every agent row pointing at a port nothing is listening
/// on — across restarts, because nothing re-derives it. Called at boot and
/// whenever a settings write changes `bind_addr`.
pub async fn resync_all(state: &SharedState) {
    let rows = match store::list_mcp_servers(&state.db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("agent MCP rows could not be re-pointed at the bind address: {e}");
            return;
        }
    };
    for id in rows.iter().filter_map(|r| r.agent_id.clone()) {
        if let Err(e) = resync(state, &id).await {
            tracing::warn!("agent '{id}': its own MCP registration could not be updated: {e}");
        }
    }
}

/// [`sync_mcp_registration`] by id, for the call sites that have one and not a
/// parsed agent — the sibling of [`token::resync`], and called beside it
/// everywhere a catalog row is written.
///
/// A row this build cannot parse loses its registration rather than keeping a
/// stale one: an unreadable manifest cannot be asked what it provides, and a
/// row pointing at tools nobody can describe is worse than no row.
pub async fn resync(state: &SharedState, agent_id: &str) -> Result<(), String> {
    let Some(row) = store::get_agent(&state.db, agent_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return drop_mcp_registration(state, agent_id).await;
    };
    match Agent::from_row(row) {
        Ok(agent) => sync_mcp_registration(state, &agent).await,
        Err(_) => drop_mcp_registration(state, agent_id).await,
    }
}

/// Remove the `agent:<id>` row, if any — `agent_delete`'s half of the pair.
pub async fn drop_mcp_registration(state: &SharedState, agent_id: &str) -> Result<(), String> {
    let Some(row) = existing_row(state, agent_id).await else {
        return Ok(());
    };
    store::delete_mcp_server(&state.db, row.id)
        .await
        .map_err(|e| e.to_string())?;
    reload(state).await
}

/// One snapshot reload — which is also what reconciles the live MCP connection
/// and fires `tools/list_changed` at every `/mcp` subscriber, exactly as
/// `mcp_server_set` relies on.
async fn reload(state: &SharedState) -> Result<(), String> {
    state
        .reload_snapshot()
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
