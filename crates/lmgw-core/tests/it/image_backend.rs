//! The `/v1/images/*` routes end to end, with wiremock standing in for
//! sd-server and for a cloud image provider (image-generation design §6).
//!
//! The audio family's tests are the template, because the routes are: alias
//! resolution, the model rewrite, admission held to the last byte, one log
//! row, one error shape. What is new here is the pair of guards §6 adds —
//! `/v1/images/edits` is refused outright for a row that is not an edit
//! pipeline (sd-server *crashes* on that request, §12.8), and neither route
//! accepts a model that cannot serve it — and sd-server's own two error
//! bodies, which are not OpenAI-shaped.

use std::sync::Arc;

use lmgw_core::config::{hash_api_key, KeyPolicy, ScopeMode, Settings};
use lmgw_core::runtime::registry::Registry;
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewImageModel};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn image_row(model_id: &str) -> NewImageModel {
    let mut files = serde_json::Map::new();
    files.insert(
        "diffusion_model".into(),
        json!(format!("leejet/{model_id}/weights.gguf")),
    );
    NewImageModel {
        model_id: model_id.into(),
        files,
        args: serde_json::Map::new(),
        modes: vec![],
        edit: false,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        idle_seconds: 0,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
    }
}

/// A `podman` that agrees to everything and starts nothing — the "container"
/// is the wiremock the port allocator points at.
struct FakePodman;

#[async_trait::async_trait]
impl lmgw_core::runtime::registry::CommandRunner for FakePodman {
    async fn run(
        &self,
        _program: &str,
        _args: &[String],
    ) -> std::io::Result<lmgw_core::runtime::registry::CmdOutput> {
        Ok(lmgw_core::runtime::registry::CmdOutput {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// sd-server's readiness route, which doubles as its capabilities probe (§3).
async fn mount_ready(mock: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "supported_modes": ["img_gen"],
            "current_mode": "img_gen",
            "limits": {"min_width": 64, "max_width": 4096},
            "samplers": ["euler"],
        })))
        .mount(mock)
        .await;
}

fn serve(state: &SharedState) -> String {
    let app = build_router(state.clone());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// An image models dir inside the state's own test dir, which goes with
/// the state: a kept `tempdir()` stayed behind in /tmp, which is RAM.
fn models_dir(state: &SharedState) -> String {
    let dir = state.data_dir.join("image-models");
    std::fs::create_dir_all(&dir).unwrap();
    dir.display().to_string()
}

/// A gateway whose image rows' containers *are* `mock`: a fake podman that
/// starts nothing and a port allocator that hands out the mock's port.
async fn setup_local(mock: &MockServer, rows: &[NewImageModel]) -> (SharedState, String) {
    let state = AppState::init_for_tests().await.unwrap();
    let port = mock.address().port();
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        Arc::new(FakePodman),
        reqwest::Client::new(),
        Arc::new(move || Ok(port)),
    )));
    let mut s = Settings::default();
    s.vram.load_timeout_seconds = 5;
    // The start refuses a class with no models dir before it renders any argv;
    // the fake podman mounts nothing, so only the path's existence matters.
    s.image.models_dir = models_dir(&state);
    // Never the dev/prod prefix: these tests only ever label a fake container,
    // but the name is what a real `podman rm` would collide with.
    s.container_prefix = "lmgwtest".into();
    store::save_settings(&state.db, &s).await.unwrap();
    for row in rows {
        store::insert_image_model(&state.db, row).await.unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let base = serve(&state);
    (state, base)
}

