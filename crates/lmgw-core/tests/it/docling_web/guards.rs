//! The security review round's cases (WP5b): a request without a key is
//! refused for web sources whatever "Require gateway API keys on /v1" says
//! (also with a cross-site `Origin`), while its uploads keep working; the
//! options are judged before anything is downloaded; `max_body_mb` at 0 and
//! too many headers are named; a refused redirect is a refusal in the
//! record; the environment's proxies receive nothing; a certificate lmgw
//! does not trust fails the source.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use lmgw_core::store;
use serde_json::{json, Map, Value};

use super::{files, post, settings, src};
use crate::docling_serve::{b64, gateway};

fn with_key(key: &str) -> reqwest::Client {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert("x-api-key", key.parse().unwrap());
    reqwest::Client::builder()
        .default_headers(h)
        .build()
        .unwrap()
}

#[tokio::test]
async fn web_sources_need_a_key_whatever_require_api_keys_says() {
    let (state, gw) = gateway().await;
    let (port, seen) = files().await;
    settings(&state, true, &[format!("127.0.0.1:{port}")]).await;
    assert!(!state.snapshot().settings.auth_enabled, "keys not required");
    let url = format!("http://127.0.0.1:{port}/docs/example-notes.md");
    // No credential, as a page open in a browser on this machine would
    // post it: with and without a cross-site Origin.
    for origin in [None, Some("https://page.example")] {
        let mut rb = gw
            .anon()
            .post(format!("{gw}/v1/convert/source"))
            .json(&src(url.clone()));
        if let Some(o) = origin {
            rb = rb.header("origin", o);
        }
        let r = rb.send().await.unwrap();
        assert_eq!(r.status().as_u16(), 401, "{origin:?}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["code"], "http_source_needs_key");
        let d = v["detail"].as_str().unwrap();
        assert!(
            d.starts_with("http_source_needs_key: web addresses need an API key"),
            "{d}"
        );
    }
    assert!(seen.lock().unwrap().is_empty(), "nothing was fetched");
    // Everything else on the route keeps working without a key.
    let r = gw
        .anon()
        .post(format!("{gw}/v1/convert/source"))
        .json(&json!({"sources": [{
            "kind": "file", "base64_string": b64(b"# Uploaded\n\nText."),
            "filename": "example-upload.md"
        }]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "success", "{v}");
    // A plain inference key fetches.
    store::insert_api_key(
        &state.db,
        "example-caller",
        &lmgw_core::config::hash_api_key("lmgw-example-caller"),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let r = with_key("lmgw-example-caller")
        .post(format!("{gw}/v1/convert/source"))
        .header("origin", "https://page.example")
        .json(&src(url))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_options_are_judged_before_anything_is_downloaded() {
    let (state, gw) = gateway().await;
    let (port, seen) = files().await;
    settings(&state, true, &[format!("127.0.0.1:{port}")]).await;
    let mut body = src(format!("http://127.0.0.1:{port}/docs/example-notes.md"));
    body["options"] = json!({"do_ocr": "maybe"});
    let r = post(&gw, "/v1/convert/source", body).await;
    assert_eq!(r.status().as_u16(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "invalid_option", "{v}");
    assert!(seen.lock().unwrap().is_empty(), "a 422 downloads nothing");
}

#[tokio::test]
async fn a_size_limit_of_0_and_too_many_headers_are_named() {
    let (state, gw) = gateway().await;
    let (port, seen) = files().await;
    settings(&state, true, &[format!("127.0.0.1:{port}")]).await;
    let url = format!("http://127.0.0.1:{port}/docs/example-notes.md");
    let mut s = state.snapshot().settings.clone();
    let before = s.max_body_mb;
    s.max_body_mb = 0;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let r = post(&gw, "/v1/convert/source", src(url.clone())).await;
    assert_eq!(r.status().as_u16(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "http_source_needs_limit");
    let d = v["detail"].as_str().unwrap();
    assert!(
        d.contains("web sources need a set request size limit") && d.contains("Max request body"),
        "{d}"
    );
    s.max_body_mb = before;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    // More header names than an HTTP header map holds: docling's 422, not
    // a panic in the connection.
    let many: Map<String, Value> = (0..30_000)
        .map(|i| (format!("x-example-{i}"), Value::from("v")))
        .collect();
    let r = post(
        &gw,
        "/v1/convert/source",
        json!({"sources": [{"kind": "http", "url": url, "headers": many}]}),
    )
    .await;
    assert_eq!(r.status().as_u16(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "invalid_source");
    assert!(
        v["detail"]
            .as_str()
            .unwrap()
            .contains("more headers than one request can carry"),
        "{v}"
    );
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_refused_redirect_is_a_refusal_in_the_record() {
    let (state, gw) = gateway().await;
    let (port, _) = files().await;
    settings(&state, true, &[format!("127.0.0.1:{port}")]).await;
    for path in ["moved", "missing.md"] {
        let r = post(
            &gw,
            "/v1/convert/source",
            src(format!("http://127.0.0.1:{port}/{path}")),
        )
        .await;
        assert_eq!(r.status().as_u16(), 200, "{path}");
    }
    let rows = store::convert_calls(&state.db, "/v1/convert/")
        .await
        .unwrap();
    let hosts: Vec<&str> = rows.iter().map(|r| r.hosts.as_str()).collect();
    assert!(
        hosts.contains(&r#"[{"host":"127.0.0.1","outcome":"refused"}]"#)
            && hosts.contains(&r#"[{"host":"127.0.0.1","outcome":"failed"}]"#),
        "{hosts:?}"
    );
}

#[tokio::test]
async fn the_environments_proxies_receive_nothing() {
    let _env = crate::common::process_env_lock().await;
    let (state, gw) = gateway().await;
    let (port, seen) = files().await;
    settings(&state, true, &[format!("127.0.0.1:{port}")]).await;
    // The test's own client, built before the variables are set.
    let client = gw.client();
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = proxy.accept().await {
            count.fetch_add(1, Ordering::SeqCst);
            drop(sock);
        }
    });
    for k in ["NO_PROXY", "no_proxy"] {
        std::env::remove_var(k);
    }
    for k in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        std::env::set_var(k, &proxy_url);
    }
    // The variables are in force: a client that reads them goes to the
    // proxy.
    let url = format!("http://127.0.0.1:{port}/docs/example-notes.md");
    let _ = reqwest::Client::new().get(&url).send().await;
    let control = accepted.load(Ordering::SeqCst);
    assert!(control >= 1, "the proxy variables reach a default client");
    let r = client
        .post(format!("{gw}/v1/convert/source"))
        .json(&src(url))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        control,
        "nothing via the proxy"
    );
    assert_eq!(seen.lock().unwrap().len(), 1, "fetched directly");
}

#[tokio::test]
async fn a_certificate_lmgw_does_not_trust_fails_the_source() {
    // The TLS listener of another gateway, its pair from a test CA that is
    // in no system store.
    let pki = crate::main_tls::Pki::new();
    let served = crate::main_tls::serve(&pki, |_| {}).await;
    let tls = served.tls.port();
    let (state, gw) = gateway().await;
    settings(&state, true, &[format!("127.0.0.1:{tls}")]).await;
    let r = post(
        &gw,
        "/v1/convert/source",
        src(format!("https://127.0.0.1:{tls}/health")),
    )
    .await;
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "failure", "{v}");
    let m = v["errors"][0]["error_message"].as_str().unwrap();
    assert!(
        m.starts_with("source_unreachable: lmgw could not connect to 127.0.0.1")
            && m.contains("certificate"),
        "{m}"
    );
    served.stop().await;
}
