//! Proxy mode (headless server design §4.5): a trusted peer's forwarded
//! headers decide the scheme, the host and the client address that the
//! cookie, the same-origin rule, `/mcp`'s guard and the agent proxy see;
//! from any other peer they change nothing. Every client here is on
//! loopback, so "trusted" is `127.0.0.1/32` and "untrusted" is a list that
//! does not hold it.

use std::net::SocketAddr;

/// The agent proxy behind a trusted proxy (D33, D49).
// Starts an agent (crate::agents_service's), which is not on Windows
// (Windows design §4.5).
#[cfg(unix)]
mod agent_proxy;
/// Proxy mode on the TLS listener (D51).
mod tls;

use axum::extract::ConnectInfo;
use axum::http::HeaderMap;
use lmgw_core::agents::token::OWNER_DASHBOARD;
use lmgw_core::config::Settings;
use lmgw_core::server::{forwarded, Conn};
use lmgw_core::state::{AppState, SharedState};
use serde_json::{json, Value};

/// A gateway serving its plain listener on a loopback port, `edit`ed
/// first.
struct Gw {
    state: SharedState,
    base: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    done: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Gw {
    async fn start(edit: impl FnOnce(&mut Settings)) -> Self {
        let state = AppState::init_for_tests().await.unwrap();
        crate::main_tls::set(&state, |s| {
            s.realtime.webrtc.webrtc_enabled = false;
            edit(s);
        })
        .await;
        let listener = lmgw_core::server::bind(&state, "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let done = tokio::spawn(lmgw_core::server::serve(
            state.clone(),
            listener,
            async {
                let _ = stopped.await;
            },
            None,
        ));
        Self {
            state,
            base,
            stop: Some(stop),
            done,
        }
    }

    fn key(&self) -> String {
        self.state
            .snapshot()
            .owner_key(OWNER_DASHBOARD)
            .unwrap()
            .to_string()
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        let _ = self.done.await;
    }
}

/// The headers a TLS-terminating proxy for `https://lmgw.example` sends.
fn proxied(client: &str) -> Vec<(&'static str, String)> {
    vec![
        ("x-forwarded-for", format!("127.0.0.1, {client}")),
        ("x-forwarded-proto", "https".into()),
        ("x-forwarded-host", "lmgw.example".into()),
    ]
}