/// A gateway with one *cloud* upstream (generic, openai protocol) whose
/// catalog advertises an image generator and an edit model, plus an alias onto
/// each — the passthrough half of §6.
async fn setup_cloud(mock: &MockServer) -> (SharedState, String) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {
                "id": "flux-1.1-pro",
                "architecture": {"input_modalities": ["text"], "output_modalities": ["image"]},
            },
            {
                "id": "flux-kontext",
                "architecture": {
                    "input_modalities": ["text", "image"],
                    "output_modalities": ["image"],
                },
            },
            {
                "id": "gpt-5",
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
            },
        ]})))
        .mount(mock)
        .await;

    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &store::NewUpstream {
            name: "bfl".into(),
            protocol: lmgw_core::config::Protocol::Openai,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: format!("{}/v1", mock.uri().trim_end_matches('/')),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    for (alias, model) in [
        ("my-image", "flux-1.1-pro"),
        ("my-edit", "flux-kontext"),
        ("my-chat", "gpt-5"),
    ] {
        store::insert_alias(
            &state.db,
            &store::NewAlias {
                alias: alias.into(),
                upstream_id: up_id,
                upstream_model_id: model.into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: None,
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let base = serve(&state);
    (state, base)
}

/// wiremock matcher for multipart bodies (the boundary makes exact matching
/// impossible): assert the re-encoded form contains a fragment.
struct BodyContains(&'static str);
impl wiremock::Match for BodyContains {
    fn matches(&self, request: &wiremock::Request) -> bool {
        String::from_utf8_lossy(&request.body).contains(self.0)
    }
}

fn one_png() -> Value {
    json!({"created": 1, "output_format": "png", "data": [{"b64_json": "aVZCT1J3MEs="}]})
}

async fn logs(state: &SharedState) -> Vec<store::RequestLogRow> {
    store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Passthrough
// ---------------------------------------------------------------------------

/// The one edit lmgw makes to a generation body is `model`. Everything else —
/// including the `<sd_cpp_extra_args>` block, which is sd.cpp's extension and
/// not the gateway's business — arrives byte for byte, and no field lmgw
/// invented arrives at all.
#[tokio::test]
async fn generation_forwards_the_body_verbatim_with_only_model_rewritten() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .expect(1)
        .mount(&mock)
        .await;
    let (state, base) = setup_cloud(&mock).await;

    let sent = json!({
        "model": "my-image",
        "prompt": "a lovely cat <sd_cpp_extra_args>{\"seed\":42}</sd_cpp_extra_args>",
        "n": 2,
        "size": "512x512",
        "output_format": "webp",
        "output_compression": 80,
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&sent)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["data"][0]["b64_json"], "aVZCT1J3MEs=");

    let reqs = mock.received_requests().await.unwrap();
    let body: Value = reqs
        .iter()
        .find(|r| r.url.path() == "/v1/images/generations")
        .map(|r| serde_json::from_slice(&r.body).expect("json body"))
        .expect("the generation reached the upstream");
    let mut expected = sent.clone();
    expected["model"] = json!("flux-1.1-pro");
    assert_eq!(body, expected, "lmgw added or dropped a field");

    // One row, the right class, and no tokens invented for a render.
    let l = logs(&state).await;
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].requested_alias, "my-image");
    assert_eq!(l[0].upstream_model.as_deref(), Some("flux-1.1-pro"));
    assert_eq!(l[0].class, Some("image".to_string()));
    assert_eq!(l[0].status, 200);
    assert_eq!(l[0].prompt_tokens, None);
    assert_eq!(l[0].completion_tokens, None);
    assert!(l[0].total_ms.is_some());
}

/// The edits route is the multipart twin: the upload is re-encoded with the
/// concrete model id and everything else relayed.
#[tokio::test]
async fn edits_relays_the_multipart_with_the_model_field_rewritten() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/edits"))
        .and(BodyContains("name=\"model\""))
        .and(BodyContains("flux-kontext"))
        .and(BodyContains("name=\"image\""))
        .and(BodyContains("name=\"prompt\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .expect(1)
        .mount(&mock)
        .await;
    let (state, base) = setup_cloud(&mock).await;

    let form = reqwest::multipart::Form::new()
        .text("model", "my-edit")
        .text("prompt", "make it night")
        .part(
            "image",
            reqwest::multipart::Part::bytes(b"\x89PNGfake".to_vec()).file_name("in.png"),
        );
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/edits"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["data"][0]["b64_json"], "aVZCT1J3MEs=");
    assert_eq!(logs(&state).await[0].class, Some("image".to_string()));
}

/// A local row resolves straight out of `image_models` under the class prefix
/// — no `upstreams` row exists here — and the request lands on the container
/// admission just started, on that container's own port.
#[tokio::test]
async fn a_local_row_reaches_its_own_container() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(wiremock::matchers::body_partial_json(
            json!({"model": "z-image-turbo"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .expect(1)
        .mount(&mock)
        .await;
    let (state, base) = setup_local(&mock, &[image_row("z-image-turbo")]).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "image/z-image-turbo", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );

    let up = state
        .runtime()
        .list()
        .iter()
        .any(|e| e.model_id == "z-image-turbo" && e.port == mock.address().port());
    assert!(up, "admission should have started the model's container");

    // Free, because it ran on our own hardware — a real 0 — and its images
    // are counted all the same, for the statistics (billable-units §4.7).
    let l = logs(&state).await;
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].upstream_name.as_deref(), Some("sdcpp"));
    assert_eq!(l[0].class, Some("image".to_string()));
    assert_eq!(l[0].images_out, Some(1));
    assert_eq!(l[0].cost_micro, Some(0));
    assert_eq!(l[0].price_source.as_deref(), Some("free_local"));
}

// ---------------------------------------------------------------------------
// Images out (billable-units design §4.4)
// ---------------------------------------------------------------------------

/// A manual `per_image` price of `rate` on `alias`.
async fn price_per_image(state: &SharedState, alias: &str, rate: f64) {
    let sheet = lmgw_core::pricing::Prices {
        source: lmgw_core::pricing::PriceSource::Manual,
        ..Default::default()
    };
    store::upsert_price(
        &state.db,
        lmgw_core::config::PriceScope::Alias,
        alias,
        lmgw_core::config::PriceUnit::PerImage,
        &sheet,
        Some(rate),
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

async fn generate_json(base: &str, body: Value) -> reqwest::Response {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    resp
}

/// The images in the answer are counted, never the request's `n`: two
/// generated images at 0.04 are 80 000 micro, whatever was asked for.
#[tokio::test]
async fn a_cloud_answer_is_priced_per_image_it_holds() {
    let mock = MockServer::start().await;
    let two = json!({"created": 1, "data": [{"b64_json": "aVZCT1J3MEs="},
        {"b64_json": "QUJD", "revised_prompt": "a \"data\": [cat]"}],
        "usage": {"input_tokens": 50, "output_tokens": 4000}});
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(two.clone()))
        .mount(&mock)
        .await;
    let (state, base) = setup_cloud(&mock).await;
    price_per_image(&state, "my-image", 0.04).await;

    let resp = generate_json(
        &base,
        json!({"model": "my-image", "prompt": "a cat", "n": 3}),
    )
    .await;
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        two,
        "relayed untouched"
    );
    let l = logs(&state).await;
    assert_eq!(l[0].images_out, Some(2));
    assert_eq!(l[0].cost_micro, Some(80_000), "{:?}", l[0]);
    assert_eq!(l[0].cost_units_micro, Some(80_000));
    assert_eq!(l[0].price_per_image, Some(0.04));
    assert_eq!(
        (l[0].prompt_tokens, l[0].completion_tokens),
        (None, None),
        "a gpt-image usage is not read (§4.8)"
    );
}

/// The edits route is counted the same way.
#[tokio::test]
async fn an_edit_answer_is_counted_too() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/edits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .mount(&mock)
        .await;
    let (state, base) = setup_cloud(&mock).await;
    price_per_image(&state, "my-edit", 0.04).await;

    let form = reqwest::multipart::Form::new()
        .text("model", "my-edit")
        .text("prompt", "make it night")
        .part(
            "image",
            reqwest::multipart::Part::bytes(b"\x89PNGfake".to_vec()).file_name("in.png"),
        );
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/edits"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    resp.bytes().await.unwrap();
    let l = logs(&state).await;
    assert_eq!(l[0].images_out, Some(1));
    assert_eq!(l[0].cost_micro, Some(40_000));
}

/// An answer with no `data` array to count is unknown, and NULL on a scope
/// priced per image — never 0.
#[tokio::test]
async fn an_answer_without_data_is_unpriced_not_free() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"created": 1})))
        .mount(&mock)
        .await;
    let (state, base) = setup_cloud(&mock).await;
    price_per_image(&state, "my-image", 0.04).await;

    generate_json(&base, json!({"model": "my-image", "prompt": "a cat"}))
        .await
        .bytes()
        .await
        .unwrap();
    let l = logs(&state).await;
    assert_eq!(l[0].images_out, None);
    assert_eq!(l[0].cost_micro, None, "{:?}", l[0]);
}

