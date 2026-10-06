//! The external rows' `/props` cache (llama egress design §4.2, §9.3), against
//! wiremock servers.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::{LlamaRoute, Protocol, Route, ToolImages, Upstream, UpstreamKind};
use crate::egress::llama_cpp::props::LlamaFacts;

fn official() -> Value {
    json!({
        "default_generation_settings": {"params": {}, "n_ctx": 8192},
        "modalities": {"vision": true, "video": false, "audio": false},
        "chat_template_caps": {"supports_tools": true},
        "build_info": "b11226-0c6a6a7",
    })
}

fn router() -> Value {
    json!({
        "role": "router",
        "max_instances": 4,
        "models_autoload": true,
        "model_alias": "llama-server",
        "model_path": "none",
        "default_generation_settings": {"params": {}, "n_ctx": 0},
        "build_info": "b11226-0c6a6a7",
    })
}

fn row(id: i64, base_url: String) -> Upstream {
    Upstream {
        id,
        name: format!("ext-{id}"),
        protocol: Protocol::LlamaCpp,
        kind: UpstreamKind::LlamaServer,
        base_url,
        api_key: Some("sk-ext".into()),
        extra_headers: vec![("x-team".into(), "lab".into())],
        timeout_ms: 0,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

async fn server(answer: ResponseTemplate) -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(answer)
        .mount(&s)
        .await;
    s
}

async fn props_asked(s: &MockServer) -> usize {
    s.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/props")
        .count()
}

/// The first use answers unknown at once and asks in the background, with
/// the row's bearer and headers at the root; the answer is then kept and
/// never asked for again. A server that is no router serves one model
/// whatever a request names: one key for the server, whatever model a later
/// use names, and the body it was read from is not kept.
#[tokio::test]
async fn the_first_use_is_unknown_and_the_probe_runs_behind_it() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .and(header("authorization", "Bearer sk-ext"))
        .and(header("x-team", "lab"))
        .respond_with(ResponseTemplate::new(200).set_body_json(official()))
        .mount(&s)
        .await;
    let (cache, http, up) = (
        ExternalFacts::default(),
        reqwest::Client::new(),
        row(7, format!("{}/v1", s.uri())),
    );

    assert_eq!(cache.lookup(&http, &up, "m"), None, "never waits");
    assert!(cache.knows(7));
    cache.settled(7).await;

    let facts = cache
        .lookup(&http, &up, "m")
        .expect("read in the background");
    assert_eq!(facts.vision, Some(true));
    assert_eq!(facts.build_info.as_deref(), Some("b11226-0c6a6a7"));
    assert_eq!(
        facts.raw,
        Value::Null,
        "the template-sized body is not kept"
    );
    for other in ["m", "another-name", "a-third"] {
        assert_eq!(cache.lookup(&http, &up, other), Some(facts.clone()));
    }
    cache.settled(7).await;
    assert_eq!(props_asked(&s).await, 1, "a kept answer is not asked again");

    let shown = cache.view(7);
    assert_eq!(shown.len(), 1, "one entry for the server: {shown:?}");
    assert_eq!(
        (
            shown[0].model.as_str(),
            shown[0].router,
            shown[0].cached,
            shown[0].probing
        ),
        ("", false, true, false)
    );
    assert_eq!(shown[0].base_url, up.base());
    assert_eq!(shown[0].props.as_ref().unwrap().n_ctx_slot, Some(8192));
    assert!(shown[0].read_at.is_some());
}

