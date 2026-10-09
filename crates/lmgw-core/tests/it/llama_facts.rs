//! What a chat send knows about its llama-server (llama egress design §3.2,
//! §4.2, §9.3): the resolver on a managed row (`gpu_world`'s containers) and
//! on an external `llama_cpp` row (wiremock), and the external cache's four
//! invalidations through the paths that make them — an edit on both write
//! paths, a transport failure and a media refusal on a chat send, and the
//! row's Test button. The cache itself (background probing, the router
//! re-probe, what is kept) is unit-tested beside it
//! (`src/llama_facts/tests.rs`).

use lmgw_core::config::{LlamaParams, Protocol, Route, UpstreamKind};
use lmgw_core::llama_facts::{self, ServerFacts};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewLocalModel, NewUpstream};
use lmgw_core::vram;
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::chat_actions::post;
use crate::common::{serve, Gw};
use crate::support::gpu_world::{Gpu, GIB};

fn official() -> Value {
    json!({
        "default_generation_settings": {"params": {}, "n_ctx": 8192},
        "modalities": {"vision": true, "video": false, "audio": false},
        "build_info": "b11226-0c6a6a7",
    })
}

fn completion() -> Value {
    json!({
        "id": "c", "object": "chat.completion", "created": 0, "model": "gguf",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "Hello"}}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 1, "total_tokens": 8},
    })
}

// ---------------------------------------------------------------------------
// An external row
// ---------------------------------------------------------------------------

struct External {
    state: SharedState,
    gw: Gw,
    mock: MockServer,
    id: i64,
}

impl External {
    /// A gateway with one `llama_cpp` row on a wiremock server that serves
    /// `/props`, and the alias `m` on it; `chat` answers its chat requests.
    async fn new(protocol: Protocol, kind: UpstreamKind, chat: ResponseTemplate) -> Self {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(200).set_body_json(official()))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(chat)
            .mount(&mock)
            .await;
        // What the Test button asks.
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"object": "list", "data": [{"id": "chat-gguf"}]})),
            )
            .mount(&mock)
            .await;
        let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
        let id = store::insert_upstream(
            &state.db,
            &NewUpstream {
                name: "llama-ext".into(),
                protocol,
                kind,
                base_url: format!("{}/v1", mock.uri()),
                api_key: Some("sk-ext".into()),
                extra_headers: vec![],
                timeout_ms: 30_000,
                enabled: true,
                expose_all: false,
                expose_prefix: String::new(),
                supports_responses: false,
            },
        )
        .await
        .unwrap();
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: "m".into(),
                upstream_id: id,
                upstream_model_id: "chat-gguf".into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: None,
            },
        )
        .await
        .unwrap();
        state.reload_snapshot().await.unwrap();
        let gw = serve(state.clone()).await;
        Self {
            state,
            gw,
            mock,
            id,
        }
    }

    async fn llama() -> Self {
        Self::new(
            Protocol::LlamaCpp,
            UpstreamKind::LlamaServer,
            ResponseTemplate::new(200).set_body_json(completion()),
        )
        .await
    }

    fn route(&self) -> Route {
        self.state.snapshot().resolve("m").unwrap()
    }

    /// The resolver for an unheld send on `m`.
    fn resolve(&self) -> ServerFacts {
        llama_facts::resolve(&self.state, None, &self.route())
    }

    /// Resolve once and let the background probe answer: the row's facts
    /// are known from here on.
    async fn known(&self) {
        assert_eq!(self.resolve(), ServerFacts::default(), "never waits");
        self.state.llama_facts.settled(self.id).await;
        assert!(self.resolve().facts.is_some(), "read in the background");
    }

    fn knows(&self) -> bool {
        self.state.llama_facts.knows(self.id)
    }

    async fn props_asked(&self) -> usize {
        self.mock
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/props")
            .count()
    }

    async fn chat(&self) -> (u16, String) {
        let r = post(
            &self.gw,
            "/v1/chat/completions",
            json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        let status = r.status().as_u16();
        (status, r.text().await.unwrap())
    }

    async fn op(&self, name: &str, args: Value) -> Value {
        let r = post(&self.gw, &format!("/api/op/{name}"), args).await;
        assert!(r.status().is_success(), "{name}: {}", r.status());
        r.json().await.unwrap()
    }
}