fn image_event(kind: &str) -> String {
    let data = json!({"type": kind, "b64_json": "aVZCT1J3MEs=", "created_at": 1,
        "partial_image_index": 0, "output_format": "png"});
    format!("event: {kind}\ndata: {data}\n\n")
}

/// `stream: true`: each final-image event is one image, a partial image is
/// none, and a stream in which no final event is recognised is unknown.
#[tokio::test]
async fn an_image_stream_counts_its_final_events() {
    let mock = MockServer::start().await;
    let two = [
        image_event("image_generation.partial_image"),
        image_event("image_generation.completed"),
        image_event("image_generation.partial_image"),
        image_event("image_generation.completed"),
    ]
    .concat();
    let partial_only = image_event("image_generation.partial_image");
    for (prompt, stream) in [("two", two.clone()), ("none", partial_only)] {
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .and(wiremock::matchers::body_partial_json(
                json!({"prompt": prompt}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
            .mount(&mock)
            .await;
    }
    let (state, base) = setup_cloud(&mock).await;
    price_per_image(&state, "my-image", 0.04).await;

    let body = |prompt: &str| json!({"model": "my-image", "prompt": prompt, "stream": true});
    let resp = generate_json(&base, body("two")).await;
    assert_eq!(resp.text().await.unwrap(), two, "relayed byte for byte");
    let l = logs(&state).await;
    assert!(l[0].streamed);
    assert_eq!(l[0].images_out, Some(2));
    assert_eq!(l[0].cost_micro, Some(80_000));

    generate_json(&base, body("none"))
        .await
        .bytes()
        .await
        .unwrap();
    let l = logs(&state).await;
    assert_eq!(l.len(), 2);
    assert_eq!(l[0].images_out, None, "partial images never count");
    assert_eq!(l[0].cost_micro, None);
}

// ---------------------------------------------------------------------------
// The two route guards (§6)
// ---------------------------------------------------------------------------

/// `Content-Type` is compared case-insensitively and without its parameters,
/// because RFC 9110 says it is. A client sending `Multipart/Form-Data` (Java's
/// HttpClient, a few Go libraries) was told its upload was not multipart.
#[tokio::test]
async fn an_uppercase_multipart_content_type_is_still_multipart() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    let (_state, base) = setup_local(&mock, &[image_row("z-image-turbo")]).await;

    let body = concat!(
        "--BOUND\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n",
        "image/z-image-turbo\r\n",
        "--BOUND\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nnight\r\n",
        "--BOUND\r\nContent-Disposition: form-data; name=\"image\"; filename=\"in.png\"\r\n",
        "Content-Type: image/png\r\n\r\nPNG\r\n--BOUND--\r\n",
    );
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/edits"))
        .header("content-type", "Multipart/Form-Data; boundary=BOUND")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    // It got as far as the *route's* own guard, which is the proof the body
    // was parsed: the shape complaint would have come first.
    assert!(msg.contains("edit = false"), "{msg}");
    assert!(
        !msg.contains("takes multipart/form-data"),
        "the body was read as multipart: {msg}"
    );
}

/// The other direction of the same gate: an `image/<id>` sent to a **text**
/// route is refused before admission, so the 7–13 GiB pipeline is never
/// started for a request sd-server has no handler for. The registry is empty
/// afterwards, which is the whole point — resolution alone used to be enough
/// to start it.
#[tokio::test]
async fn a_chat_completion_against_an_image_model_is_refused_before_admission() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    let (state, base) = setup_local(&mock, &[image_row("z-image-turbo")]).await;

    for (route, body) in [
        (
            "/v1/chat/completions",
            json!({"model": "image/z-image-turbo",
                   "messages": [{"role": "user", "content": "hi"}]}),
        ),
        (
            "/v1/completions",
            json!({"model": "image/z-image-turbo", "prompt": "hi"}),
        ),
        (
            "/v1/embeddings",
            json!({"model": "image/z-image-turbo", "input": "hi"}),
        ),
        (
            "/v1/rerank",
            json!({"model": "image/z-image-turbo", "query": "hi", "documents": ["a"]}),
        ),
    ] {
        let resp = reqwest::Client::new()
            .post(format!("{base}{route}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400, "{route}");
        let v: Value = resp.json().await.unwrap();
        let msg = v["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("is an image model"), "{route}: {msg}");
        assert!(msg.contains("/v1/images/generations"), "{route}: {msg}");
        assert!(msg.contains("/v1/images/edits"), "{route}: {msg}");
    }

    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        0,
        "no text route may contact the container"
    );
    assert!(
        state.runtime().list().is_empty(),
        "no text route may start an image container"
    );
}

/// The hard gate. A row that is not an edit pipeline is refused *before* the
/// container is touched: sd-server does not answer 400 to a reference-image
/// request it cannot serve, it dies (exit 139, §12.8), so this refusal is the
/// only thing between a client and a crashed container.
#[tokio::test]
async fn edits_on_a_non_edit_row_are_refused_before_the_container() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    let (state, base) = setup_local(&mock, &[image_row("z-image-turbo")]).await;

    let form = reqwest::multipart::Form::new()
        .text("model", "image/z-image-turbo")
        .text("prompt", "make it night")
        .part(
            "image",
            reqwest::multipart::Part::bytes(b"\x89PNGfake".to_vec()).file_name("in.png"),
        );
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/edits"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("edit = false"),
        "the refusal names the flag: {msg}"
    );
    assert!(msg.contains("/v1/images/edits"), "{msg}");

    // Nothing was contacted and nothing was started: not the health probe, not
    // the edits route.
    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        0,
        "the container must not even be started for a refused edit"
    );
    assert!(state.runtime().list().is_empty());
    assert_eq!(
        logs(&state).await[0].error_kind.as_deref(),
        Some("unsupported")
    );

    // The same row on the generations route is fine — the refusal is about the
    // route, not about the model.
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .mount(&mock)
        .await;
    let ok = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "image/z-image-turbo", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status().as_u16(), 200);
}