fn with(
    rb: reqwest::RequestBuilder,
    headers: &[(&'static str, String)],
) -> reqwest::RequestBuilder {
    headers.iter().fold(rb, |rb, (k, v)| rb.header(*k, v))
}

/// Sign in with the owner key as a browser on `origin` behind `headers`:
/// the status and the `Set-Cookie`.
async fn sign_in(gw: &Gw, origin: &str, headers: &[(&'static str, String)]) -> (u16, String) {
    let r = with(
        reqwest::Client::new().post(format!("{}/api/session", gw.base)),
        headers,
    )
    .header("origin", origin)
    .json(&json!({ "token": gw.key() }))
    .send()
    .await
    .unwrap();
    let cookie = r
        .headers()
        .get("set-cookie")
        .map(|c| c.to_str().unwrap().to_string())
        .unwrap_or_default();
    (r.status().as_u16(), cookie)
}

#[tokio::test]
async fn a_trusted_proxy_s_https_origin_gets_the_host_cookie_and_passes_same_origin() {
    let gw = Gw::start(|s| s.trusted_proxies = vec!["127.0.0.1/32".into()]).await;
    let key = gw.key();
    let (status, cookie) = sign_in(&gw, "https://lmgw.example", &proxied("203.0.113.5")).await;
    assert_eq!(status, 204, "{cookie}");
    assert!(
        cookie.starts_with(&format!("__Host-lmgw_session={key}; Path=/;"))
            && cookie.ends_with("; Secure"),
        "{cookie}"
    );
    let status = |origin: &'static str, cookie: String| {
        let base = gw.base.clone();
        async move {
            with(
                reqwest::Client::new().get(format!("{base}/api/status")),
                &proxied("203.0.113.5"),
            )
            .header("cookie", cookie)
            .header("origin", origin)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
        }
    };
    let host_cookie = format!("__Host-lmgw_session={key}");
    assert_eq!(
        status("https://lmgw.example", host_cookie.clone()).await,
        200
    );
    assert_eq!(
        status("http://lmgw.example", host_cookie.clone()).await,
        403,
        "the client used https: a plain origin is another one"
    );
    assert_eq!(
        status("https://lmgw.example", format!("lmgw_session={key}")).await,
        401,
        "on https only the host-prefixed cookie counts"
    );
    gw.stop().await;
}

#[tokio::test]
async fn the_same_headers_from_an_untrusted_peer_change_nothing() {
    for trusted in [Vec::new(), vec!["192.0.2.0/24".to_string()]] {
        let gw = Gw::start(|s| s.trusted_proxies = trusted.clone()).await;
        let (status, cookie) = sign_in(&gw, "https://lmgw.example", &proxied("203.0.113.5")).await;
        assert_eq!(status, 403, "{trusted:?}: the origin is not http://<Host>");
        assert!(cookie.is_empty(), "{cookie}");
        // The request's own origin still signs in, with the plain cookie.
        let own = gw.base.clone();
        let (status, cookie) = sign_in(&gw, &own, &proxied("203.0.113.5")).await;
        assert_eq!(status, 204);
        assert!(
            cookie.starts_with("lmgw_session=") && !cookie.contains("Secure"),
            "{cookie}"
        );
        // A desktop with proxy mode off notes no first peer (D53).
        let card = forwarded::status(&gw.state.snapshot().settings, &gw.state.proxy_seen);
        assert_eq!(
            card.first_peers.is_empty(),
            trusted.is_empty(),
            "{trusted:?}: {card:?}"
        );
        gw.stop().await;
    }
}

#[tokio::test]
async fn the_mcp_guard_reads_the_forwarded_scheme() {
    let gw = Gw::start(|s| {
        s.trusted_proxies = vec!["127.0.0.1/32".into()];
        s.public_url = "https://lmgw.example".into();
    })
    .await;
    let allowed = |headers: Vec<(&'static str, String)>, origin: &'static str| {
        let (base, key) = (gw.base.clone(), gw.key());
        async move {
            let r = with(reqwest::Client::new().post(format!("{base}/mcp")), &headers)
                .bearer_auth(key)
                .header("origin", origin)
                .header("accept", "application/json, text/event-stream")
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
                .send()
                .await
                .unwrap();
            let status = r.status().as_u16();
            !(status == 403 && r.text().await.unwrap() == "origin not allowed")
        }
    };
    assert!(allowed(proxied("203.0.113.5"), "https://lmgw.example").await);
    assert!(!allowed(proxied("203.0.113.5"), "http://lmgw.example").await);
    assert!(
        !allowed(Vec::new(), "https://lmgw.example").await,
        "without the proxy's word the request is plain http"
    );
    gw.stop().await;
}

/// The edge alone, in front of a route that answers what it was handed:
/// the connect info, the `Host`, and whether a forwarded header survived.
async fn edge_echo(trusted: &[&str]) -> (SharedState, String, tokio::task::JoinHandle<()>) {
    let state = AppState::init_for_tests().await.unwrap();
    let list: Vec<String> = trusted.iter().map(|t| t.to_string()).collect();
    crate::main_tls::set(&state, |s| s.trusted_proxies = list).await;
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::get(
                |ConnectInfo(c): ConnectInfo<Conn>, h: HeaderMap| async move {
                    let left: Vec<&str> = [
                        "x-forwarded-for",
                        "x-forwarded-proto",
                        "x-forwarded-host",
                        "x-forwarded-port",
                        "forwarded",
                    ]
                    .into_iter()
                    .filter(|n| h.contains_key(*n))
                    .collect();
                    axum::Json(json!({
                        "peer": c.peer.to_string(),
                        "proxy": c.proxy.map(|p| p.ip().to_string()),
                        "scheme": c.scheme().as_str(),
                        "host": h.get("host").map(|v| v.to_str().unwrap().to_string()),
                        "left": left,
                    }))
                },
            ),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            forwarded::edge,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<Conn>())
            .await
            .unwrap();
    });
    (state, base, served)
}

async fn echo(base: &str, headers: &[(&'static str, &str)]) -> (u16, Value) {
    let r = headers
        .iter()
        .fold(
            reqwest::Client::new().get(format!("{base}/")),
            |rb, (k, v)| rb.header(*k, *v),
        )
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

#[tokio::test]
async fn the_edge_writes_the_client_the_scheme_and_the_host_and_strips_the_rest() {
    let (state, base, served) = edge_echo(&["127.0.0.1/32"]).await;
    let (status, e) = echo(
        &base,
        &[
            ("x-forwarded-for", "127.0.0.1, 203.0.113.5, 127.0.0.1"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "lmgw.example:8443"),
            ("x-forwarded-port", "8443"),
        ],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(e["peer"], json!("203.0.113.5:0"), "{e}");
    assert_eq!(e["proxy"], json!("127.0.0.1"));
    assert_eq!(e["scheme"], json!("https"));
    assert_eq!(e["host"], json!("lmgw.example:8443"));
    assert_eq!(e["left"], json!([]), "nothing past the edge reads them");
    // RFC 7239: the client's element.
    let (_, e) = echo(
        &base,
        &[(
            "forwarded",
            "for=127.0.0.1;proto=http, for=\"[2001:db8::5]:4711\";proto=https;host=lmgw.example",
        )],
    )
    .await;
    assert_eq!(e["peer"], json!("[2001:db8::5]:4711"), "{e}");
    assert_eq!(
        (&e["scheme"], &e["host"]),
        (&json!("https"), &json!("lmgw.example"))
    );
    // A direct request from the trusted address: its own, nothing forwarded.
    let (_, e) = echo(&base, &[]).await;
    assert_eq!(e["proxy"], Value::Null);
    assert_eq!(e["scheme"], json!("http"));
    // Refusals: both families, values that differ, two Hosts.
    for headers in [
        vec![
            ("x-forwarded-for", "203.0.113.5"),
            ("forwarded", "for=203.0.113.6"),
        ],
        vec![("x-forwarded-proto", "https, http")],
        vec![
            ("x-forwarded-host", "lmgw.example"),
            ("x-forwarded-host", "evil.example"),
        ],
    ] {
        let (status, body) = echo(&base, &headers).await;
        assert_eq!(status, 400, "{headers:?}");
        assert_eq!(body["code"], json!(forwarded::REFUSED_CODE), "{body}");
    }
    let (status, body) = echo(&base, &[("host", "a.example"), ("host", "b.example")]).await;
    assert_eq!((status, &body["code"]), (400, &json!("host_invalid")));
    // The card counts the refusals and names the first peer.
    let seen = forwarded::status(&state.snapshot().settings, &state.proxy_seen);
    assert!(seen.on);
    assert!(
        seen.refused
            .as_deref()
            .unwrap_or("")
            .starts_with("3 request(s) refused"),
        "{seen:?}"
    );
    assert!(
        seen.first_peers[0].contains("came from 127.0.0.1")
            && seen.first_peers[0].contains("a trusted proxy"),
        "{seen:?}"
    );
    assert!(
        seen.warnings
            .iter()
            .any(|w| w.contains("no public URL is set")),
        "{seen:?}"
    );
    served.abort();
}

#[tokio::test]
async fn from_an_untrusted_peer_the_edge_only_strips() {
    let (_state, base, served) = edge_echo(&["192.0.2.0/24", "::/0"]).await;
    let (status, e) = echo(
        &base,
        &[
            ("x-forwarded-for", "198.51.100.1"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "evil.example"),
            ("forwarded", "for=198.51.100.2"),
        ],
    )
    .await;
    assert_eq!(
        status, 200,
        "both families from an untrusted peer: not even read"
    );
    let peer: SocketAddr = e["peer"].as_str().unwrap().parse().unwrap();
    assert!(peer.ip().is_loopback(), "{e}");
    assert_eq!(e["proxy"], Value::Null);
    assert_eq!(e["scheme"], json!("http"));
    assert_ne!(e["host"], json!("evil.example"));
    assert_eq!(e["left"], json!([]));
    served.abort();
}

#[tokio::test]
async fn the_trusted_proxies_are_a_dashboard_setting_the_patch_op_refuses() {
    let gw = Gw::start(|_| {}).await;
    let op = |name: &'static str, args: Value| {
        let (base, key) = (gw.base.clone(), gw.key());
        async move {
            let r = reqwest::Client::new()
                .post(format!("{base}/api/op/{name}"))
                .bearer_auth(key)
                .json(&args)
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        }
    };
    let (status, res) = op(
        "settings_set_full",
        json!({ "trusted_proxies": ["127.0.0.1", "fd00::/8"] }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(
        gw.state.snapshot().settings.trusted_proxies,
        vec!["127.0.0.1/32", "fd00::/8"]
    );
    // Applied at once, parsed once for the edge; the record starts again.
    assert_eq!(gw.state.snapshot().trusted_nets.len(), 2);
    assert_eq!(gw.state.proxy_seen.since(), forwarded::Since::Save);
    let (status, res) = op(
        "settings_set_full",
        json!({ "trusted_proxies": ["192.0.2.5/24"] }),
    )
    .await;
    assert_ne!(status, 200, "{res}");
    assert!(res.to_string().contains("192.0.2.0/24"), "{res}");
    let (status, res) = op("settings_set", json!({ "trusted_proxies": [] })).await;
    // An access setting no self-admin tool changes: the op does not know
    // the field at all.
    assert_eq!((status, &res["code"]), (400, &json!("op_failed")), "{res}");
    assert!(
        res["message"]
            .as_str()
            .unwrap_or("")
            .starts_with("invalid arguments: unknown field `trusted_proxies`"),
        "{res}"
    );
    assert_eq!(gw.state.snapshot().settings.trusted_proxies.len(), 2);
    gw.stop().await;
}

#[tokio::test]
async fn the_edge_s_refusals_are_logged_in_a_request_span() {
    let (log, _guard) = crate::common::captured_log::capture_log_at(tracing::Level::DEBUG);
    let (_state, base, served) = edge_echo(&["127.0.0.1/32"]).await;
    let (status, _) = echo(&base, &[("host", "a.example"), ("host", "b.example")]).await;
    assert_eq!(status, 400);
    let (status, _) = echo(
        &base,
        &[
            ("x-forwarded-for", "203.0.113.5"),
            ("forwarded", "for=203.0.113.6"),
        ],
    )
    .await;
    assert_eq!(status, 400);
    served.abort();
    let text = log.text();
    for code in ["host_invalid", forwarded::REFUSED_CODE] {
        let line = text
            .lines()
            .find(|l| l.contains(&format!("code=\"{code}\"")))
            .unwrap_or_else(|| panic!("no line for {code}: {text}"));
        assert!(
            line.contains("request{method=GET path=/ via=plain client=127.0.0.1 scheme=http")
                && line.contains("refused at the edge"),
            "{line}"
        );
    }
    // The first refusal from a trusted proxy is a warning as well.
    assert!(
        text.contains("proxy mode: a request from the trusted proxy 127.0.0.1 was refused"),
        "{text}"
    );
}