/// An external row's facts come from the background cache: unknown at
/// first, without waiting, then what the server said. It has no projector
/// advisory, which only a managed row's start can read.
#[tokio::test]
async fn the_resolver_reads_an_external_row_from_the_cache() {
    let w = External::llama().await;
    w.known().await;
    let got = w.resolve();
    let facts = got.facts.expect("known");
    assert_eq!(facts.vision, Some(true));
    assert_eq!(facts.build_info.as_deref(), Some("b11226-0c6a6a7"));
    assert_eq!(got.ubatch_advisory, None);
    assert_eq!(w.props_asked().await, 1);

    // Shown on both surfaces: one entry for a server that is no router.
    let listed = crate::chat_actions::get_json(&w.gw, "/api/upstreams").await;
    let shown = &listed["upstreams"][0]["llama_facts"];
    assert_eq!(shown.as_array().map(Vec::len), Some(1), "{listed}");
    assert_eq!(shown[0]["model"], "", "the server's, not a model's");
    assert_eq!(shown[0]["router"], false);
    assert_eq!(shown[0]["props"]["build_info"], "b11226-0c6a6a7");
    assert_eq!(shown[0]["props"]["n_ctx_slot"], 8192);
    assert_eq!(shown[0]["cached"], true);
    assert!(shown[0]["read_at"].is_i64(), "{shown}");
    let mcp = lmgw_core::ops::upstreams(&w.state).await.unwrap();
    assert_eq!(mcp["upstreams"][0]["llama_facts"], *shown);
}

/// A row of another protocol is never asked, and has nothing to show.
#[tokio::test]
async fn the_resolver_asks_nothing_of_another_protocol() {
    let w = External::new(
        Protocol::Openai,
        UpstreamKind::Generic,
        ResponseTemplate::new(200).set_body_json(completion()),
    )
    .await;
    assert_eq!(w.resolve(), ServerFacts::default());
    w.state.llama_facts.settled(w.id).await;
    assert!(!w.knows());
    assert_eq!(w.props_asked().await, 0);
    let listed = lmgw_core::ops::upstreams(&w.state).await.unwrap();
    assert!(
        listed["upstreams"][0].get("llama_facts").is_none(),
        "{listed}"
    );
}

/// An edit of the row drops its facts, on the dashboard's write path and on
/// the tool plane's, and the next use asks again.
#[tokio::test]
async fn an_edit_drops_the_rows_facts() {
    let w = External::llama().await;
    w.known().await;
    w.op(
        "upstream_set_full",
        json!({"action": "update", "id": w.id, "timeout_ms": 20_000}),
    )
    .await;
    assert!(!w.knows(), "the dashboard's edit");
    assert!(w.resolve().facts.is_none());
    w.state.llama_facts.settled(w.id).await;
    assert!(w.resolve().facts.is_some());

    w.op(
        "upstream_set",
        json!({"action": "update", "id": w.id, "name": "llama-ext-2"}),
    )
    .await;
    assert!(!w.knows(), "the tool plane's edit");
    w.known().await;
    w.op("upstream_set", json!({"action": "disable", "id": w.id}))
        .await;
    assert!(!w.knows(), "disable is an edit");
    assert_eq!(w.props_asked().await, 3);
}

/// A transport failure on a chat send drops the row's facts: the server
/// went away and may come back as another build.
#[tokio::test]
async fn a_transport_failure_drops_the_rows_facts() {
    let w = External::llama().await;
    w.known().await;
    // Point the row at a port nothing answers on, keeping what the cache
    // knows (an edit would drop it on its own).
    let (port, _held) = crate::common::refusing_port();
    let dead = format!("http://127.0.0.1:{port}/v1");
    sqlx::query("UPDATE upstreams SET base_url = ? WHERE id = ?")
        .bind(&dead)
        .bind(w.id)
        .execute(&w.state.db)
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    assert!(w.knows());

    let (status, body) = w.chat().await;
    assert_eq!(status, 502, "{body}");
    assert!(!w.knows(), "{:?}", w.state.llama_facts.view(w.id));
}