/// An edit row passes the same gate.
#[tokio::test]
async fn edits_on_an_edit_row_reach_the_container() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/edits"))
        .and(BodyContains("flux-kontext"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .expect(1)
        .mount(&mock)
        .await;
    let mut row = image_row("flux-kontext");
    row.edit = true;
    let (_state, base) = setup_local(&mock, &[row]).await;

    let form = reqwest::multipart::Form::new()
        .text("model", "image/flux-kontext")
        .text("prompt", "make it night")
        .part(
            "image",
            reqwest::multipart::Part::bytes(b"\x89PNGfake".to_vec()).file_name("in.png"),
        );
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/edits"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );
}

/// A model that is not an image model at all: a cloud chat alias whose catalog
/// says it outputs text, and a local row of another class. Both are refused
/// with the routes' own 400, before anything is forwarded.
#[tokio::test]
async fn a_model_that_does_not_serve_the_route_is_refused() {
    let mock = MockServer::start().await;
    let (state, base) = setup_cloud(&mock).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "my-chat", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("outputs text"), "{msg}");
    assert!(
        !mock
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path().starts_with("/v1/images/")),
        "nothing may be forwarded"
    );

    // A cloud model whose catalog states no modalities is *not* refused:
    // absent means unknown, and refusing on a silence would lock out every
    // provider that publishes no metadata.
    store::insert_alias(
        &state.db,
        &store::NewAlias {
            alias: "unlisted".into(),
            upstream_id: state
                .snapshot()
                .upstreams
                .values()
                .find(|u| u.name == "bfl")
                .unwrap()
                .id,
            upstream_model_id: "not-in-the-catalog".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .expect(1)
        .mount(&mock)
        .await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "unlisted", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

/// An alias onto a non-openai upstream never reaches an images route: an
/// anthropic base URL has neither the path nor the auth header.
#[tokio::test]
async fn a_non_openai_upstream_is_refused() {
    let mock = MockServer::start().await;
    let (state, base) = setup_cloud(&mock).await;
    let up_id = store::insert_upstream(
        &state.db,
        &store::NewUpstream {
            name: "claude".into(),
            protocol: lmgw_core::config::Protocol::Anthropic,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: mock.uri(),
            api_key: Some("sk-ant".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
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
        &store::NewAlias {
            alias: "not-images".into(),
            upstream_id: up_id,
            upstream_model_id: "claude-sonnet-5".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "not-images", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let v: Value = resp.json().await.unwrap();
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("openai-protocol"));
}

/// An unknown model is the gateway's own 404, not a forwarded one.
#[tokio::test]
async fn an_unknown_model_is_a_gateway_404() {
    let mock = MockServer::start().await;
    let (_state, base) = setup_cloud(&mock).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "nope", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let v: Value = resp.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("nope"));
}

/// The edits route takes multipart and says so; a JSON body is refused with
/// the shape it should have had rather than relayed as something sd-server
/// would answer 500 to.
#[tokio::test]
async fn edits_refuses_a_json_body_by_naming_the_shape() {
    let mock = MockServer::start().await;
    let (_state, base) = setup_cloud(&mock).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/edits"))
        .json(&json!({"model": "my-edit", "prompt": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let v: Value = resp.json().await.unwrap();
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("multipart/form-data"));
}

// ---------------------------------------------------------------------------
// Error normalization (§12.6)
// ---------------------------------------------------------------------------

/// sd-server answers `400 {"error":"<string>"}` and `500
/// {"error":"server_error","message":…}` — neither is OpenAI-shaped. Both come
/// out of lmgw in the one envelope every other route uses, with the server's
/// own sentence intact and in the log row.
#[tokio::test]
async fn both_sd_server_error_shapes_become_one_envelope() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(wiremock::matchers::body_partial_json(json!({"prompt": ""})))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "prompt required"})))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(wiremock::matchers::body_partial_json(
            json!({"prompt": "boom"}),
        ))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "server_error",
            "message": "[json.exception.parse_error.101] unexpected end of input",
        })))
        .mount(&mock)
        .await;
    let (state, base) = setup_local(&mock, &[image_row("z-image-turbo")]).await;
    let client = reqwest::Client::new();

    // The 400 string: the status is the upstream's, the message is the
    // server's own sentence and not the raw JSON.
    let resp = client
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "image/z-image-turbo", "prompt": ""}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let v: Value = resp.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("prompt required"),
        "the server's own sentence, not the raw JSON: {v}"
    );
    assert_eq!(v["error"]["code"], "upstream");
    assert_eq!(v["error"]["type"], "api_error");

    // The 500 object: the `error` string is the *type* there and `message`
    // carries the text, so that is what the client and the log row get.
    let resp = client
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "image/z-image-turbo", "prompt": "boom"}))
        .send()
        .await
        .unwrap();
    // A 5xx from any upstream is a 502 through this gateway, as it is on every
    // other route — the provider's status is preserved only where it is the
    // client's to act on.
    assert_eq!(resp.status().as_u16(), 502);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "upstream");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unexpected end of input"),
        "{v}"
    );

    let l = logs(&state).await;
    assert_eq!(l.len(), 2);
    for row in &l {
        assert_eq!(row.error_kind.as_deref(), Some("upstream"));
        assert_eq!(row.class, Some("image".to_string()));
    }
    assert!(
        l.iter().any(|r| r
            .error_msg
            .as_deref()
            .unwrap_or_default()
            .contains("prompt required")),
        "the log row lost the server's message: {:?}",
        l.iter().map(|r| r.error_msg.clone()).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Body size, key policy, hold
// ---------------------------------------------------------------------------

/// `max_body_mb` does not bound these routes (§6): an edit carries images and
/// a prompt is as long as it is. The body here is well over a 1 MB setting
/// that a chat request would be refused for.
#[tokio::test]
async fn a_generation_larger_than_max_body_mb_still_forwards() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .expect(1)
        .mount(&mock)
        .await;
    let (state, base) = setup_local(&mock, &[image_row("z-image-turbo")]).await;
    let mut s = state.snapshot().settings.clone();
    s.max_body_mb = 1;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let prompt = "x".repeat(3 * 1024 * 1024);
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "image/z-image-turbo", "prompt": prompt}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );

    // The same size on a bounded route is still refused, so the exemption is
    // per route rather than the setting having stopped working.
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "whatever", "messages": [{"role": "user", "content": "x".repeat(3 * 1024 * 1024)}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 413);
}