/// A router is asked again about the model, without loading it; that it is a
/// router is kept, so its next model is asked about alone. Its models are
/// keys of their own, shown after the server.
#[tokio::test]
async fn a_router_is_asked_about_the_model() {
    let s = MockServer::start().await;
    for (model, ctx) in [("gemma", 8192), ("qwen", 32768)] {
        let mut facts = official();
        facts["default_generation_settings"]["n_ctx"] = json!(ctx);
        Mock::given(method("GET"))
            .and(path("/props"))
            .and(query_param("model", model))
            .and(query_param("autoload", "false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(facts))
            .with_priority(1)
            .mount(&s)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(router()))
        .with_priority(2)
        .mount(&s)
        .await;
    let (cache, http, up) = (
        ExternalFacts::default(),
        reqwest::Client::new(),
        row(7, format!("{}/v1", s.uri())),
    );
    cache.lookup(&http, &up, "gemma");
    cache.settled(7).await;
    let facts = cache
        .lookup(&http, &up, "gemma")
        .expect("the model's facts");
    assert_eq!(facts.n_ctx_slot, Some(8192));
    assert_eq!(facts.raw, Value::Null);

    assert_eq!(cache.lookup(&http, &up, "qwen"), None, "a model of its own");
    cache.settled(7).await;
    let qwen = cache.lookup(&http, &up, "qwen").expect("asked alone");
    assert_eq!(qwen.n_ctx_slot, Some(32768));
    let asked: Vec<Option<String>> = s
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.query().map(str::to_string))
        .collect();
    assert_eq!(
        asked,
        [
            None,
            Some("model=gemma&autoload=false".to_string()),
            Some("model=qwen&autoload=false".to_string()),
        ]
    );

    let shown = cache.view(7);
    let said: Vec<(&str, bool, Option<u64>)> = shown
        .iter()
        .map(|f| {
            let ctx = f.props.as_ref().and_then(|p| p.n_ctx_slot);
            (f.model.as_str(), f.router, ctx)
        })
        .collect();
    assert_eq!(
        said,
        [
            ("", true, None),
            ("gemma", false, Some(8192)),
            ("qwen", false, Some(32768))
        ]
    );
    let build = shown[0]
        .props
        .as_ref()
        .and_then(|p| p.build_info.as_deref());
    assert_eq!(build, Some("b11226-0c6a6a7"), "the router's own build");
    assert!(shown[0].cached && shown[0].unknown.is_none(), "{shown:?}");

    // An invalidation names the models the router was asked about.
    assert_eq!(cache.invalidate(7), ["gemma", "qwen"]);
}

/// "Model is not loaded" is never kept: shown, and asked again on the next
/// use, which finds it loaded — the router itself is not asked again.
#[tokio::test]
async fn a_model_the_router_has_not_loaded_is_asked_again() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .and(query_param("model", "gemma"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": 400, "message": "model is not loaded", "type": "invalid_request_error"}})))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .and(query_param("model", "gemma"))
        .respond_with(ResponseTemplate::new(200).set_body_json(official()))
        .with_priority(2)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(router()))
        .with_priority(3)
        .mount(&s)
        .await;
    let (cache, http, up) = (
        ExternalFacts::default(),
        reqwest::Client::new(),
        row(7, format!("{}/v1", s.uri())),
    );
    cache.lookup(&http, &up, "gemma");
    cache.settled(7).await;
    let shown = cache.view(7);
    assert_eq!(shown[1].model, "gemma", "{shown:?}");
    assert!(!shown[1].cached, "{shown:?}");
    assert!(
        shown[1].unknown.as_deref().unwrap().contains("not loaded"),
        "{shown:?}"
    );

    assert_eq!(cache.lookup(&http, &up, "gemma"), None);
    assert!(cache.view(7)[1].probing, "asked again");
    cache.settled(7).await;
    assert!(cache.lookup(&http, &up, "gemma").is_some());
    assert_eq!(props_asked(&s).await, 3, "the router once, the model twice");
}

/// A probe that reaches nothing is never kept either.
#[tokio::test]
async fn a_server_that_does_not_answer_is_asked_again() {
    // A port that was just closed (not a dropped MockServer, which wiremock
    // pools and hands to the next test).
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", closed.local_addr().unwrap());
    drop(closed);
    let (cache, http, up) = (
        ExternalFacts::default(),
        reqwest::Client::new(),
        row(7, base),
    );
    cache.lookup(&http, &up, "m");
    cache.settled(7).await;
    let shown = cache.view(7);
    assert!(!shown[0].cached && !shown[0].probing, "{shown:?}");
    assert!(
        shown[0]
            .unknown
            .as_deref()
            .unwrap()
            .starts_with("GET /props reached no server"),
        "{shown:?}"
    );
    cache.lookup(&http, &up, "m");
    assert!(cache.view(7)[0].probing, "asked again on the next use");
    cache.settled(7).await;
}

/// The probe is bounded by the row's own request timeout: a server that does
/// not answer within it is a probe that reached nothing.
#[tokio::test]
async fn the_probe_is_bounded_by_the_rows_timeout() {
    let s = server(
        ResponseTemplate::new(200)
            .set_body_json(official())
            .set_delay(Duration::from_secs(5)),
    )
    .await;
    let mut up = row(7, format!("{}/v1", s.uri()));
    up.timeout_ms = 100;
    let (cache, http) = (ExternalFacts::default(), reqwest::Client::new());
    cache.lookup(&http, &up, "m");
    cache.settled(7).await;
    let shown = cache.view(7);
    assert!(!shown[0].cached, "{shown:?}");
}

/// A server that answers without facts — an old build's 404, a 401 — is
/// kept as unknown with its answer, and not asked again.
#[tokio::test]
async fn an_answer_without_facts_is_kept_as_unknown_with_the_answer() {
    for (status, body, says) in [
        (404, "File Not Found", "HTTP 404: File Not Found"),
        (401, "Invalid API Key", "HTTP 401: Invalid API Key"),
    ] {
        let s = server(ResponseTemplate::new(status).set_body_string(body)).await;
        let (cache, http, up) = (
            ExternalFacts::default(),
            reqwest::Client::new(),
            row(7, format!("{}/v1", s.uri())),
        );
        cache.lookup(&http, &up, "m");
        cache.settled(7).await;
        assert_eq!(cache.lookup(&http, &up, "m"), None);
        cache.settled(7).await;
        assert_eq!(props_asked(&s).await, 1, "kept: {status}");
        let shown = cache.view(7);
        assert!(shown[0].cached && shown[0].props.is_none(), "{shown:?}");
        assert!(
            shown[0].unknown.as_deref().unwrap().contains(says),
            "{shown:?}"
        );
    }
}

/// A server error is never kept: llama-server's 503 "Loading model" (a
/// router autoloading on the first send, a server just restarted) and a
/// reverse proxy's 502 or 504 are shown, and asked again on the next use.
#[tokio::test]
async fn a_server_error_is_asked_again() {
    let loading = json!({"error": {"code": 503, "message": "Loading model",
        "type": "unavailable_error"}});
    for (status, says) in [
        (503, "HTTP 503: {\"error\""),
        (502, "HTTP 502: Bad Gateway"),
        (504, "HTTP 504: Gateway Timeout"),
    ] {
        let answer = match status {
            503 => ResponseTemplate::new(503).set_body_json(&loading),
            502 => ResponseTemplate::new(502).set_body_string("Bad Gateway"),
            _ => ResponseTemplate::new(504).set_body_string("Gateway Timeout"),
        };
        let s = server(answer).await;
        let (cache, http, up) = (
            ExternalFacts::default(),
            reqwest::Client::new(),
            row(7, format!("{}/v1", s.uri())),
        );
        cache.lookup(&http, &up, "m");
        cache.settled(7).await;
        let shown = cache.view(7);
        assert!(!shown[0].cached && !shown[0].probing, "{status}: {shown:?}");
        let why = shown[0].unknown.as_deref().unwrap();
        assert!(
            why.contains(says) && why.ends_with("asked again on next use"),
            "{why}"
        );

        assert_eq!(cache.lookup(&http, &up, "m"), None);
        assert!(cache.view(7)[0].probing, "{status}: asked again");
        cache.settled(7).await;
        assert_eq!(props_asked(&s).await, 2, "{status}");
    }
}

/// The server's key is the row and its base URL, never a model: a row
/// pointed elsewhere never reads the old server's facts, and every model
/// name on one server reads the same.
#[tokio::test]
async fn the_key_is_the_row_and_its_base_url() {
    let s = server(ResponseTemplate::new(200).set_body_json(official())).await;
    let (cache, http) = (ExternalFacts::default(), reqwest::Client::new());
    let up = row(7, format!("{}/v1", s.uri()));
    cache.lookup(&http, &up, "a");
    cache.settled(7).await;
    assert!(cache.lookup(&http, &up, "a").is_some());
    assert!(cache.lookup(&http, &up, "b").is_some(), "another model");
    let moved = row(7, format!("{}/v2", s.uri()));
    assert!(cache.lookup(&http, &moved, "a").is_none(), "another base");
    assert!(cache
        .lookup(&http, &row(8, up.base_url.clone()), "a")
        .is_none());
    cache.settled(7).await;
    cache.settled(8).await;
    assert_eq!(cache.view(7).len(), 2);
    assert_eq!(cache.view(8).len(), 1);
}

/// One probe per row at a time: a router's models wait for its own answer
/// and each other's; another row is asked beside it.
#[tokio::test]
async fn a_row_is_asked_one_probe_at_a_time() {
    let (now, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let app = {
        let (now, most) = (now.clone(), most.clone());
        axum::Router::new().route(
            "/props",
            axum::routing::get(
                move |q: axum::extract::Query<std::collections::HashMap<String, String>>| {
                    let (now, most) = (now.clone(), most.clone());
                    async move {
                        let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(n, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        now.fetch_sub(1, Ordering::SeqCst);
                        axum::Json(match q.contains_key("model") {
                            true => official(),
                            false => router(),
                        })
                    }
                },
            ),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let (cache, http) = (ExternalFacts::default(), reqwest::Client::new());
    let up = row(7, format!("http://{addr}/v1"));
    for model in ["a", "b", "c"] {
        cache.lookup(&http, &up, model);
    }
    let shown = cache.view(7);
    assert_eq!(shown.len(), 1, "the server first: {shown:?}");
    assert!(shown[0].probing, "{shown:?}");
    cache.settled(7).await;
    assert_eq!(most.load(Ordering::SeqCst), 1, "one at a time per row");
    for model in ["a", "b", "c"] {
        assert!(cache.lookup(&http, &up, model).is_some(), "{model}");
    }
    assert_eq!(cache.view(7).len(), 4, "the router and its three models");

    // Two rows are asked side by side.
    most.store(0, Ordering::SeqCst);
    let other = row(8, up.base_url.clone());
    cache.lookup(&http, &row(9, up.base_url.clone()), "a");
    cache.lookup(&http, &other, "a");
    cache.settled(9).await;
    cache.settled(8).await;
    assert_eq!(most.load(Ordering::SeqCst), 2, "rows are independent");
}

/// An invalidation drops what the row said, and an answer still on its way
/// is not stored; the next use asks again.
#[tokio::test]
async fn an_invalidation_drops_the_row_and_an_answer_in_flight() {
    let s = server(
        ResponseTemplate::new(200)
            .set_body_json(official())
            .set_delay(Duration::from_millis(200)),
    )
    .await;
    let (cache, http, up) = (
        ExternalFacts::default(),
        reqwest::Client::new(),
        row(7, format!("{}/v1", s.uri())),
    );
    cache.lookup(&http, &up, "a");
    cache.settled(7).await;
    assert!(cache.lookup(&http, &up, "a").is_some());
    assert!(cache.invalidate(7).is_empty(), "no router: no models");
    assert!(!cache.knows(7));

    cache.lookup(&http, &up, "b");
    assert_eq!(cache.invalidate(7), ["b"], "the model it was asked about");
    cache.settled(7).await;
    assert!(!cache.knows(7), "{:?}", cache.view(7));
    assert!(cache.view(7).is_empty());

    assert!(cache.lookup(&http, &up, "a").is_none());
    cache.settled(7).await;
    assert!(cache.lookup(&http, &up, "a").is_some());
    // Another row is not touched.
    assert!(cache.invalidate(8).is_empty());
    assert!(cache.knows(7));
}

/// A server that accepts and never answers, under a row with no deadline
/// (`timeout_ms = 0`), holds the row's probing only until the next
/// invalidation: an edit, or Test, aborts the probe, and the row is asked
/// again at once rather than behind it.
#[tokio::test]
async fn an_invalidation_aborts_a_probe_that_never_answers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (accepted, mut probes) = tokio::sync::mpsc::unbounded_channel();
    let mute = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            open.push(socket);
            let _ = accepted.send(());
        }
    });
    async fn went_out(probes: &mut tokio::sync::mpsc::UnboundedReceiver<()>) {
        tokio::time::timeout(Duration::from_secs(10), probes.recv())
            .await
            .expect("the probe went out");
    }
    let s = server(ResponseTemplate::new(200).set_body_json(official())).await;
    let (cache, http) = (ExternalFacts::default(), reqwest::Client::new());
    let hung = row(7, format!("http://{addr}/v1"));
    assert_eq!(hung.request_timeout(), None, "no deadline of lmgw's own");

    cache.lookup(&http, &hung, "m");
    went_out(&mut probes).await;
    assert!(cache.view(7)[0].probing);
    // Test asks the same server again, at once: not behind the hung probe.
    cache.retest(&http, &hung, []);
    went_out(&mut probes).await;
    let shown = cache.view(7);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(shown[0].probing && shown[0].read_at.is_none(), "{shown:?}");

    // An edit points the row at a server that answers.
    assert_eq!(
        cache.invalidate(7),
        ["m"],
        "still waiting for the server's answer"
    );
    assert!(!cache.knows(7));
    let moved = row(7, format!("{}/v1", s.uri()));
    assert_eq!(cache.lookup(&http, &moved, "m"), None);
    tokio::time::timeout(Duration::from_secs(10), cache.settled(7))
        .await
        .expect("the hung probe no longer holds the row");
    assert!(cache.lookup(&http, &moved, "m").is_some());
    assert_eq!(
        cache.view(7).len(),
        1,
        "the hung server's probes left nothing"
    );
    mute.abort();
}

/// The Test button asks the row's server again, now — a row never asked
/// before too — and a router about the models it is given and those it had
/// been asked about; on a row of another protocol it asks nothing.
#[tokio::test]
async fn a_retest_asks_the_server_now() {
    let s = server(ResponseTemplate::new(200).set_body_json(official())).await;
    let (cache, http, up) = (
        ExternalFacts::default(),
        reqwest::Client::new(),
        row(7, format!("{}/v1", s.uri())),
    );
    cache.retest(&http, &up, ["aliased".to_string()]);
    let shown = cache.view(7);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(shown[0].probing && shown[0].model.is_empty(), "{shown:?}");
    cache.settled(7).await;
    let first = cache.view(7)[0].read_at;
    assert!(cache.view(7)[0].props.is_some(), "read on Test alone");
    assert_eq!(
        props_asked(&s).await,
        1,
        "no router: the models are not asked"
    );

    cache.retest(&http, &up, []);
    assert!(cache.view(7)[0].probing);
    cache.settled(7).await;
    assert_eq!(props_asked(&s).await, 2);
    assert!(cache.view(7)[0].read_at >= first);

    let mut openai = up.clone();
    openai.protocol = Protocol::Openai;
    cache.retest(&http, &openai, []);
    assert!(!cache.knows(7));
    cache.settled(7).await;
    assert_eq!(props_asked(&s).await, 2);
}

/// Test on a router asks it about the models it is given and those it had
/// been asked about, each once.
#[tokio::test]
async fn a_retest_asks_a_router_about_its_models() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .and(query_param("autoload", "false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(official()))
        .with_priority(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(router()))
        .with_priority(2)
        .mount(&s)
        .await;
    let (cache, http, up) = (
        ExternalFacts::default(),
        reqwest::Client::new(),
        row(7, format!("{}/v1", s.uri())),
    );
    cache.lookup(&http, &up, "used");
    cache.settled(7).await;
    let models = ["aliased".to_string(), "used".to_string()];
    cache.retest(&http, &up, models);
    cache.settled(7).await;
    let shown: Vec<(String, bool)> = cache
        .view(7)
        .into_iter()
        .map(|f| (f.model, f.props.is_some_and(|p| p.n_ctx_slot.is_some())))
        .collect();
    assert_eq!(
        shown,
        [
            (String::new(), false),
            ("aliased".into(), true),
            ("used".into(), true)
        ]
    );
    let mut asked: Vec<Option<String>> = s
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.query().map(str::to_string))
        .collect();
    asked.sort();
    assert_eq!(
        asked,
        [
            None,
            None,
            Some("model=aliased&autoload=false".to_string()),
            Some("model=used&autoload=false".to_string()),
            Some("model=used&autoload=false".to_string()),
        ]
    );
}

/// A media refusal names its medium; any other error is none.
#[test]
fn media_refusals_are_read_from_the_error_body() {
    let refusal = |m: &str| {
        json!({"error": {"code": 500, "type": "server_error", "message": format!(
            "{m} input is not supported - hint: if this is unexpected, you may need to provide \
             the mmproj")}})
        .to_string()
    };
    for m in ["image", "audio", "video"] {
        assert_eq!(media_refusal(refusal(m).as_bytes()), Some(m));
    }
    assert_eq!(
        media_refusal(b"Image Input Is Not Supported"),
        Some("image")
    );
    for other in [
        r#"{"error":{"message":"request (5000 tokens) exceeds the available context size"}}"#,
        r#"{"error":{"message":"model is not loaded"}}"#,
        "",
    ] {
        assert_eq!(media_refusal(other.as_bytes()), None, "{other}");
    }
}

/// The single endpoint writer moves a route to a container and carries what
/// was decided for it unchanged (§3.2).
#[test]
fn pointing_a_route_keeps_its_llama_decision() {
    let decided = Arc::new(LlamaRoute {
        facts: Arc::new(LlamaFacts {
            vision: Some(true),
            ..Default::default()
        }),
        tool_images: ToolImages::Refused("no per-image bound".into()),
    });
    let mut route = Route {
        upstream: Upstream {
            llama: Some(decided.clone()),
            ..row(
                crate::config::ROUTER_UPSTREAM_ID,
                "http://127.0.0.1:0/v1".into(),
            )
        },
        upstream_model: "seer".into(),
        param_defaults: Default::default(),
        fallback: None,
    };
    crate::vram::LocalHold::point_at(&mut route, 4321);
    assert_eq!(route.upstream.base_url, "http://127.0.0.1:4321/v1");
    let kept = route.upstream.llama.as_ref().expect("carried");
    assert!(Arc::ptr_eq(kept, &decided), "the same decision, not a copy");
    assert_eq!(kept.tool_images.refusal(), Some("no per-image bound"));
    assert!(!kept.tool_images.allowed());
}

/// Only a stored `llama_cpp` row is external; the synthetic upstreams of
/// managed models never are.
#[test]
fn only_stored_llama_rows_are_external() {
    assert!(is_external_llama(&row(7, String::new())));
    assert!(!is_external_llama(&row(
        crate::config::ROUTER_UPSTREAM_ID,
        String::new()
    )));
    let mut openai = row(7, String::new());
    openai.protocol = Protocol::Openai;
    assert!(!is_external_llama(&openai));
}
