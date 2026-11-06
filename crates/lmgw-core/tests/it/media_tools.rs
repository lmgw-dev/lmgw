//! The image and audio tools (chat-media-tools design §11, slice 1), end to
//! end: wiremock stands in for sd-server on the port a fake podman's
//! container "answers" on (the image backend's fixtures,
//! `image_backend.rs`), for a cloud image or speech provider, and for the
//! chat model a Chat thread's loop calls.

mod actions;
mod audio_call;
mod cancel;
mod cancel_follow;
mod chat_turn;
mod claim;
mod defaults;
mod edit_eligibility;
mod hold;
mod image_call;
mod keep;
mod labels;
mod list;
mod mcp_progress;
mod progress;
mod refs;
mod resubmit;
mod scope;
mod upscale;
mod upscale_checks;
mod upscale_stop;

use std::sync::Arc;

use base64::Engine as _;
use lmgw_core::config::Settings;
use lmgw_core::erased::Erased;
use lmgw_core::runtime::registry::Registry;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewImageModel};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};

/// sd-server 0518b34's capabilities for the spike pipeline.
pub(crate) const CAPS: &str =
    include_str!("../fixtures/sdcpp/capabilities-z-image-turbo-0518b34.json");

/// The spike's diffusion model: the Z-Image-Turbo recipe matches it.
pub(crate) const Z_IMAGE: &str = "leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf";

/// A `podman` that agrees to everything and starts nothing — the
/// "container" is the wiremock the port allocator points at.
pub(crate) struct FakePodman;

impl lmgw_core::runtime::registry::CommandRunner for FakePodman {
    fn run<'a>(
        &'a self,
        _program: &'a str,
        _args: &'a [String],
    ) -> Erased<'a, std::io::Result<lmgw_core::runtime::registry::CmdOutput>> {
        Box::pin(async move {
            Ok(lmgw_core::runtime::registry::CmdOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        })
    }
}

/// An image row on `file`, with the spike's defaults in its args.
pub(crate) fn image_row(model_id: &str, file: &str, edit: bool) -> NewImageModel {
    let mut files = serde_json::Map::new();
    files.insert("diffusion_model".into(), json!(file));
    let args = json!({"cfg_scale": 1.0, "steps": 8, "width": 1024, "height": 1024});
    NewImageModel {
        model_id: model_id.into(),
        files,
        args: args.as_object().unwrap().clone(),
        edit,
        enabled: true,
        ..Default::default()
    }
}

/// A gateway whose image rows' containers are `sd`, with `rows` stored.
pub(crate) async fn gateway(sd: &MockServer, rows: &[NewImageModel]) -> (SharedState, Gw) {
    gateway_on(vec![sd.address().port()], rows).await
}

/// A `podman` that agrees to everything, starts nothing, and records each
/// command's arguments.
#[derive(Default)]
pub(crate) struct RecordingPodman(std::sync::Mutex<Vec<Vec<String>>>);

impl RecordingPodman {
    /// The containers it was asked to stop, by name.
    pub(crate) fn stopped(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.first().is_some_and(|c| c == "stop"))
            .filter_map(|a| a.last().cloned())
            .collect()
    }
}

