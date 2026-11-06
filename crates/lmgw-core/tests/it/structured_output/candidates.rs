//! Candidate aliases and structured output: the `json_object` an alias
//! publishes is what its routable candidates say, absent when none is
//! (§11.20), and the early refusal reads the same; the facet refusal is
//! `response_format_unsupported` only when structured output is all that is
//! missing; and the two codes never send a request to another candidate or
//! a fallback (§11.9).

use lmgw_core::store::{NewCandidateAlias, NewLocalModel};
use lmgw_core::vram;
use serde_json::{json, Value};

use crate::common::{serve, Gw};
use crate::support::gpu_world::{Gpu, GIB};

/// A candidate alias as stored, enabling `enabled`, with `fallback`.
fn alias(
    name: &str,
    candidates: &[&str],
    background: bool,
    fallback: Option<&str>,
    enabled: &[&str],
) -> NewCandidateAlias {
    NewCandidateAlias {
        alias: name.into(),
        candidates: candidates.iter().map(|c| c.to_string()).collect(),
        background,
        fallback: fallback.map(str::to_string),
        capabilities_disabled: vec![],
        capabilities_enabled: enabled.iter().map(|f| f.to_string()).collect(),
        enabled: true,
        notes: String::new(),
    }
}

/// A public chat row whose owner's override says `json_object: object`
/// (the sparse weights carry no header, so the override states it all).
fn row(id: &str, object: bool) -> NewLocalModel {
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
        fallback: None,
        capabilities_override: Some(json!({"capabilities": {
            "task": "chat",
            "endpoints": ["/v1/chat/completions"],
            "input_modalities": ["text"],
            "vision": false,
            "structured_output": {"json_schema": true, "json_object": object},
            "source": "owner",
        }})),
        ladder: vec![],
    }
}

fn ask(model: &str, format: Value) -> Value {
    json!({"model": model, "max_tokens": 16, "response_format": format,
           "messages": [{"role": "user", "content": "Reply with ok."}]})
}

fn json_schema() -> Value {
    json!({"type": "json_schema", "json_schema": {"name": "answer",
           "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}}}})
}

async fn send(gw: &Gw, body: Value) -> (u16, Value, Option<String>) {
    let resp = gw
        .client()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let fallback = resp
        .headers()
        .get("x-lmgw-fallback")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    (status, resp.json().await.unwrap_or(Value::Null), fallback)
}

async fn published(gw: &Gw, name: &str) -> Value {
    let v: Value = gw
        .client()
        .get(format!("{gw}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == name)
        .unwrap_or_else(|| panic!("{name} is listed: {v}"))["capabilities"]["structured_output"]
        .clone()
}

#[tokio::test]
async fn a_candidate_aliases_json_object_is_what_its_routable_candidates_say() {
    let g = Gpu::new(24 * GIB, 2, 2).await;
    let gw = serve(g.state.clone()).await;
    g.row(row("takes", true), 4 * GIB).await;
    g.row(row("refuses", false), 4 * GIB).await;
    g.candidate(alias(
        "all",
        &["takes"],
        false,
        None,
        &["structured_output"],
    ))
    .await;
    g.candidate(alias(
        "mixed",
        &["takes", "refuses"],
        false,
        None,
        &["structured_output"],
    ))
    .await;
    g.candidate(alias(
        "empty",
        &["gone"],
        false,
        None,
        &["structured_output"],
    ))
    .await;
    g.candidate(alias("off", &["takes"], false, None, &[]))
        .await;

    assert_eq!(
        published(&gw, "all").await,
        json!({"json_schema": true, "json_object": true})
    );
    assert_eq!(
        published(&gw, "mixed").await,
        json!({"json_schema": true, "json_object": false})
    );
    // Nothing routable says nothing about the type.
    assert_eq!(published(&gw, "empty").await, json!({"json_schema": true}));
    assert_eq!(
        published(&gw, "off").await,
        json!({"json_schema": false, "json_object": false})
    );

    // The early refusal reads the same value, before anything starts.
    let (status, v, _) = send(&gw, ask("mixed", json!({"type": "json_object"}))).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "response_format_unsupported", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'mixed' does not produce json_object"),
        "{v}"
    );
    // An empty set is the alias's own problem, not the type's.
    let (status, v, _) = send(&gw, ask("empty", json!({"type": "json_object"}))).await;
    assert_ne!(v["error"]["code"], "response_format_unsupported", "{v}");
    assert_eq!(v["error"]["code"], "candidate_unavailable", "{status} {v}");
    assert!(g.runs().is_empty(), "nothing started for either");

    // The other type, which every candidate takes, goes.
    let (status, v, _) = send(&gw, ask("mixed", json_schema())).await;
    assert_eq!(status, 200, "{v}");
}

/// Structured output and another facet missing: the generic facet refusal,
/// since a client stepping down its format would still be refused.
#[tokio::test]
async fn the_facet_refusal_is_a_format_code_only_when_the_format_is_all_that_is_missing() {
    let g = Gpu::new(24 * GIB, 1, 2).await;
    let gw = serve(g.state.clone()).await;
    g.row(row("takes", true), 4 * GIB).await;
    g.candidate(alias("writer", &["takes"], false, None, &[]))
        .await;
    let mut body = ask("writer", json_schema());
    body["messages"] = json!([{"role": "user", "content": [
        {"type": "text", "text": "what is this?"},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
    ]}]);
    let (status, v, _) = send(&gw, body).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "unsupported", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("Vision") && msg.contains("Structured output"),
        "{msg}"
    );
    assert!(v["error"]["param"].is_null(), "{v}");
}

/// A schema refusal is the request's answer: no other candidate is tried,
/// no fallback answers — for a guest, whose context refusal would be
/// re-picked, and for the owner (§11.9).
#[tokio::test]
async fn a_schema_refusal_never_repicks_or_falls_back() {
    let g = Gpu::new(24 * GIB, 2, 2).await;
    let gw = serve(g.state.clone()).await;
    g.row(row("p", true), 8 * GIB).await;
    g.row(row("a", true), 8 * GIB).await;
    g.cloud("cloud", None).await;
    for (name, background) in [("jobs", true), ("writer", false)] {
        g.candidate(alias(
            name,
            &["p", "a"],
            background,
            Some("cloud"),
            &["structured_output"],
        ))
        .await;
    }
    for model in ["p", "a"] {
        vram::admit(&g.state, &g.route(model), model)
            .await
            .unwrap()
            .expect("a local model");
    }
    g.world().refuse_schema.insert("p".into());

    for name in ["jobs", "writer"] {
        let (status, v, fallback) = send(&gw, ask(name, json_schema())).await;
        assert_eq!(status, 400, "{name}: {v}");
        assert_eq!(v["error"]["code"], "invalid_json_schema", "{name}: {v}");
        assert_eq!(v["error"]["param"], "response_format", "{v}");
        assert_eq!(fallback, None, "{name}");
    }
    assert_eq!(g.world().schema_refused, ["p", "p"]);
    assert!(g.world().chats.is_empty(), "the alternate was never asked");
    assert!(g.cloud_bodies().await.is_empty(), "nor the fallback");
}
