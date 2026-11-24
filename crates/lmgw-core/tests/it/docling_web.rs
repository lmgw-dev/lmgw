//! The docling adapter's `http` sources over HTTP (document-conversion
//! design ruling 9, D28, §7.4.1, WP5b): the setting off, on without the
//! host, on with it; the fetched bytes converted like an upload, a failed
//! source failing alone, the async route, the headers, and the settings op.
//! The sources are served by a loopback server started here, listed by its
//! address and port, so the "listed explicitly" rule is what lets it in.

use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
use axum::routing::get;
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde_json::{json, Value};

use crate::common::Gw;
use crate::docling_serve::{b64, gateway};

/// The security review round's cases: keyless callers, options before
/// downloads, the named limits, refused probes in the record, the
/// environment's proxies, a TLS failure.
mod guards;

/// A loopback file server: its port, and the requests' paths with their
/// `authorization` and `accept-encoding` headers.
type Seen = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;

async fn files() -> (u16, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let note = move |path: &str, h: &HeaderMap| {
        let v = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(String::from);
        log.lock()
            .unwrap()
            .push((path.to_string(), v("authorization"), v("accept-encoding")));
    };
    let (n1, n2, n3) = (note.clone(), note.clone(), note);
    let app = axum::Router::new()
        .route(
            "/docs/example-notes.md",
            get(move |h: HeaderMap| async move {
                n1("/docs/example-notes.md", &h);
                "# Example notes\n\nFetched from the web."
            }),
        )
        .route(
            "/missing.md",
            get(move |h: HeaderMap| async move {
                n2("/missing.md", &h);
                (axum::http::StatusCode::NOT_FOUND, "example-internal-detail")
            }),
        )
        .route(
            "/moved",
            get(move |h: HeaderMap| async move {
                n3("/moved", &h);
                (
                    axum::http::StatusCode::FOUND,
                    [("location", "http://198.51.100.7/elsewhere.md")],
                )
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await });
    (port, seen)
}

async fn settings(state: &SharedState, on: bool, hosts: &[String]) {
    let mut s = state.snapshot().settings.clone();
    s.documents.fetch_sources = on;
    s.documents.source_hosts = hosts.to_vec();
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

async fn post(gw: &Gw, route: &str, body: Value) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}{route}"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn src(url: String) -> Value {
    json!({"sources": [{"kind": "http", "url": url}]})
}

#[tokio::test]
async fn off_on_without_the_host_and_on_with_it() {
    let (state, gw) = gateway().await;
    let (port, seen) = files().await;
    let url = format!("http://127.0.0.1:{port}/docs/example-notes.md?token=example-secret");

    // Off (the default): the named 400 with the setting, nothing fetched.
    let r = post(&gw, "/v1/convert/source", src(url.clone())).await;
    assert_eq!(r.status().as_u16(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "http_source_refused");
    let d = v["detail"].as_str().unwrap();
    assert!(
        d.contains("base64_string") && d.contains("documents.fetch_sources"),
        "{d}"
    );

    // On, the host not listed: the named 400 with the host and the setting.
    settings(&state, true, &["docs.example".into()]).await;
    let r = post(&gw, "/v1/convert/source", src(url.clone())).await;
    assert_eq!(r.status().as_u16(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "http_source_host_refused");
    let d = v["detail"].as_str().unwrap();
    assert!(
        d.contains("127.0.0.1 (port") && d.contains("documents.source_hosts"),
        "{d}"
    );
    assert!(
        !d.contains("example-secret") && !d.contains("/docs/"),
        "{d}"
    );
    // The name's listed, but not the address: loopback is refused.
    settings(&state, true, &[format!("localhost:{port}")]).await;
    let r = post(
        &gw,
        "/v1/convert/source",
        src(format!("http://localhost:{port}/docs/example-notes.md")),
    )
    .await;
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "failure", "{v}");
    let m = v["errors"][0]["error_message"].as_str().unwrap();
    assert!(m.starts_with("source_address_refused: localhost"), "{m}");
    assert!(seen.lock().unwrap().is_empty(), "nothing was fetched yet");

    // On, the address listed with its port: fetched and converted.
    settings(&state, true, &[format!("127.0.0.1:{port}")]).await;
    let r = post(&gw, "/v1/convert/source", src(url.clone())).await;
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(v["document"]["filename"], "example-notes.md");
    assert!(v["document"]["md_content"]
        .as_str()
        .unwrap()
        .contains("Fetched from the web."));

    // The record keeps each host with its outcome, never the URL.
    let rows = store::convert_calls(&state.db, "/v1/convert/")
        .await
        .unwrap();
    let all = format!("{rows:?}");
    assert!(
        all.contains("fetched") && all.contains("127.0.0.1"),
        "{all}"
    );
    assert!(
        !all.contains("example-secret") && !all.contains("example-notes"),
        "{all}"
    );
}

#[tokio::test]
async fn a_failed_source_fails_alone_beside_the_others() {
    let (state, gw) = gateway().await;
    let (port, _) = files().await;
    settings(&state, true, &[format!("127.0.0.1:{port}")]).await;
    let body = json!({
        "sources": [
            {"kind": "http", "url": format!("http://127.0.0.1:{port}/docs/example-notes.md")},
            {"kind": "file", "base64_string": b64(b"# Uploaded\n\nText."), "filename": "example-upload.md"},
            {"kind": "http", "url": format!("http://127.0.0.1:{port}/missing.md")},
            {"kind": "http", "url": format!("http://127.0.0.1:{port}/moved")}
        ],
        "target": {"kind": "zip"}
    });
    let r = post(&gw, "/v1/convert/source", body).await;
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        r.headers()["x-lmgw-documents"],
        "converted=2, partial=0, failed=2, skipped=0"
    );
    let bytes = r.bytes().await.unwrap();
    let z = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
    let mut names: Vec<String> = z.file_names().map(String::from).collect();
    names.sort();
    assert_eq!(names, ["example-notes.md", "example-upload.md"]);

    // The same request as one-file answers names each error.
    for (path, start) in [
        (
            "missing.md",
            "source_http_status: 127.0.0.1 answered HTTP 404",
        ),
        ("moved", "source_redirect_refused: 127.0.0.1 redirected"),
    ] {
        let r = post(
            &gw,
            "/v1/convert/source",
            src(format!("http://127.0.0.1:{port}/{path}")),
        )
        .await;
        let v: Value = r.json().await.unwrap();
        let m = v["errors"][0]["error_message"].as_str().unwrap();
        assert!(m.starts_with(start), "{m}");
        assert!(
            !m.contains("example-internal") && !m.contains("198.51.100.7"),
            "{m}"
        );
    }
}

