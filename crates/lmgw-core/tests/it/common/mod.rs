//! Shared plumbing for the suites that drive the real router over HTTP.
//!
//! One thing lives here rather than in each file. Since the principals work
//! (principals design §3.2) every `/api` route needs an `Admin` principal,
//! which in a browser is the `owner:dashboard` key riding in the session
//! cookie. A test has no cookie jar, so it presents the same key as a bearer —
//! and [`Gw`] is what carries it from the gateway that minted it to the
//! request that needs it.
//!
//! A suite that wants the *refusal* asks for [`Gw::anon`] and gets a client
//! with no credential at all, which is what a foreign process is.
//!
//! [`Gw::origin_client`] is the other plane: an agent's UI answers on a host
//! name of its own (origins §4.1) and asks for no credential, so that client
//! resolves the name to this gateway's socket and presents nothing.

// Declared once, as `mod common;` in `tests/it/main.rs`, and `#[path]`-included
// by `tests/trace_span.rs`; each binary uses the part it needs, so the rest is
// dead code from that binary's point of view.
#![allow(dead_code)]

use std::fmt;

use lmgw_core::agents::token::OWNER_DASHBOARD;
use lmgw_core::server::build_router;
use lmgw_core::state::SharedState;

/// A running test gateway: its base URL, and the credential its `Admin`
/// routes want.
///
/// Formats as its base URL, so `format!("{gw}/api/status")` reads exactly as
/// it did when `serve` returned a `String`.
#[derive(Clone)]
pub struct Gw {
    pub base: String,
    /// The plaintext of this gateway's `owner:dashboard` row. Per gateway,
    /// because every `init_for_tests` mints its own.
    pub key: String,
}

impl fmt::Display for Gw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base)
    }
}

impl Gw {
    /// A client that presents the dashboard session on every request — what a
    /// browser with the cookie is, for a test that has no cookie jar.
    pub fn client(&self) -> reqwest::Client {
        self.builder()
            .build()
            .expect("a client with default headers always builds")
    }

    /// The same credential, but the client never follows a redirect — for the
    /// tests that assert the `Location` header rather than where it leads.
    pub fn client_no_redirect(&self) -> reqwest::Client {
        self.builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("a client with no redirect policy always builds")
    }

    fn builder(&self) -> reqwest::ClientBuilder {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", self.key)
                .parse()
                .expect("a minted owner key is header-safe"),
        );
        reqwest::Client::builder().default_headers(headers)
    }

    /// A client with no credential at all, for the tests that assert what a
    /// route says to a caller who has none.
    pub fn anon(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    /// An agent origin on this gateway (origins §4.1): its own port, addressed
    /// by the agent's host name — `board.localhost` under the default suffix.
    pub fn origin(&self, host: &str) -> String {
        format!("http://{host}:{}", self.addr().port())
    }

    /// A client that reaches those host names on this gateway's socket, and
    /// carries **no credential** — which is what a browser has on an agent
    /// origin: the origin is the container's namespace and asks for none.
    ///
    /// The names are resolved by the client rather than by the box. Every
    /// browser maps `*.localhost` to loopback internally and systemd-resolved
    /// does the same here, but a test that failed on a machine without it
    /// would be testing the resolver instead of the dispatch.
    pub fn origin_client(&self, hosts: &[&str]) -> reqwest::Client {
        self.origin_builder(hosts)
            .build()
            .expect("a client with DNS overrides always builds")
    }

    /// The same, for the tests that assert a `Location` rather than where it
    /// leads.
    pub fn origin_client_no_redirect(&self, hosts: &[&str]) -> reqwest::Client {
        self.origin_builder(hosts)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("a client with DNS overrides always builds")
    }

    fn origin_builder(&self, hosts: &[&str]) -> reqwest::ClientBuilder {
        let addr = self.addr();
        hosts
            .iter()
            .fold(reqwest::Client::builder(), |b, host| b.resolve(host, addr))
    }

    /// The socket this gateway is listening on, read back out of its base URL.
    pub fn addr(&self) -> std::net::SocketAddr {
        self.base
            .trim_start_matches("http://")
            .parse()
            .expect("a test gateway's base is http://<ip>:<port>")
    }
}

/// One gateway's `owner:dashboard` plaintext.
///
/// Its own function because several suites bind the listener *before* they
/// build the router — a container is told where the gateway is from
/// `bind_addr` alone, so the port has to be known before the settings are
/// written — and cannot go through [`serve`].
pub fn dashboard_key(state: &SharedState) -> String {
    let snap = state.snapshot();
    snap.owner_key(OWNER_DASHBOARD)
        .expect("AppState::init_for_tests seeds the dashboard key, as init does")
        .to_string()
}

/// Serve `state` on an ephemeral port, and read the dashboard key out of the
/// snapshot it was seeded with.
pub async fn serve(state: SharedState) -> Gw {
    let key = dashboard_key(&state);
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { serve_on(listener, app).await });
    Gw { base, key }
}

/// Serve `app` on `listener` the way `server::run` does: with the TCP peer's
/// address on every request, which the agent proxy forwards as
/// `X-Forwarded-For` (origins §4.6). For the suites that bind their own
/// listener before building the router.
pub async fn serve_on(listener: tokio::net::TcpListener, app: axum::Router) {
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .unwrap()
}

/// Guards the handful of suites that mutate a process-wide environment
/// variable (`HF_ENDPOINT`, `LMGW_AUDIO_CATALOG_ENDPOINT`) to point the code
/// under test at a mock server. Each used to be its own test binary, so
/// owning the process for the duration of the mutation was safe; folded into
/// one `tests/it` binary, two of them running at once would stomp on each
/// other's endpoint — and a suite that expects the *unset* default (a live
/// suite hitting the real hub) could just as easily start mid-mutation.
///
/// Every such test fn — the ones that set the var and the ones that rely on
/// it being unset — takes this guard as its first statement and holds it for
/// its whole body, so at most one of them ever touches the variable at a
/// time. Dropping the guard puts both variables back as it found them, on a
/// panic too, so a failing suite cannot leave a dead mock endpoint behind for
/// the rest of the run.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The variables [`process_env_lock`] guards and restores.
const GUARDED_ENV: [&str; 2] = ["HF_ENDPOINT", "LMGW_AUDIO_CATALOG_ENDPOINT"];

pub struct EnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

pub async fn process_env_lock() -> EnvGuard {
    let lock = ENV.lock().await;
    EnvGuard {
        saved: GUARDED_ENV
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect(),
        _lock: lock,
    }
}