/// A media refusal drops the row's facts, and the client gets what it gets
/// from a send that is not watched. Any other error leaves them.
///
/// Since every chat send asks the resolver (tool images, llama egress design
/// §8.2), a send on an unknown row starts the background probe itself, so
/// whether the send after a refusal is watched is a race with that probe:
/// the client gets the same answer either way, which is the point.
#[tokio::test]
async fn a_media_refusal_drops_the_rows_facts() {
    let refusal = json!({"error": {"code": 500, "type": "server_error", "message":
        "image input is not supported - hint: if this is unexpected, you may need to provide \
         the mmproj"}});
    let w = External::new(
        Protocol::LlamaCpp,
        UpstreamKind::LlamaServer,
        ResponseTemplate::new(500).set_body_json(refusal),
    )
    .await;
    w.known().await;
    let watched = w.chat().await;
    assert_eq!(watched.0, 502, "{}", watched.1);
    assert!(watched.1.contains("image input is not supported"));
    assert!(!w.knows());
    assert_eq!(w.chat().await, watched, "relayed as it came");

    let other = json!({"error": {"code": 400, "type": "invalid_request_error",
        "message": "Unable to parse the grammar"}});
    let w = External::new(
        Protocol::LlamaCpp,
        UpstreamKind::LlamaServer,
        ResponseTemplate::new(400).set_body_json(other),
    )
    .await;
    w.known().await;
    let watched = w.chat().await;
    assert_eq!(watched.0, 400, "{}", watched.1);
    assert!(watched.1.contains("Unable to parse the grammar"));
    assert!(w.knows(), "not a media refusal");
    assert!(w.resolve().facts.is_some());
    assert_eq!(w.chat().await, watched, "relayed as it came");
}