/// A gateway with auth on and one key carrying `policy`, plus a local image
/// row whose container is `mock`.
async fn with_policy(mock: &MockServer, policy: KeyPolicy) -> (SharedState, String) {
    let (state, base) = setup_local(mock, &[image_row("z-image-turbo")]).await;
    let mut s = state.snapshot().settings.clone();
    s.auth_enabled = true;
    store::save_settings(&state.db, &s).await.unwrap();
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, scope_mode, scope_patterns,
             budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit, expires_at)
         VALUES ('agent', ?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(hash_api_key("lmgw-image-key"))
    .bind(policy.scope_mode.as_str())
    .bind(&policy.scope_patterns)
    .bind(policy.budget_micro)
    .bind(policy.budget_period.as_str())
    .bind(policy.rpm_limit)
    .bind(policy.tpm_limit)
    .bind(policy.concurrency_limit)
    .bind(policy.expires_at.clone())
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    (state, base)
}

async fn generate(base: &str, model: &str) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .header("authorization", "Bearer lmgw-image-key")
        .json(&json!({"model": model, "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Scope applies to these routes like every other: a key scoped to chat
/// aliases cannot reach an image model by posting to a different path.
#[tokio::test]
async fn a_key_outside_its_scope_is_refused_on_the_image_routes() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    let (state, base) = with_policy(
        &mock,
        KeyPolicy {
            scope_mode: ScopeMode::Allow,
            scope_patterns: "claude-*".into(),
            ..Default::default()
        },
    )
    .await;

    let (status, err) = generate(&base, "image/z-image-turbo").await;
    assert_eq!(status, 403);
    assert_eq!(err["error"]["code"], "key_scope");
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("image/z-image-turbo"));
    assert_eq!(mock.received_requests().await.unwrap().len(), 0);
    let l = logs(&state).await;
    assert_eq!(l[0].class, Some("image".to_string()));
    assert_eq!(l[0].error_kind.as_deref(), Some("key_scope"));
}

/// Requests-per-minute is the knob that bites here — image traffic carries no
/// tokens, so a tokens-per-minute limit would never see it.
#[tokio::test]
async fn the_requests_per_minute_limit_bites_on_the_image_routes() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .mount(&mock)
        .await;
    let (_state, base) = with_policy(
        &mock,
        KeyPolicy {
            rpm_limit: 1,
            ..Default::default()
        },
    )
    .await;

    let (first, _) = generate(&base, "image/z-image-turbo").await;
    assert_eq!(first, 200);
    let (second, err) = generate(&base, "image/z-image-turbo").await;
    assert_eq!(second, 429);
    assert_eq!(err["error"]["code"], "key_rate");
}

