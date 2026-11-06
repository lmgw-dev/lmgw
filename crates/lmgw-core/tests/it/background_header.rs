//! `x-lmgw-background: on|off` (per-request background design): one request
//! as a guest on the GPU, without an alias made for it.
//!
//! Driven over HTTP through the real router, on `support/gpu_world.rs`, as
//! the candidate walk's suite (`candidate_routing.rs`) is: a fake driver
//! whose free memory follows what the fake podman loaded, so a refused
//! start really means the card is full and an eviction really makes room.
//!
//! - [`plain`]: a plain local model (§4) — joined when loaded, started into
//!   free VRAM as a guest, else its row's fallback or `503 gpu_deferred`;
//!   the hold (met mid-admission by the owner's request too), a model no
//!   card holds, a cloud model, the header's `400`.
//! - [`alias`]: on a candidate alias, a guest walk (§5); an alias's deferral
//!   answers `gpu_deferred` on the wire (§8).
//! - [`classes`]: embeddings and images through the same guest branch (§7).
//! - [`routes`]: speech, the voice probe, the counters and systemone too;
//!   a row on the CPU unaffected (§6, §7).
//! - [`runs`]: a `/v1/responses` run's tools as its traffic, and its stream's
//!   close ending a guest's run at once, never the owner's (§7, §9).
//! - [`cancel`]: never preempted, cancelled by closing the request (§9).
//! - [`scope`]: the header read on its own routes only (§11.12), a `HEAD`
//!   as its `GET` (§11.20).
//! - [`visible`]: a local model's deferrals counted on its row (§10).

use std::time::Duration;

use lmgw_core::runtime::registry::RuntimeView;
use lmgw_core::store::{self, NewLocalModel};
use lmgw_core::vram;
use serde_json::{json, Value};

use crate::common::{self, Gw};
use crate::support::gpu_world::Gpu;

mod alias;
mod cancel;
mod classes;
mod plain;
mod routes;
mod runs;
mod scope;
mod visible;

const BACKGROUND: &str = "x-lmgw-background";

async fn world(total: u64, containers: usize) -> (Gpu, Gw) {
    world_with_queue(total, containers, 2).await
}

async fn world_with_queue(total: u64, containers: usize, queue_seconds: u64) -> (Gpu, Gw) {
    let g = Gpu::new(total, containers, queue_seconds).await;
    let gw = common::serve(g.state.clone()).await;
    (g, gw)
}

/// A public chat row on `<id>.gguf` whose fallback is the alias `fallback`.
fn with_fallback(id: &str, fallback: &str) -> NewLocalModel {
    NewLocalModel {
        model_id: id.into(),
        gguf_path: format!("{id}.gguf"),
        params: Default::default(),
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        fallback: Some(fallback.into()),
        capabilities_override: None,
        ladder: vec![],
    }
}

/// Load `model` as the owner would — a request for its own name — and leave
/// it resident and idle.
async fn owner_loads(g: &Gpu, model: &str) {
    vram::admit(&g.state, &g.route(model), model)
        .await
        .unwrap()
        .expect("a local model");
}

/// A chat request for `model`, with `x-lmgw-background` set to `value`
/// (none for `None`).
async fn chat(gw: &Gw, model: &str, value: Option<&str>) -> reqwest::Response {
    let mut req = gw
        .client()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}));
    if let Some(v) = value {
        req = req.header(BACKGROUND, v);
    }
    req.send().await.unwrap()
}

/// [`chat`] as background traffic.
async fn guest_chat(gw: &Gw, model: &str) -> reqwest::Response {
    chat(gw, model, Some("on")).await
}

fn header<'r>(resp: &'r reqwest::Response, name: &str) -> Option<&'r str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

fn view(g: &Gpu, model: &str) -> Option<RuntimeView> {
    g.state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == model)
}

/// The newest request-log row for `alias` (the record is written as the
/// response goes out, so it is waited for).
async fn newest_log(g: &Gpu, alias: &str) -> store::RequestLogRow {
    for _ in 0..200 {
        let rows = store::query_logs(
            &g.state.db,
            &store::LogFilter {
                alias: Some(alias.into()),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if let Some(row) = rows.into_iter().next() {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no request-log row for '{alias}'");
}

/// A chat answer from the local model `model`: 200, no fallback header.
async fn answered_locally(resp: reqwest::Response, model: &str) {
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    assert_eq!(header(&resp, "x-lmgw-fallback"), None);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(
        v["choices"][0]["message"]["content"],
        format!("ok from {model}")
    );
}

/// A deferral of `named`: `503 gpu_deferred` on the wire and in the log, no
/// fallback header, and the message saying what to give a fallback to.
async fn deferred(g: &Gpu, resp: reqwest::Response, named: &str, noun: &str) -> String {
    assert_eq!(resp.status(), 503, "{:?}", resp.headers());
    assert_eq!(header(&resp, "x-lmgw-fallback"), None);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "gpu_deferred", "{v}");
    assert_eq!(v["error"]["type"], "api_error", "{v}");
    let msg = v["error"]["message"].as_str().unwrap().to_string();
    assert!(msg.starts_with(&format!("'{named}' deferred: ")), "{msg}");
    assert!(
        msg.ends_with(&format!("retry later, or give the {noun} a fallback")),
        "{msg}"
    );
    let row = newest_log(g, named).await;
    assert_eq!(row.error_kind.as_deref(), Some("gpu_deferred"), "{row:?}");
    msg
}

async fn hold_on(g: &Gpu) {
    let mut s = g.state.snapshot().settings.clone();
    s.hold.active = true;
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
}

/// Spin until `cond`, in 10 ms steps; bounded, so a broken invariant fails
/// the test instead of hanging it.
async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}
