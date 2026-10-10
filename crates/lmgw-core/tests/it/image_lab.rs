//! The Image lab's mini-API (`/image-lab/api/*`, image-generation design §8),
//! with wiremock standing in for sd-server and for a cloud image provider.
//!
//! `tests/it/image_backend.rs` proves the two `/v1/images/*` routes; this file
//! proves that the lab reaches them *through* those routes rather than beside
//! them. What it asserts is therefore the seam: the body the page's form turns
//! into (including when sd.cpp's prompt extension is appended and when it is
//! not), the multipart an upload becomes, the one `class = image` log row a lab
//! call leaves — the lab is not a second client that logs differently — and the
//! error envelope, which must arrive at the page exactly as a client would have
//! received it.

use std::sync::Arc;

use lmgw_core::config::Settings;
use lmgw_core::runtime::registry::Registry;
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewImageModel};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;
use common::Gw;

// ---------------------------------------------------------------------------
// Fixtures — the image_backend.rs set, kept deliberately identical so the two
// files describe the same gateway.
// ---------------------------------------------------------------------------

pub(crate) fn image_row(model_id: &str, edit: bool) -> NewImageModel {
    let mut files = serde_json::Map::new();
    files.insert(
        "diffusion_model".into(),
        json!(format!("leejet/{model_id}/weights.gguf")),
    );
    let mut args = serde_json::Map::new();
    args.insert("width".into(), json!(768));
    args.insert("height".into(), json!(768));
    NewImageModel {
        model_id: model_id.into(),
        files,
        args,
        modes: vec![],
        edit,
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

/// sd-server's readiness route, which doubles as its capabilities probe (§3) —
/// and is where the lab's sampler and scheduler selects come from.
pub(crate) async fn mount_ready(mock: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "supported_modes": ["img_gen"],
            "current_mode": "img_gen",
            "limits": {"min_width": 64, "max_width": 4096, "min_height": 64, "max_height": 4096},
            "samplers": ["euler", "euler_a", "dpm++2m"],
            "schedulers": ["discrete", "karras"],
            "output_formats": ["png", "jpeg", "webp"],
            "loras": [{"name": "add_detail"}],
        })))
        .mount(mock)
        .await;
}

fn serve(state: &SharedState) -> Gw {
    let key = common::dashboard_key(state);
    let app = build_router(state.clone());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Gw { base, key }
}

/// An image models dir inside the state's own test dir, which goes with
/// the state: a kept `tempdir()` stayed behind in /tmp, which is RAM.
fn models_dir(state: &SharedState) -> String {
    let dir = state.data_dir.join("image-models");
    std::fs::create_dir_all(&dir).unwrap();
    dir.display().to_string()
}

pub(crate) async fn setup_local(mock: &MockServer, rows: &[NewImageModel]) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let port = mock.address().port();
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        Arc::new(FakePodman),
        reqwest::Client::new(),
        Arc::new(move || Ok(port)),
    )));
    let mut s = Settings::default();
    s.vram.load_timeout_seconds = 5;
    s.image.models_dir = models_dir(&state);
    s.container_prefix = "lmgwtest".into();
    store::save_settings(&state.db, &s).await.unwrap();
    for row in rows {
        store::insert_image_model(&state.db, row).await.unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let base = serve(&state);
    (state, base)
}