/// A spent budget refuses too. Local rendering is free, so the spend is seeded
/// on a priced cloud alias and the refusal then applies to every route the key
/// can reach — which is the point of a budget.
#[tokio::test]
async fn a_spent_budget_refuses_an_image_request() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    let (state, base) = with_policy(
        &mock,
        KeyPolicy {
            budget_micro: 1_000_000,
            budget_period: lmgw_core::config::BudgetPeriod::Month,
            ..Default::default()
        },
    )
    .await;
    // Spend the whole budget on something priced: local rendering is free, so
    // the spend has to come from traffic that had a price.
    sqlx::query(
        "INSERT INTO usage_hourly (bucket_utc, key_id, alias, upstream_id, class, outcome,
             requests, cost_micro)
         VALUES (strftime('%Y-%m-%dT%H','now'),
                 (SELECT id FROM api_keys WHERE name='agent'), 'a', 1, 'chat', 'ok', 1, 2000000)",
    )
    .execute(&state.db)
    .await
    .unwrap();

    let (status, err) = generate(&base, "image/z-image-turbo").await;
    assert_eq!(status, 403, "{err}");
    assert_eq!(err["error"]["code"], "key_budget");
    assert_eq!(mock.received_requests().await.unwrap().len(), 0);
}

/// Under a GPU hold the local row is not started at all: the request goes to
/// its configured fallback alias, and the response says which one answered.
#[tokio::test]
async fn a_held_image_model_falls_back_and_stamps_the_header() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(wiremock::matchers::body_partial_json(
            json!({"model": "flux-1.1-pro"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .expect(1)
        .mount(&mock)
        .await;
    // The cloud gateway (so an alias exists to fall back to), with a local row
    // and the hold on top of it.
    let (state, base) = setup_cloud(&mock).await;
    let port = mock.address().port();
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        Arc::new(FakePodman),
        reqwest::Client::new(),
        Arc::new(move || Ok(port)),
    )));
    let mut row = image_row("z-image-turbo");
    row.hold_fallback_mode = lmgw_core::config::HoldFallbackMode::Alias;
    row.hold_fallback = Some("my-image".into());
    store::insert_image_model(&state.db, &row).await.unwrap();
    let mut s = state.snapshot().settings.clone();
    s.hold.active = true;
    s.image.models_dir = models_dir(&state);
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "image/z-image-turbo", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("my-image")
    );
    assert_eq!(fallback_reason(&resp), Some("hold"));
    // The held model's container was never started.
    assert!(state.runtime().list().is_empty());
}

