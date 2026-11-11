//! What the request's scheme decides (headless server design §4.3): the
//! session cookie's name and `Secure`, the same-origin rule, `/mcp`'s
//! rebinding guard, and the URLs `/api/connect`, the wiring view and the
//! pairing proposal hand out.

use lmgw_core::agents::token::OWNER_DASHBOARD;
use serde_json::{json, Value};

use super::{serve, Pki};

fn owner(gw: &super::Served) -> String {
    gw.state
        .snapshot()
        .owner_key(OWNER_DASHBOARD)
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn the_cookie_is_secure_and_host_prefixed_on_tls_and_each_scheme_reads_its_own() {
    let pki = Pki::new();
    let gw = serve(&pki, |_| {}).await;
    let key = owner(&gw);
    let tls = format!("https://localhost:{}", gw.tls.port());
    let http = pki.http();
    let signed = http
        .post(format!("{tls}/api/session"))
        .header("origin", &tls)
        .json(&json!({ "token": key }))
        .send()
        .await
        .unwrap();
    assert_eq!(signed.status(), 204);
    let cookie = signed.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(
        cookie.starts_with(&format!("__Host-lmgw_session={key}; Path=/;"))
            && cookie.ends_with("; Secure"),
        "{cookie}"
    );
    assert!(!cookie.contains("Domain"), "{cookie}");

    // The plain listener's cookie has neither.
    let plain = format!("http://{}", gw.plain);
    let signed = reqwest::Client::new()
        .post(format!("{plain}/api/session"))
        .header("origin", &plain)
        .json(&json!({ "token": key }))
        .send()
        .await
        .unwrap();
    let cookie = signed.headers()["set-cookie"].to_str().unwrap();
    assert!(
        cookie.starts_with("lmgw_session=") && !cookie.contains("Secure"),
        "{cookie}"
    );

    // On TLS the host-prefixed cookie signs in, from the https origin only.
    let session = |c: String| {
        let (http, tls) = (http.clone(), tls.clone());
        async move {
            let r = http
                .get(format!("{tls}/api/session"))
                .header("cookie", c)
                .send()
                .await
                .unwrap();
            r.json::<Value>().await.unwrap()["authenticated"].as_bool()
        }
    };
    assert_eq!(
        session(format!("__Host-lmgw_session={key}")).await,
        Some(true)
    );
    assert_eq!(
        session(format!("lmgw_session={key}")).await,
        Some(false),
        "the plain cookie is sent to every port of the host: on TLS it is nobody's"
    );
    let status = |origin: String| {
        let (http, tls, key) = (http.clone(), tls.clone(), key.clone());
        async move {
            http.get(format!("{tls}/api/status"))
                .header("cookie", format!("__Host-lmgw_session={key}"))
                .header("origin", origin)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    assert_eq!(
        status(tls.clone()).await,
        200,
        "the https origin is this gateway's"
    );
    let wrong = format!("http://localhost:{}", gw.tls.port());
    assert_eq!(
        status(wrong).await,
        403,
        "the wrong scheme on the same host and port"
    );
    gw.stop().await;
}

/// `/mcp`'s rebinding guard: the scheme of the listener the request came
/// in on, its own port or the public origin's, a portless origin by its
/// scheme's default, and the public host besides loopback.
#[tokio::test]
async fn the_mcp_guard_reads_the_scheme_the_port_and_the_public_origin() {
    let pki = Pki::new();
    let gw = serve(&pki, |s| s.public_url = "https://lmgw.example".into()).await;
    let key = owner(&gw);
    let http = pki.http();
    let tls = format!("https://localhost:{}", gw.tls.port());
    let post = |origin: String| {
        let (http, tls, key) = (http.clone(), tls.clone(), key.clone());
        async move {
            let r = http
                .post(format!("{tls}/mcp"))
                .bearer_auth(key)
                .header("origin", origin)
                .header("accept", "application/json, text/event-stream")
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
                .send()
                .await
                .unwrap();
            let status = r.status().as_u16();
            let body = r.text().await.unwrap();
            !(status == 403 && body == "origin not allowed")
        }
    };
    let port = gw.tls.port();
    for (origin, allowed) in [
        (format!("https://localhost:{port}"), true),
        (format!("https://127.0.0.1:{port}"), true),
        (format!("http://localhost:{port}"), false),
        // Each host with its own port: a loopback name on the public port
        // (443 here) is another server on this box.
        ("https://localhost".to_string(), false),
        ("https://127.0.0.1".to_string(), false),
        ("https://localhost:9999".to_string(), false),
        // The public host on the public port or the listener's.
        ("https://lmgw.example".to_string(), true),
        (format!("https://lmgw.example:{port}"), true),
        ("https://lmgw.example:9999".to_string(), false),
        (format!("https://evil.example:{port}"), false),
        ("https://evil.example".to_string(), false),
        ("http://lmgw.example".to_string(), false),
    ] {
        assert_eq!(post(origin.clone()).await, allowed, "{origin}");
    }
    gw.stop().await;
}

#[tokio::test]
async fn the_url_builders_hand_out_the_public_origin() {
    let pki = Pki::new();
    let gw = serve(&pki, |s| s.public_url = "https://lmgw.example:8443".into()).await;
    let key = owner(&gw);
    let get = |path: &'static str| {
        let (key, plain) = (key.clone(), gw.plain);
        async move {
            reqwest::Client::new()
                .get(format!("http://{plain}{path}"))
                .bearer_auth(key)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let connect = get("/api/connect").await;
    let bases: Vec<&str> = connect["bases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["url"].as_str().unwrap())
        .collect();
    assert_eq!(bases[0], "https://lmgw.example:8443", "{connect}");
    assert_eq!(bases[1], format!("https://127.0.0.1:{}", gw.tls.port()));
    // D42 on the desktop: the plain loopback stays for this machine's
    // clients.
    let plain_base = lmgw_core::net::primary_base_url(&gw.state.snapshot().settings.bind_addr);
    assert_eq!(bases[2..], [plain_base.as_str()], "{bases:?}");
    let wiring = get("/api/wiring").await;
    assert_eq!(
        wiring["base_url"],
        json!("https://lmgw.example:8443"),
        "{wiring}"
    );
    // The pairing form proposes the public name first.
    let proposed = lmgw_core::devices::proposed_for_device(&gw.state.snapshot().settings).unwrap();
    let port = gw
        .state
        .snapshot()
        .settings
        .devices
        .effective_port(&gw.state.snapshot().settings.bind_addr)
        .unwrap();
    assert_eq!(proposed[0].url, format!("https://lmgw.example:{port}"));
    assert_eq!(proposed[0].interface, "public URL");
    // lmgw's own dial to an agent's MCP stays on the plain loopback listener.
    let own = lmgw_core::agents::service::mcp_url(&gw.state.snapshot().settings.bind_addr, "board");
    assert!(own.starts_with("http://127.0.0.1:"), "{own}");
    gw.stop().await;
}

/// A desktop's save of a TLS pair and address applies at the next start
/// (D41): until then nothing serves TLS, and every URL stays the plain
/// listener's; the restart brings them over to the TLS listener.
#[tokio::test]
async fn a_desktop_save_of_tls_leaves_the_urls_plain_until_the_restart() {
    let pki = Pki::new();
    pki.write(&["localhost", "127.0.0.1"], 1);
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    super::set(&state, |s| s.realtime.webrtc.webrtc_enabled = false).await;
    let listener = lmgw_core::server::bind(&state, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let plain = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (released_tx, released) = tokio::sync::oneshot::channel();
    let served = tokio::spawn(lmgw_core::server::serve(
        state.clone(),
        listener,
        async {
            let _ = stopped.await;
        },
        Some(released_tx),
    ));
    let key = state
        .snapshot()
        .owner_key(OWNER_DASHBOARD)
        .unwrap()
        .to_string();
    let client = reqwest::Client::new();
    crate::common::patience::until_async("the plain listener answers", || async {
        client
            .get(format!("http://{plain}/api/version"))
            .send()
            .await
            .is_ok_and(|r| r.status() == 200)
    })
    .await;
    let tls_port = super::free_port();
    let saved = client
        .post(format!("http://{plain}/api/op/settings_set_full"))
        .bearer_auth(&key)
        .json(&json!({"tls": {
            "cert_path": pki.cert_path().display().to_string(),
            "key_path": pki.key_path().display().to_string(),
            "listen": format!("127.0.0.1:{tls_port}"),
        }}))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), 200, "{}", saved.text().await.unwrap());
    let get = |path: &'static str| {
        let (client, key) = (client.clone(), key.clone());
        async move {
            client
                .get(format!("http://{plain}{path}"))
                .bearer_auth(key)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let urls = |connect: &Value| -> Vec<String> {
        connect["bases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["url"].as_str().unwrap().to_string())
            .collect()
    };
    let connect = get("/api/connect").await;
    assert!(
        urls(&connect).iter().all(|u| u.starts_with("http://")),
        "saved, not started: {connect}"
    );
    let full = get("/api/settings-full").await;
    assert!(
        full["public_base_url"]
            .as_str()
            .unwrap()
            .starts_with("http://"),
        "{}",
        full["public_base_url"]
    );
    assert_eq!(full["tls_status"]["restart_pending"], json!(true));
    assert_eq!(state.snapshot().serving_tls, None);

    // The restart: this start's listeners go, the next brings TLS up.
    let _ = stop.send(());
    let _ = released.await;
    let _ = served.await;
    let again = super::serve_on(state, &pki, |s| {
        s.tls.listen = format!("127.0.0.1:{tls_port}");
    })
    .await;
    let connect = get_on(&again, "/api/connect", &key).await;
    assert_eq!(
        urls(&connect)[0],
        format!("https://127.0.0.1:{tls_port}"),
        "{connect}"
    );
    again.stop().await;
}

async fn get_on(gw: &super::Served, path: &str, key: &str) -> Value {
    reqwest::Client::new()
        .get(format!("http://{}{path}", gw.plain))
        .bearer_auth(key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// The start's login line (§3.4) names the address this start serves on:
/// the plain listener as bound (a port of 0 is the one the OS chose) while
/// no TLS listener serves, the TLS listener while one does.
#[tokio::test]
async fn the_login_line_names_the_bound_listener() {
    let (log, _guard) = crate::common::captured_log::capture_log();
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    super::set(&state, |s| {
        s.realtime.webrtc.webrtc_enabled = false;
        s.bind_addr = "127.0.0.1:0".into();
    })
    .await;
    let listener = lmgw_core::server::bind(&state, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let plain = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (released_tx, released) = tokio::sync::oneshot::channel();
    let served = tokio::spawn(lmgw_core::server::serve(
        state.clone(),
        listener,
        async {
            let _ = stopped.await;
        },
        Some(released_tx),
    ));
    let line = format!("dashboard login: http://{plain}/api/session/login?token=");
    crate::common::patience::until("the login line", || log.text().contains(&line)).await;
    let _ = stop.send(());
    let _ = released.await;
    let _ = served.await;

    let pki = Pki::new();
    pki.write(&["localhost", "127.0.0.1"], 1);
    let gw = super::serve_on(state, &pki, |_| {}).await;
    let line = format!(
        "dashboard login: https://127.0.0.1:{}/api/session/login?token=",
        gw.tls.port()
    );
    crate::common::patience::until("the TLS login line", || log.text().contains(&line)).await;
    gw.stop().await;
}

/// D41: a public URL whose host the stored agent origin suffix would then
/// shadow is refused as the suffix's own save is, and nothing is saved.
#[tokio::test]
async fn a_public_url_that_makes_the_suffix_shadow_the_dashboard_is_refused() {
    let pki = Pki::new();
    let gw = serve(&pki, |s| s.agent_origin_suffix = "apps.lmgw.example".into()).await;
    let key = owner(&gw);
    let saved = reqwest::Client::new()
        .post(format!("http://{}/api/op/settings_set_full", gw.plain))
        .bearer_auth(&key)
        .json(&json!({"public_url": "https://lmgw.example"}))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), 400);
    let body: Value = saved.json().await.unwrap();
    assert_eq!(
        body["code"],
        json!("origin_suffix_shadows_gateway"),
        "{body}"
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .starts_with("the public URL https://lmgw.example makes"),
        "{body}"
    );
    assert_eq!(gw.state.snapshot().settings.public_url, "");
    gw.stop().await;
}