/// A gateway with one cloud upstream publishing an image generator, an edit
/// model and a chat model, plus an alias onto each.
pub(crate) async fn setup_cloud(mock: &MockServer) -> (SharedState, Gw) {
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

pub(crate) fn one_png() -> Value {
    json!({"created": 1, "output_format": "png", "data": [{"b64_json": "aVZCT1J3MEs="}]})
}

async fn logs(state: &SharedState) -> Vec<store::RequestLogRow> {
    store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap()
}

/// `POST /image-lab/api/generate` with the page's form → (status, body).
async fn generate(base: &Gw, form: Value) -> (u16, Value) {
    let resp = base
        .client()
        .post(format!("{base}/image-lab/api/generate"))
        .json(&form)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The body one generation actually reached the upstream with.
async fn sent_body(mock: &MockServer, nth: usize) -> Value {
    let reqs = mock.received_requests().await.unwrap();
    let mut bodies = reqs
        .iter()
        .filter(|r| r.url.path() == "/v1/images/generations")
        .map(|r| serde_json::from_slice::<Value>(&r.body).expect("json body"));
    bodies
        .nth(nth)
        .expect("the generation reached the upstream")
}

/// The `<sd_cpp_extra_args>` block out of a prompt, parsed.
fn extra_block(prompt: &str) -> Option<Value> {
    use lmgw_api_types::image_lab::{EXTRA_CLOSE, EXTRA_OPEN};
    let start = prompt.find(EXTRA_OPEN)? + EXTRA_OPEN.len();
    let end = prompt.find(EXTRA_CLOSE)?;
    Some(serde_json::from_str(&prompt[start..end]).expect("the block is valid JSON"))
}

// ---------------------------------------------------------------------------
// The model list
// ---------------------------------------------------------------------------

/// The picker is fed by the same capability objects the routes are gated on:
/// an enabled local row under its `image/` name, its container's probed
/// sampler list once it is up, and the `args` the form reads its size defaults
/// out of.
#[tokio::test]
async fn the_model_list_carries_endpoints_args_and_probed_capabilities() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .mount(&mock)
        .await;
    let (_state, base) = setup_local(
        &mock,
        &[
            image_row("z-image-turbo", false),
            image_row("kontext", true),
        ],
    )
    .await;

    let v: Value = base
        .client()
        .get(format!("{base}/image-lab/api/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let models = v["models"].as_array().unwrap();
    assert_eq!(models.len(), 2, "{v}");
    let turbo = models
        .iter()
        .find(|m| m["name"] == "image/z-image-turbo")
        .unwrap();
    assert_eq!(turbo["local"], true);
    assert_eq!(turbo["edit"], false);
    assert_eq!(turbo["endpoints"], json!(["/v1/images/generations"]));
    // The form's size defaults are the row's own flags, never a constant.
    assert_eq!(turbo["args"]["width"], 768);
    // Nothing has started yet, so there is no sampler list to offer.
    assert!(turbo["image_capabilities"].is_null());
    assert!(turbo["state"].is_null());

    let kontext = models
        .iter()
        .find(|m| m["name"] == "image/kontext")
        .unwrap();
    assert_eq!(kontext["edit"], true);
    assert_eq!(
        kontext["endpoints"],
        json!(["/v1/images/generations", "/v1/images/edits"]),
        "an edit row offers the lab's edit panel"
    );

    // Once a container is up, its own answer fills the sampler and scheduler
    // selects — and the limits the size fields are bounded by.
    let (status, _) = generate(
        &base,
        json!({"model": "image/z-image-turbo", "prompt": "a cat"}),
    )
    .await;
    assert_eq!(status, 200);
    let v: Value = base
        .client()
        .get(format!("{base}/image-lab/api/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let turbo = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "image/z-image-turbo")
        .unwrap();
    assert_eq!(turbo["state"], "ready");
    assert_eq!(
        turbo["image_capabilities"]["samplers"],
        json!(["euler", "euler_a", "dpm++2m"])
    );
    assert_eq!(turbo["image_capabilities"]["limits"]["max_width"], 4096);
    assert_eq!(
        turbo["image_capabilities"]["loras"][0]["name"],
        "add_detail"
    );
}

/// A cloud alias belongs in the picker exactly when its catalog says it draws
/// — and a chat model never does, however OpenAI-shaped its upstream is.
#[tokio::test]
async fn cloud_aliases_are_listed_by_what_their_catalog_advertises() {
    let mock = MockServer::start().await;
    let (_state, base) = setup_cloud(&mock).await;

    let v: Value = base
        .client()
        .get(format!("{base}/image-lab/api/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"my-image"), "{names:?}");
    assert!(names.contains(&"my-edit"), "{names:?}");
    assert!(!names.contains(&"my-chat"), "a chat model is not a drawer");

    let edit = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "my-edit")
        .unwrap();
    assert_eq!(edit["local"], false);
    assert_eq!(edit["edit"], true, "the catalog takes an image in");
    assert!(edit["args"].is_null(), "a cloud alias has no argv");
}

// ---------------------------------------------------------------------------
// Generate
// ---------------------------------------------------------------------------

/// The lab's form becomes the OpenAI body and nothing more — and sd.cpp's
/// prompt extension is appended **only** when the form set something that has
/// nowhere else to go. An empty block is not harmless: on a build that does not
/// strip it, it is prompt text.
#[tokio::test]
async fn the_block_is_appended_only_when_an_extra_is_set() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .expect(2)
        .mount(&mock)
        .await;
    let (state, base) = setup_local(&mock, &[image_row("z-image-turbo", false)]).await;

    // 1. A bare form: prompt, size, n — the five fields the route reads.
    let (status, v) = generate(
        &base,
        json!({
            "model": "image/z-image-turbo",
            "prompt": "a lovely cat",
            "width": "512", "height": "512", "n": "1", "output_format": "png",
        }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["endpoint"], "/v1/images/generations");
    assert_eq!(v["response"]["data"][0]["b64_json"], "aVZCT1J3MEs=");
    // The lab echoes the request it dispatched, so the page's panel and the
    // wire cannot disagree.
    assert_eq!(v["request"]["prompt"], "a lovely cat");
    assert_eq!(v["request"]["size"], "512x512");

    let body = sent_body(&mock, 0).await;
    assert_eq!(body["prompt"], "a lovely cat");
    assert_eq!(body["size"], "512x512");
    assert_eq!(body["n"], 1);
    assert_eq!(body["output_format"], "png");
    // The alias is rewritten to the concrete id by the route, as for any client.
    assert_eq!(body["model"], "z-image-turbo");
    assert_eq!(
        body.as_object().unwrap().len(),
        5,
        "lmgw invented a field: {body}"
    );

    // 2. The same form with every extra the panel offers.
    let (status, v) = generate(
        &base,
        json!({
            "model": "image/z-image-turbo",
            "prompt": "a lovely cat",
            "negative_prompt": "blurry",
            "width": "512", "height": "512",
            "steps": "8", "cfg_scale": "1.5", "seed": "42",
            "sampler": "euler", "scheduler": "karras",
            "loras": [{"path": "add_detail", "multiplier": "0.8"}],
        }),
    )
    .await;
    assert_eq!(status, 200, "{v}");

    let body = sent_body(&mock, 1).await;
    let prompt = body["prompt"].as_str().unwrap();
    assert!(prompt.starts_with("a lovely cat "), "{prompt}");
    let extra = extra_block(prompt).expect("the block is there");
    assert_eq!(extra["negative_prompt"], "blurry");
    assert_eq!(extra["seed"], 42);
    assert_eq!(extra["sample_params"]["sample_steps"], 8);
    assert_eq!(extra["sample_params"]["sample_method"], "euler");
    assert_eq!(extra["sample_params"]["scheduler"], "karras");
    assert_eq!(extra["sample_params"]["guidance"]["txt_cfg"], 1.5);
    assert_eq!(extra["lora"][0]["path"], "add_detail");
    assert_eq!(extra["lora"][0]["multiplier"], 0.8);
    // The block is the *only* place the extras go: the top level stays the
    // five fields sd-server reads.
    assert!(body.get("seed").is_none());
    assert!(body.get("steps").is_none());

    // One log row per lab call, in the image class — the lab is a client of
    // the route, not a bypass of it.
    let l = logs(&state).await;
    assert_eq!(
        l.len(),
        2,
        "{:?}",
        l.iter().map(|r| r.status).collect::<Vec<_>>()
    );
    for row in &l {
        assert_eq!(row.class, Some("image".to_string()));
        assert_eq!(row.requested_alias, "image/z-image-turbo");
        assert_eq!(row.status, 200);
        assert!(row.total_ms.is_some());
    }
}

/// A form that cannot describe a request is refused before anything starts a
/// container, in the envelope shape the page renders every other failure with.
#[tokio::test]
async fn a_form_that_is_not_a_request_is_named_not_guessed() {
    let mock = MockServer::start().await;
    let (state, base) = setup_local(&mock, &[image_row("z-image-turbo", false)]).await;

    let (status, v) = generate(
        &base,
        json!({"model": "image/z-image-turbo", "prompt": " "}),
    )
    .await;
    assert_eq!(status, 400);
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("prompt is required"));

    let (status, v) = generate(
        &base,
        json!({"model": "image/z-image-turbo", "prompt": "a cat", "steps": "eight"}),
    )
    .await;
    assert_eq!(status, 400);
    assert!(v["error"]["message"].as_str().unwrap().contains("steps"));

    // Nothing was dispatched, so nothing is in the log and no container was
    // started for a request that was never going to be made.
    assert!(logs(&state).await.is_empty());
}

/// The lab must show the gateway's own error, not its own rendering of one:
/// same status, same envelope, byte for byte what a client would have read.
#[tokio::test]
async fn a_gateway_error_reaches_the_lab_unchanged() {
    let mock = MockServer::start().await;
    mount_ready(&mock).await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "prompt required"})))
        .mount(&mock)
        .await;
    let (state, base) = setup_local(&mock, &[image_row("z-image-turbo", false)]).await;

    let form = json!({"model": "image/z-image-turbo", "prompt": "a cat"});
    let (lab_status, lab_body) = generate(&base, form).await;

    // The same request, straight at the published route.
    let resp = base
        .client()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "image/z-image-turbo", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    let route_status = resp.status().as_u16();
    let route_body: Value = resp.json().await.unwrap();

    assert_eq!(lab_status, route_status);
    assert_eq!(lab_body, route_body, "the lab re-wrapped the envelope");
    assert_eq!(lab_body["error"]["code"], "upstream");
    assert!(lab_body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("prompt required"));

    let l = logs(&state).await;
    assert_eq!(l.len(), 2);
    for row in &l {
        assert_eq!(row.class, Some("image".to_string()));
        assert_eq!(row.error_kind.as_deref(), Some("upstream"));
    }
}

/// A model that does not serve the route is refused by `resolve_image`, and the
/// lab shows that refusal — it does not pre-empt it with a rule of its own.
#[tokio::test]
async fn a_chat_alias_is_refused_by_the_route_the_lab_dispatches_to() {
    let mock = MockServer::start().await;
    let (_state, base) = setup_cloud(&mock).await;

    let (status, v) = generate(&base, json!({"model": "my-chat", "prompt": "a cat"})).await;
    assert_eq!(status, 400, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outputs text"),
        "{v}"
    );
}

// ---------------------------------------------------------------------------
// Edit
// ---------------------------------------------------------------------------

/// The browser uploads a JSON form plus two files; what leaves the gateway is
/// one multipart with `model` rewritten to the concrete id, and the same log
/// row a client's edit leaves.
#[tokio::test]
async fn an_edit_relays_the_multipart_with_the_model_rewritten() {
    struct BodyContains(&'static str);
    impl wiremock::Match for BodyContains {
        fn matches(&self, request: &wiremock::Request) -> bool {
            String::from_utf8_lossy(&request.body).contains(self.0)
        }
    }

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/edits"))
        .and(BodyContains("name=\"model\""))
        .and(BodyContains("flux-kontext"))
        .and(BodyContains("name=\"image\""))
        .and(BodyContains("name=\"mask\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .expect(1)
        .mount(&mock)
        .await;
    let (state, base) = setup_cloud(&mock).await;

    let form = json!({
        "model": "my-edit",
        "prompt": "make it night",
        "negative_prompt": "daylight",
        "width": "512", "height": "512", "n": "1", "output_format": "png",
    });
    let upload = reqwest::multipart::Form::new()
        .text("form", form.to_string())
        .part(
            "image",
            reqwest::multipart::Part::bytes(b"\x89PNGfake".to_vec())
                .file_name("in.png")
                .mime_str("image/png")
                .unwrap(),
        )
        .part(
            "mask",
            reqwest::multipart::Part::bytes(b"\x89PNGmask".to_vec())
                .file_name("mask.png")
                .mime_str("image/png")
                .unwrap(),
        );
    let resp = base
        .client()
        .post(format!("{base}/image-lab/api/edit"))
        .multipart(upload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["endpoint"], "/v1/images/edits");
    assert_eq!(v["response"]["data"][0]["b64_json"], "aVZCT1J3MEs=");
    // The panel's field summary describes the request that was made.
    let fields = v["request"]["fields"].as_array().unwrap();
    let field = |name: &str| {
        fields
            .iter()
            .find(|f| f["name"] == name)
            .map(|f| f["value"].as_str().unwrap().to_string())
    };
    assert_eq!(field("size").as_deref(), Some("512x512"));
    assert!(field("prompt").unwrap().contains("\"negative_prompt\""));
    assert_eq!(v["request"]["files"][0]["name"], "image");
    assert_eq!(v["request"]["files"][1]["bytes"], 8);

    // The upstream saw the real thing: the extension block rode along in the
    // prompt field, and `model` is the concrete id.
    let reqs = mock.received_requests().await.unwrap();
    let body = reqs
        .iter()
        .find(|r| r.url.path() == "/v1/images/edits")
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .unwrap();
    assert!(body.contains("sd_cpp_extra_args"), "{body}");
    assert!(body.contains("daylight"));
    assert!(!body.contains("my-edit"), "the alias reached the upstream");

    let l = logs(&state).await;
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].class, Some("image".to_string()));
    assert_eq!(l[0].requested_alias, "my-edit");
}

/// The upload is identified by its **field name**, not by whether a filename
/// could be parsed out of its header.
///
/// A filename ending in a backslash is an unterminated escape in a quoted
/// string, so the multipart parser hands back `None` — and the part was
/// dropped, which the page then reported as "an edit needs an image to edit"
/// for a picture the owner had plainly attached. The bytes are what matter;
/// the name is a label.
#[tokio::test]
async fn an_upload_whose_filename_will_not_parse_is_still_the_image() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/edits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(one_png()))
        .expect(1)
        .mount(&mock)
        .await;
    let (_state, base) = setup_cloud(&mock).await;

    let upload = reqwest::multipart::Form::new()
        .text(
            "form",
            json!({"model": "my-edit", "prompt": "make it night"}).to_string(),
        )
        .part(
            "image",
            reqwest::multipart::Part::bytes(b"\x89PNGfake".to_vec()).file_name("shot\\"),
        );
    let resp = base
        .client()
        .post(format!("{base}/image-lab/api/edit"))
        .multipart(upload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "{:?}", resp.text().await);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["request"]["files"][0]["name"], "image");
    assert_eq!(v["request"]["files"][0]["bytes"], 8);

    // And the part that reached the upstream carries a filename that cannot
    // break out of its own header.
    let reqs = mock.received_requests().await.unwrap();
    let body = reqs
        .iter()
        .find(|r| r.url.path() == "/v1/images/edits")
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .unwrap();
    // The parser really did hand back no filename (this is the whole premise),
    // so the relayed part is labelled with the field name and carries no
    // backslash into a header.
    assert!(
        body.contains("name=\"image\"; filename=\"image\""),
        "{body}"
    );
    assert!(
        !body.contains('\\'),
        "no backslash may survive into a header: {body}"
    );
}

/// An upload with no image is a form error, not a crashed pipeline: the edits
/// route needs an image, and a request without one must not reach a container.
#[tokio::test]
async fn an_edit_without_an_image_never_leaves_the_gateway() {
    let mock = MockServer::start().await;
    let (state, base) = setup_cloud(&mock).await;

    let upload = reqwest::multipart::Form::new().text(
        "form",
        json!({"model": "my-edit", "prompt": "make it night"}).to_string(),
    );
    let resp = base
        .client()
        .post(format!("{base}/image-lab/api/edit"))
        .multipart(upload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let v: Value = resp.json().await.unwrap();
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("needs an image"));
    assert!(logs(&state).await.is_empty());
}