/// A row with no fallback refuses with the hold's own code rather than
/// starting the container the hold exists to prevent.
#[tokio::test]
async fn a_held_image_model_without_a_fallback_refuses() {
    let mock = MockServer::start().await;
    let (state, base) = setup_local(&mock, &[image_row("z-image-turbo")]).await;
    let mut s = state.snapshot().settings.clone();
    s.hold.active = true;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "image/z-image-turbo", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "gpu_hold");
    assert_eq!(mock.received_requests().await.unwrap().len(), 0);
}

/// The admission guard is held for the whole render, not released at the
/// headers: while a generation is outstanding the idle reaper must leave the
/// container alone, and only once the answer has been relayed does the same
/// model become reapable.
///
/// The wiremock delays its answer rather than stalling mid-body, which is what
/// a synchronous render looks like anyway — sd-server computes the whole image
/// before it writes a byte.
#[tokio::test]
async fn the_admission_guard_is_held_for_the_whole_render() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(one_png())
                .set_delay(std::time::Duration::from_millis(2_500)),
        )
        .mount(&mock)
        .await;
    let mut row = image_row("z-image-turbo");
    row.idle_seconds = 1;
    let (state, base) = setup_local(&mock, &[row]).await;

    let url = format!("{base}/v1/images/generations");
    let call = tokio::spawn(async move {
        reqwest::Client::new()
            .post(url)
            .json(&json!({"model": "image/z-image-turbo", "prompt": "a cat"}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    });

    // Long enough that the entry's `last_used` is older than its 1 s idle
    // timeout while the render is still running.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
    let before = state.runtime().list();
    assert_eq!(before.len(), 1, "the container should be up: {before:?}");
    assert_eq!(before[0].in_flight, 1, "the guard must still be held");
    lmgw_core::runtime::lifecycle::reap_idle(&state).await;
    assert_eq!(
        state.runtime().list().len(),
        1,
        "the reaper must not stop a model that is rendering"
    );

    assert_eq!(call.await.unwrap(), 200);

    // Released with the last byte: the same model is reapable on a later tick.
    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    assert_eq!(state.runtime().list()[0].in_flight, 0);
    lmgw_core::runtime::lifecycle::reap_idle(&state).await;
    assert!(
        state.runtime().list().is_empty(),
        "the guard was never released"
    );
}

// ---------------------------------------------------------------------------
// The class's load test (image-generation design §8, WP4)
// ---------------------------------------------------------------------------

/// A PNG signature followed by filler — 112 bytes, so "how many bytes came
/// back" is a number the assertion can actually be wrong about. Not a valid
/// image: nothing in lmgw decodes one, it only counts them.
const TEST_PNG_B64: &str =
    "iVBORw0KGgpsbWd3IGxvYWQgdGVzdCBpbWFnZSBieXRlc2xtZ3cgbG9hZCB0ZXN0IGltYWdl\
                            IGJ5dGVzbG1ndyBsb2FkIHRlc3QgaW1hZ2UgYnl0ZXNsbWd3IGxvYWQgdGVzdCBpbWFnZS\
                            BieXRlcw==";
const TEST_PNG_LEN: usize = 112;

/// `lmgw__local_model_test target=image` starts the row's own container and
/// draws one image through the **real** `/v1/images/generations` handler.
///
/// Three things are asserted because three things were easy to get wrong: the
/// container is started for real (the fake podman saw a `run`), the request
/// carries the §12.3 test shape — 256², four steps, a fixed seed, and the
/// steps/seed inside sd.cpp's own `<sd_cpp_extra_args>` block, because the
/// OpenAI route reads nothing else — and the answer is measured in decoded
/// bytes rather than reported as "200 OK".
#[tokio::test]
async fn load_testing_an_image_model_draws_one_small_image_and_reports_its_bytes() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1,
            "output_format": "png",
            "data": [{"b64_json": TEST_PNG_B64}],
        })))
        .expect(1)
        .mount(&mock)
        .await;
    let (state, _base) = setup_local(&mock, &[image_row("z-image-turbo")]).await;

    let out = crate::common::model_test_wire(
        lmgw_core::modelinfo::local_model_test(&state, "z-image-turbo", Some("image"))
            .await
            .unwrap(),
    );
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["class"], "image");
    assert_eq!(out["probe"], "image_generation");
    assert_eq!(out["endpoint"], "/v1/images/generations");
    assert_eq!(out["public_name"], "image/z-image-turbo");
    assert_eq!(out["size"], "256x256");
    assert_eq!(out["steps"], 4);
    assert_eq!(out["seed"], 42);
    assert_eq!(out["n"], 1);
    assert_eq!(out["output_format"], "png");
    assert_eq!(out["bytes"], TEST_PNG_LEN);
    assert!(out["latency_ms"].is_u64(), "{out}");
    // Visible under --nocapture: the one figure the spike's table is about.
    eprintln!(
        "local_model_test target=image: {} ms for {} bytes",
        out["latency_ms"], out["bytes"]
    );

    let reqs = mock.received_requests().await.unwrap();
    let body: Value = reqs
        .iter()
        .find(|r| r.url.path() == "/v1/images/generations")
        .map(|r| serde_json::from_slice(&r.body).expect("json body"))
        .expect("the test never reached sd-server");
    assert_eq!(body["size"], "256x256");
    assert_eq!(body["n"], 1);
    assert_eq!(body["output_format"], "png");
    let prompt = body["prompt"].as_str().unwrap();
    assert!(
        prompt.contains("<sd_cpp_extra_args>")
            && prompt.contains("\"seed\":42")
            && prompt.contains("\"sample_steps\":4"),
        "steps and seed must ride in sd.cpp's own extension block: {prompt}"
    );

    // It went through the real handler, so it is in Logs like any request.
    let l = logs(&state).await;
    assert_eq!(l.len(), 1, "{l:?}");
    assert_eq!(l[0].class, Some("image".to_string()));
    assert_eq!(l[0].requested_alias, "image/z-image-turbo");
    assert_eq!(l[0].status, 200);
}