impl lmgw_core::runtime::registry::CommandRunner for RecordingPodman {
    fn run<'a>(
        &'a self,
        _program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<lmgw_core::runtime::registry::CmdOutput>> {
        self.0.lock().unwrap().push(args.to_vec());
        Box::pin(async move {
            Ok(lmgw_core::runtime::registry::CmdOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        })
    }
}

/// [`gateway`] with a [`RecordingPodman`]: what it was asked to stop.
pub(crate) async fn gateway_recording(
    sd: &MockServer,
    rows: &[NewImageModel],
) -> (SharedState, Gw, Arc<RecordingPodman>) {
    let podman = Arc::new(RecordingPodman::default());
    let (state, gw) = gateway_with(podman.clone(), vec![sd.address().port()], rows).await;
    (state, gw, podman)
}

/// [`gateway`], each container start on the next of `ports` (the last one
/// for every start after it).
pub(crate) async fn gateway_on(ports: Vec<u16>, rows: &[NewImageModel]) -> (SharedState, Gw) {
    gateway_with(Arc::new(FakePodman), ports, rows).await
}

/// [`gateway_on`], with `podman` as the registry's.
async fn gateway_with(
    podman: Arc<dyn lmgw_core::runtime::registry::CommandRunner>,
    ports: Vec<u16>,
    rows: &[NewImageModel],
) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let ports = std::sync::Mutex::new(std::collections::VecDeque::from(ports));
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        podman,
        reqwest::Client::new(),
        Arc::new(move || {
            let mut p = ports.lock().unwrap();
            Ok(if p.len() > 1 {
                p.pop_front().unwrap()
            } else {
                p[0]
            })
        }),
    )));
    let mut s = Settings::default();
    s.vram.load_timeout_seconds = 5;
    let dir = state.data_dir.join("image-models");
    std::fs::create_dir_all(&dir).unwrap();
    s.image.models_dir = dir.display().to_string();
    s.container_prefix = "lmgwtest".into();
    store::save_settings(&state.db, &s).await.unwrap();
    for row in rows {
        store::insert_image_model(&state.db, row).await.unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

/// The test picture: a 64×64 PNG. A completed job's picture ([`job`]).
pub(crate) fn png() -> Vec<u8> {
    lmgw_core::bench::png::solid_red()
}

/// A picture an edit or a variation starts from: the test picture with
/// bytes after its end, so a completed job's [`png`] is never its input
/// byte for byte — a result that is its input is the call's error
/// (`edit_eligibility`).
pub(crate) fn source_png() -> Vec<u8> {
    let mut p = png();
    p.extend_from_slice(b"source");
    p
}

pub(crate) fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A job poll's body in `status`, as 0518b34 answers it.
pub(crate) fn job(status: &str) -> Value {
    let mut v = json!({
        "id": "job_1", "kind": "img_gen", "status": status, "created": 1000,
        "started": 1000, "completed": null, "queue_position": 0,
        "result": null, "error": null,
    });
    match status {
        "generating" => {
            v["preview"] = json!({"pass": 1, "step": 3, "total_steps": 8, "b64_json": ""});
        }
        "completed" => {
            v["completed"] = json!(2046);
            v["result"] = json!({"images": [{"index": 0, "b64_json": b64(&png())}],
                                 "output_format": "png"});
        }
        "failed" => {
            v["error"] = json!({"code": "generation_failed", "message": "out of memory"});
        }
        _ => {}
    }
    v
}

/// A generating job's poll at `step` of 8 in `pass`, its preview `b64`.
pub(crate) fn generating(pass: u32, step: u32, b64: &str) -> Value {
    let mut v = job("generating");
    v["preview"] = json!({"pass": pass, "step": step, "total_steps": 8, "b64_json": b64});
    v
}

/// sd-server on `sd`: its capabilities, a job submit, and the polls in
/// `polls` order (the last one repeats).
pub(crate) async fn mount_sd(sd: &MockServer, polls: &[Value]) {
    mount_sd_caps(sd, CAPS, polls).await;
}

/// [`mount_sd`] with the capabilities document `caps`.
pub(crate) async fn mount_sd_caps(sd: &MockServer, caps: &str, polls: &[Value]) {
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_string(caps.to_string()))
        .mount(sd)
        .await;
    Mock::given(method("POST"))
        .and(path("/sdcpp/v1/img_gen"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "id": "job_1", "kind": "img_gen", "status": "queued", "created": 1000,
            "poll_url": "/sdcpp/v1/jobs/job_1",
        })))
        .mount(sd)
        .await;
    let last = polls.len().saturating_sub(1);
    for (i, p) in polls.iter().enumerate() {
        let m = Mock::given(method("GET"))
            .and(path("/sdcpp/v1/jobs/job_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(p.clone()))
            .with_priority((i + 1) as u8);
        let m = if i == last { m } else { m.up_to_n_times(1) };
        m.mount(sd).await;
    }
}

/// The JSON bodies sd-server was sent at `/sdcpp/v1/img_gen`.
pub(crate) async fn submitted(sd: &MockServer) -> Vec<Value> {
    sd.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/sdcpp/v1/img_gen")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// An `/mcp` session as the gateway's owner.
async fn session(gw: &Gw) -> String {
    let resp = gw
        .client()
        .post(format!("{gw}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": { "name": "test", "version": "0" } }
        }))
        .send()
        .await
        .unwrap();
    let sid = resp.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    let _ = resp.text().await;
    sid
}

/// One `/mcp` call: its answer.
pub(crate) async fn mcp(gw: &Gw, method: &str, params: Value) -> Value {
    let sid = session(gw).await;
    let text = gw
        .client()
        .post(format!("{gw}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", sid)
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": method, "params": params}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("/mcp is not JSON ({e}): {text}"))
}

/// `tools/call` of `name` on `/mcp`: the `CallToolResult`.
pub(crate) async fn call(gw: &Gw, name: &str, args: Value) -> Value {
    let v = mcp(gw, "tools/call", json!({"name": name, "arguments": args})).await;
    v.get("result").cloned().unwrap_or_else(|| panic!("{v}"))
}

/// The text of a `CallToolResult`'s first block.
pub(crate) fn text_of(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{result}"))
        .to_string()
}

/// A cloud upstream on `mock` with an alias `alias` onto `model`, its
/// published routes those of `task` (an owner override, as a provider with
/// no task in its catalog needs).
pub(crate) async fn cloud_alias(state: &SharedState, mock: &MockServer, alias: &str, task: &str) {
    let up = match state
        .snapshot()
        .upstreams
        .values()
        .find(|u| u.name == "cloud")
    {
        Some(u) => u.id,
        None => store::insert_upstream(
            &state.db,
            &store::NewUpstream {
                name: "cloud".into(),
                protocol: lmgw_core::config::Protocol::OpenaiCompatible,
                kind: lmgw_core::config::UpstreamKind::Generic,
                base_url: format!("{}/v1", mock.uri().trim_end_matches('/')),
                api_key: Some("sk-up".into()),
                extra_headers: vec![],
                timeout_ms: 5_000,
                enabled: true,
                expose_all: false,
                expose_prefix: String::new(),
                supports_responses: false,
                catalog_all_modalities: true,
            },
        )
        .await
        .unwrap(),
    };
    store::insert_alias(
        &state.db,
        &store::NewAlias {
            alias: alias.into(),
            upstream_id: up,
            upstream_model_id: format!("{alias}-model"),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({"capabilities": {"task": task}})),
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// The `request_logs` rows of class `class`, newest last, once `n` are in
/// (rows are written by their own task).
pub(crate) async fn rows_of(state: &SharedState, class: &str, n: usize) -> Vec<Value> {
    for _ in 0..200 {
        let rows: Vec<(String, Option<i64>, i64, Option<String>)> = sqlx::query_as(
            "SELECT requested_alias, images_out, status, ingress_proto FROM request_logs
             WHERE class = ?1 ORDER BY id",
        )
        .bind(class)
        .fetch_all(&state.db)
        .await
        .unwrap();
        if rows.len() >= n {
            return rows
                .into_iter()
                .map(|(alias, images, status, proto)| {
                    json!({"alias": alias, "images_out": images, "status": status, "proto": proto})
                })
                .collect();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("fewer than {n} {class} rows were written");
}
