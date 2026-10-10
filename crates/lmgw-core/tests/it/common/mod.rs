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

/// A loopback port nothing answers on, for as long as the socket lives:
/// bound and never listened on, so a connect is refused, and no other socket
/// — another test's server, under a parallel suite — can be given the port
/// meanwhile. A port bound and closed again was free for whoever bound next.
pub fn refusing_port() -> (u16, tokio::net::TcpSocket) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind(([127, 0, 0, 1], 0).into()).unwrap();
    (socket.local_addr().unwrap().port(), socket)
}

/// `live` read into `T` and written back is `live`: a field the gateway adds
/// to an answer without adding it to the documented type is dropped by the
/// read and fails here. The one drift guard the typed-answer tests share.
///
/// It also holds the document to what the gateway sends: every field the
/// admin document lists as `required` for `T` is in `live`, so a field the
/// server leaves out is never promised to a client generator.
pub fn round_trips<T: serde::de::DeserializeOwned + serde::Serialize + schemars::JsonSchema>(
    what: &str,
    live: &serde_json::Value,
) -> T {
    let typed: T =
        serde_json::from_value(live.clone()).unwrap_or_else(|e| panic!("{what}: {e}\n{live}"));
    assert_eq!(
        &serde_json::to_value(&typed).unwrap(),
        live,
        "{what}: the type drops or invents a field"
    );
    let name = T::schema_name();
    let doc = lmgw_core::openapi::admin_doc();
    let schemas = documented_schemas(doc, &name);
    assert!(
        !schemas.is_empty(),
        "{what}: the document has neither a component nor an answer schema titled {name}"
    );
    for schema in &schemas {
        if let (Some(required), Some(sent)) = (
            schema.get("required").and_then(serde_json::Value::as_array),
            live.as_object(),
        ) {
            for field in required.iter().filter_map(serde_json::Value::as_str) {
                assert!(
                    sent.contains_key(field),
                    "{what}: the document lists {name}.{field} as required, the answer has none\n{live}"
                );
            }
        }
        validates_against(what, doc, schema, live);
    }
    typed
}

/// The document's schemas for the type titled `name`: its component when it
/// has one, else every answer or event-frame schema a route carries inline
/// (a `root_schema_for` answer keeps the type's name as its `title`).
pub fn documented_schemas(doc: &serde_json::Value, name: &str) -> Vec<serde_json::Value> {
    if let Some(c) = doc.pointer(&format!("/components/schemas/{name}")) {
        return vec![c.clone()];
    }
    let mut found = Vec::new();
    let Some(paths) = doc.get("paths").and_then(serde_json::Value::as_object) else {
        return found;
    };
    for op in paths
        .values()
        .filter_map(serde_json::Value::as_object)
        .flat_map(|methods| methods.values())
    {
        let Some(responses) = op.get("responses").and_then(serde_json::Value::as_object) else {
            continue;
        };
        for resp in responses.values() {
            let bodies = resp
                .get("content")
                .and_then(serde_json::Value::as_object)
                .into_iter()
                .flat_map(|c| c.values().filter_map(|m| m.get("schema")));
            let frames = resp
                .get("x-lmgw-sse-events")
                .and_then(serde_json::Value::as_object)
                .into_iter()
                .flat_map(|e| e.values());
            for schema in bodies.chain(frames) {
                if schema.get("title").and_then(serde_json::Value::as_str) == Some(name)
                    && !found.contains(schema)
                {
                    found.push(schema.clone());
                }
            }
        }
    }
    found
}

/// `instance` validated against the named component of the built document;
/// a missing component is a failure.
pub fn validates_against_component_of(what: &str, name: &str, instance: &serde_json::Value) {
    let doc = lmgw_core::openapi::admin_doc();
    assert!(
        doc.pointer(&format!("/components/schemas/{name}"))
            .is_some(),
        "{what}: the document has no component {name}"
    );
    validates_against(
        what,
        doc,
        &serde_json::json!({"$ref": format!("#/components/schemas/{name}")}),
        instance,
    );
}

/// `instance` validated against `schema`, whose `$ref`s resolve in `doc`'s
/// `components` (the synthetic-root trick of `openapi_live.rs`'s
/// `validator_root`).
pub fn validates_against(
    what: &str,
    doc: &serde_json::Value,
    schema: &serde_json::Value,
    instance: &serde_json::Value,
) {
    let mut root = schema.clone();
    if let serde_json::Value::Object(map) = &mut root {
        map.insert("components".to_string(), doc["components"].clone());
    }
    if let Err(e) = jsonschema::validate(&root, instance) {
        panic!("{what}: the answer does not validate against the built document: {e}\n{instance}");
    }
}

/// A `container` op answer as the JSON it goes out as, read through the
/// documented type on the way: a field the gateway adds without adding it to
/// the type fails here.
pub fn container_wire(answer: impl serde::Serialize) -> serde_json::Value {
    let live = serde_json::to_value(answer).unwrap();
    round_trips::<lmgw_api_types::ContainerAnswer>("container", &live);
    live
}

/// The same for the load test's answer.
pub fn model_test_wire(answer: impl serde::Serialize) -> serde_json::Value {
    let live = serde_json::to_value(answer).unwrap();
    round_trips::<lmgw_api_types::ModelTest>("local_model_test", &live);
    live
}

/// `local_model_get`'s answer as JSON, read through its documented type.
pub fn read_wire(answer: impl serde::Serialize) -> serde_json::Value {
    let live = serde_json::to_value(answer).unwrap();
    round_trips::<lmgw_api_types::LocalModelRead>("local_model_get", &live);
    live
}

/// `local_model_check`'s answer as JSON, read through its documented type.
pub fn check_wire(answer: impl serde::Serialize) -> serde_json::Value {
    let live = serde_json::to_value(answer).unwrap();
    round_trips::<lmgw_api_types::LocalModelCheck>("local_model_check", &live);
    live
}

/// `model_inspect`'s answer as JSON, read through its documented type.
pub fn inspect_wire(answer: impl serde::Serialize) -> serde_json::Value {
    let live = serde_json::to_value(answer).unwrap();
    round_trips::<lmgw_api_types::ModelInspect>("model_inspect", &live);
    live
}

pub mod captured_log;
pub mod patience;