/// sd-server's errors are not OpenAI-shaped, and the test reports the
/// normalized one plus the row's own container log rather than "it failed".
#[tokio::test]
async fn a_failing_generation_reports_the_error_and_a_hint() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "server_error",
            "message": "CUDA error: out of memory",
        })))
        .mount(&mock)
        .await;
    let (state, _base) = setup_local(&mock, &[image_row("flux")]).await;

    let out = crate::common::model_test_wire(
        lmgw_core::modelinfo::local_model_test(&state, "flux", None)
            .await
            .unwrap(),
    );
    assert_eq!(out["ok"], false, "{out}");
    assert_eq!(out["loaded"], false);
    assert!(
        out["error"].as_str().unwrap().contains("out of memory"),
        "{out}"
    );
    assert!(
        out["hint"].as_str().unwrap().contains("offload_to_cpu"),
        "the VRAM hint should name this class's knob: {out}"
    );
}

/// The row says what it does, and the test sends only what the row advertises
/// — the same rule the edits route applies to the `edit` column.
#[tokio::test]
async fn a_row_that_does_not_claim_img_gen_is_not_asked_to_draw_a_still() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    let mut row = image_row("wan-video");
    row.modes = vec!["vid_gen".into()];
    let (state, _base) = setup_local(&mock, &[row]).await;

    let err = lmgw_core::modelinfo::local_model_test(&state, "wan-video", Some("image"))
        .await
        .unwrap_err();
    assert!(err.contains("vid_gen") && err.contains("img_gen"), "{err}");
    assert!(
        mock.received_requests().await.unwrap().is_empty(),
        "nothing should have been sent"
    );
}

/// `x-lmgw-fallback-reason`, which always travels with `x-lmgw-fallback`.
fn fallback_reason(resp: &reqwest::Response) -> Option<&str> {
    resp.headers()
        .get("x-lmgw-fallback-reason")
        .and_then(|v| v.to_str().ok())
}