#[tokio::test]
async fn an_async_source_is_fetched_at_acceptance_and_its_headers_go_to_its_host() {
    let (state, gw) = gateway().await;
    let (port, seen) = files().await;
    settings(&state, true, &[format!("127.0.0.1:{port}")]).await;
    let body = json!({"http_sources": [{
        "url": format!("http://127.0.0.1:{port}/docs/example-notes.md"),
        "headers": {"Authorization": "Bearer example-secret", "Accept-Encoding": "gzip"}
    }]});
    let r = post(&gw, "/v1/convert/source/async", body).await;
    assert_eq!(r.status().as_u16(), 200);
    let notes = r.headers()["x-lmgw-option-notes"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        notes.contains("headers=mapped(") && notes.contains("accept-encoding"),
        "{notes}"
    );
    assert!(!notes.contains("example-secret"), "{notes}");
    let t: Value = r.json().await.unwrap();
    let id = t["task_id"].as_str().unwrap();
    // Fetched before the 202-equivalent answer: the server saw it already.
    {
        let s = seen.lock().unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].1.as_deref(), Some("Bearer example-secret"));
        assert_eq!(s[0].2, None, "no encoding is asked for");
    }
    let done: Value = gw
        .client()
        .get(format!("{gw}/v1/status/poll/{id}?wait=30"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(done["task_status"], "success", "{done}");
    let v: Value = gw
        .client()
        .get(format!("{gw}/v1/result/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v["document"]["md_content"]
        .as_str()
        .unwrap()
        .contains("Fetched from the web."));
    // The header value is in no stored row.
    let rows = store::convert_calls(&state.db, "/v1/convert/")
        .await
        .unwrap();
    assert!(!format!("{rows:?}").contains("example-secret"));
}

#[tokio::test]
async fn the_settings_op_stores_the_sites_normalised_and_refuses_what_is_not_a_host() {
    let (state, gw) = gateway().await;
    let set = |body: Value| {
        let gw = &gw;
        async move {
            gw.client()
                .post(format!("{gw}/api/op/settings_set"))
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };
    let r = set(json!({"documents_source_hosts": ["https://docs.example/a"]})).await;
    assert!(r.status().is_client_error(), "{}", r.status());
    let t = r.text().await.unwrap();
    assert!(t.contains("has a scheme"), "{t}");
    let r = set(json!({
        "documents_fetch_sources": true,
        "documents_source_hosts": ["Docs.Example.", "0x7f.1:8897", "docs.example"]
    }))
    .await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let s = state.snapshot();
    assert!(s.settings.documents.fetch_sources);
    assert_eq!(
        s.settings.documents.source_hosts,
        ["docs.example", "127.0.0.1:8897"]
    );
    let caps: Value = gw
        .client()
        .post(format!("{gw}/api/op/doc_tools"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(caps["web"]["fetch"], true);
    assert_eq!(
        caps["web"]["hosts"],
        json!(["docs.example", "127.0.0.1:8897"])
    );
    // The fetch pool: the tool pool's size, and what one fetch may hold.
    assert_eq!(caps["web"]["pool"]["slots"], caps["pool"]["slots"]);
    assert_eq!(
        caps["web"]["pool"]["per_fetch_bytes"],
        u64::from(s.settings.max_body_mb) << 20
    );
}