/// A raw server in front of which a chat answer's body breaks off: `/props`
/// answers the official facts, and every chat request a 500 that promises
/// more body than it sends — cut in the middle of a media refusal. Its base
/// URL.
async fn breaking_server() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                // The whole request first: one closed with bytes unread is a
                // reset, not a broken body.
                let mut got = Vec::new();
                let mut buf = [0u8; 8192];
                let (head, length) = loop {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    got.extend_from_slice(&buf[..n]);
                    if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&got[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (head, end + 4 + length);
                    }
                };
                while got.len() < length {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    got.extend_from_slice(&buf[..n]);
                }
                let answer = match head.starts_with("get /props") {
                    true => {
                        let body = official().to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    false => "HTTP/1.1 500 Internal Server Error\r\n\
                              content-type: application/json\r\ncontent-length: 400\r\n\
                              connection: close\r\n\r\n{\"error\":{\"code\":500,\
                              \"message\":\"image input is not supp"
                        .to_string(),
                };
                let _ = socket.write_all(answer.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{addr}/v1")
}

/// A watched send whose error body breaks off reaches the client as an
/// unwatched one does: the media check is skipped (what was read of the body
/// is no answer), and the caller meets the broken body itself — a buffered
/// send as a failed request, a streaming one as the status with no body.
#[tokio::test]
async fn an_error_body_that_breaks_off_is_relayed_as_it_came() {
    let w = External::llama().await;
    w.known().await;
    // Point the row at the breaking server, keeping what the cache knows
    // (an edit would drop it), and put an `openai` row there — a send
    // nobody watches, through the same error mapping.
    let base = breaking_server().await;
    sqlx::query("UPDATE upstreams SET base_url = ? WHERE id = ?")
        .bind(&base)
        .bind(w.id)
        .execute(&w.state.db)
        .await
        .unwrap();
    let plain = store::insert_upstream(
        &w.state.db,
        &NewUpstream {
            name: "plain".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: base,
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &w.state.db,
        &NewAlias {
            alias: "plain".into(),
            upstream_id: plain,
            upstream_model_id: "chat-gguf".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    // The URL a buffered send's transport error names is the one thing a
    // rebuilt response does not carry.
    let unurled = |text: String| match text.find(" for url (") {
        Some(at) => text[..at].to_string(),
        None => text,
    };

    let gw = &w.gw;
    for stream in [false, true] {
        let send = |model: &str| {
            let body = json!({"model": model, "stream": stream,
                              "messages": [{"role": "user", "content": "hi"}]});
            async move {
                let r = post(gw, "/v1/chat/completions", body).await;
                let status = r.status().as_u16();
                (status, unurled(r.text().await.unwrap()))
            }
        };
        let watched = send("m").await;
        assert!(w.knows(), "stream {stream}: no media refusal was read");
        assert_eq!(watched, send("plain").await, "stream {stream}");
        assert!(!watched.1.contains("image input"), "{watched:?}");
    }
}

/// The first chat send on an unknown external row goes out at once with
/// today's bytes (decision 14), and asks the server in the background: the
/// send after it knows the facts.
#[tokio::test]
async fn the_first_send_asks_the_server_in_the_background() {
    let w = External::llama().await;
    assert!(!w.knows());
    assert_eq!(w.chat().await.0, 200);
    w.state.llama_facts.settled(w.id).await;
    assert!(w.knows());
    assert!(w.resolve().facts.is_some());
    assert_eq!(w.props_asked().await, 1);
}

/// The row's Test button asks its server again, at once.
#[tokio::test]
async fn the_test_button_asks_the_row_again() {
    let w = External::llama().await;
    w.known().await;
    let before = w.state.llama_facts.view(w.id)[0].read_at;
    w.op("upstream_test", json!({"id": w.id})).await;
    w.state.llama_facts.settled(w.id).await;
    assert_eq!(w.props_asked().await, 2);
    let shown = w.state.llama_facts.view(w.id);
    assert_eq!(shown.len(), 1);
    assert!(shown[0].props.is_some() && shown[0].read_at >= before);
}

/// Test reads a row no chat request has made lmgw ask yet: its server at
/// once, and — a router — the model its alias names.
#[tokio::test]
async fn the_test_button_reads_a_row_never_asked() {
    let w = External::llama().await;
    assert!(!w.knows());
    w.op("upstream_test", json!({"id": w.id})).await;
    w.state.llama_facts.settled(w.id).await;
    let shown = w.state.llama_facts.view(w.id);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(shown[0].props.is_some(), "{shown:?}");
    assert!(w.resolve().facts.is_some());
    assert_eq!(w.props_asked().await, 1);

    let w = External::llama().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .and(query_param("autoload", "false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(official()))
        .with_priority(1)
        .mount(&w.mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "role": "router", "build_info": "b11226-0c6a6a7",
            "default_generation_settings": {"params": {}, "n_ctx": 0}})))
        .with_priority(2)
        .mount(&w.mock)
        .await;
    w.op("upstream_test", json!({"id": w.id})).await;
    w.state.llama_facts.settled(w.id).await;
    let shown: Vec<(String, bool, bool)> = w
        .state
        .llama_facts
        .view(w.id)
        .into_iter()
        .map(|f| {
            (
                f.model,
                f.router,
                f.props.is_some_and(|p| p.vision.is_some()),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            (String::new(), true, false),
            ("chat-gguf".to_string(), false, true)
        ]
    );
    assert!(w.resolve().facts.is_some(), "the alias's model is known");
}

// ---------------------------------------------------------------------------
// A managed row
// ---------------------------------------------------------------------------

/// A managed row's facts come from its container's registry entry through
/// the request's hold, with the started row's projector advisory; the
/// external cache is never asked.
#[tokio::test]
async fn the_resolver_reads_a_managed_row_through_its_hold() {
    let gpu = Gpu::new(24 * GIB, 1, 5).await;
    gpu.model("seer", GIB).await;
    gpu.world().props.insert("seer".into(), official());
    let route = gpu.route("seer");
    let hold = vram::admit(&gpu.state, &route, "seer")
        .await
        .unwrap()
        .expect("a local model");
    let got = llama_facts::resolve(&gpu.state, Some(&hold), &route);
    assert_eq!(got.facts.as_ref().and_then(|f| f.n_ctx_slot), Some(8192));
    assert_eq!(got.ubatch_advisory, None, "no projector");
    // Unheld, the synthetic upstream is nobody's to probe.
    assert_eq!(
        llama_facts::resolve(&gpu.state, None, &route),
        ServerFacts::default()
    );
    assert!(!gpu.state.llama_facts.knows(route.upstream.id));
}

/// A row that loads a projector lmgw cannot read carries its advisory to the
/// resolver; a container whose `/props` could not be read has no facts.
#[tokio::test]
async fn the_resolver_carries_a_managed_rows_advisory() {
    let gpu = Gpu::new(24 * GIB, 1, 5).await;
    gpu.file("seer-mmproj.gguf", 1024);
    gpu.row(
        NewLocalModel {
            model_id: "seer".into(),
            gguf_path: "seer.gguf".into(),
            params: LlamaParams {
                mmproj_path: Some("seer-mmproj.gguf".into()),
                ..Default::default()
            },
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
        GIB,
    )
    .await;
    // No `/props` served: the facts are unknown, the advisory still known.
    let route = gpu.route("seer");
    let hold = vram::admit(&gpu.state, &route, "seer")
        .await
        .unwrap()
        .expect("a local model");
    let got = llama_facts::resolve(&gpu.state, Some(&hold), &route);
    assert!(got.facts.is_none());
    let advisory = got.ubatch_advisory.expect("the projector cannot be read");
    assert!(advisory.contains("non-causal"), "{advisory}");
}
