//! The early refusal (§7) beyond the plain unary case: a streamed request
//! is refused before any stream opens, the model judged is the one the hold
//! swapped in, and the check never waits on a provider's model list
//! (§11.21).

use std::time::{Duration, Instant};

use lmgw_core::config::Protocol;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::routes::{
    assert_format_error, chat_body, json_schema, list_models, messages_body, openai_catalog,
    openai_ok, posted, responses_body, schema, world,
};
use crate::support::gpu_world::{Gpu, GIB};

/// A streamed request the flag refuses gets the plain error body, `400` and
/// JSON, on every route — no `200` with an error inside a stream.
#[tokio::test]
async fn a_streamed_request_is_refused_before_any_stream_opens() {
    let mock = MockServer::start().await;
    openai_catalog(&mock).await;
    let caps = json!({"capabilities": {"structured_output": {"json_schema": false}}});
    let (_state, gw) = world(&mock.uri(), Protocol::OpenaiCompatible, false, Some(caps)).await;
    list_models(&gw).await;

    let mut responses = responses_body(json!({"type": "json_schema", "name": "answer",
                                               "schema": schema()}));
    responses["stream"] = json!(true);
    let mut messages = messages_body();
    messages["stream"] = json!(true);
    for (route, body, param) in [
        (
            "/v1/chat/completions",
            chat_body(Some(json_schema()), true),
            Some("response_format"),
        ),
        ("/v1/responses", responses, Some("text.format")),
        ("/v1/messages", messages, None),
    ] {
        let resp = gw
            .client()
            .post(format!("{gw}{route}"))
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{route}");
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(ct.starts_with("application/json"), "{route}: {ct}");
        let v: Value = resp.json().await.unwrap();
        match param {
            Some(param) => assert_format_error(&v, "response_format_unsupported", param),
            None => {
                assert_eq!(v["type"], "error", "{v}");
                assert_eq!(v["error"]["type"], "invalid_request_error", "{v}");
            }
        }
    }
    assert!(posted(&mock).await.is_empty(), "nothing went upstream");
}

/// A catalog never read, whose provider takes a minute to list: the JSON
/// request is answered at once (unknown, so it goes), and the catalog is
/// read behind it for the next one (§11.21).
#[tokio::test]
async fn a_cold_catalog_does_not_hold_up_a_json_request() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"object": "list", "data": [{"id": "tgt"}]}))
                .set_delay(Duration::from_secs(60)),
        )
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_ok())
        .mount(&mock)
        .await;
    let (_state, gw) = world(&mock.uri(), Protocol::OpenaiCompatible, false, None).await;

    let started = Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_secs(20),
        gw.client()
            .post(format!("{gw}/v1/chat/completions"))
            .json(&chat_body(Some(json!({"type": "json_object"})), false))
            .send(),
    )
    .await
    .expect("the request waited on the catalog")
    .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(posted(&mock).await.len(), 1, "it went upstream");
}

/// A public local chat row on a sparse `<id>.gguf`, with the owner's
/// capabilities override (the sparse file has no header to read).
fn local_row(id: &str, caps: Value) -> store::NewLocalModel {
    store::NewLocalModel {
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
        fallback: None,
        capabilities_override: Some(caps),
        ladder: vec![],
    }
}

/// A whole chat capability object whose `structured_output` says
/// `json_object: object`.
fn chat_caps(object: Option<bool>) -> Value {
    json!({"capabilities": {
        "task": "chat",
        "endpoints": ["/v1/chat/completions"],
        "input_modalities": ["text"],
        "structured_output": {"json_schema": true, "json_object": object},
        "source": "owner",
    }})
}

/// Under the hold the row's fallback answers, so it is the one judged
/// (§11.7): its `false` refuses a request the local row would take, and the
/// local row's `false` does not refuse a request the fallback takes.
#[tokio::test]
async fn the_hold_swaps_in_the_model_that_is_judged() {
    let g = Gpu::new(24 * GIB, 1, 2).await;
    let gw = crate::common::serve(g.state.clone()).await;
    g.row(local_row("takes", chat_caps(Some(true))), GIB).await;
    g.row(local_row("refuses", chat_caps(Some(false))), GIB)
        .await;
    g.cloud("cloud-refuses", Some(chat_caps(Some(false)))).await;
    g.cloud("cloud-takes", Some(chat_caps(None))).await;
    let ask = |model: &str| {
        json!({"model": model, "max_tokens": 16, "response_format": {"type": "json_object"},
               "messages": [{"role": "user", "content": "Reply with ok."}]})
    };
    let hold = |fallback: &'static str| {
        let g = &g;
        async move {
            let mut s = g.state.snapshot().settings.clone();
            s.hold.active = true;
            store::save_settings(&g.state.db, &s).await.unwrap();
            g.state.reload_snapshot().await.unwrap();
            crate::support::fallbacks::every_chat_model_falls_back_to(&g.state, Some(&*fallback))
                .await;
        }
    };
    let send = |model: &'static str| {
        let gw = &gw;
        let body = ask(model);
        async move {
            gw.client()
                .post(format!("{gw}/v1/chat/completions"))
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };

    hold("cloud-refuses").await;
    let resp = send("takes").await;
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert_format_error(&v, "response_format_unsupported", "response_format");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("'cloud-refuses'"),
        "the fallback is named: {msg}"
    );

    hold("cloud-takes").await;
    let resp = send("refuses").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-takes")
    );
    assert_eq!(g.cloud_bodies().await.len(), 1, "the fallback answered");
    assert!(g.runs().is_empty(), "nothing local started under the hold");
}
