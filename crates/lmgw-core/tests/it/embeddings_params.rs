//! `/v1/embeddings` beyond `model` and `input`: `dimensions` reaches the
//! upstream in its own spelling and is checked on the way back,
//! `encoding_format` is honoured by the gateway itself, and anything it
//! cannot honour is a 400 rather than a field quietly left out.

use base64::Engine as _;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::common::{serve, Gw};

/// One alias, `emb`, on an upstream of the given protocol and kind at `base`.
async fn world(base: &str, protocol: Protocol, kind: UpstreamKind) -> Gw {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "emb-up".into(),
            protocol,
            kind,
            base_url: base.trim_end_matches('/').to_string(),
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
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "emb".into(),
            upstream_id: up_id,
            upstream_model_id: "tgt-embed".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    serve(state).await
}

fn openai_vectors(vectors: &[&[f32]]) -> Value {
    let data: Vec<Value> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| json!({"object": "embedding", "index": i, "embedding": v}))
        .collect();
    json!({"object": "list", "model": "tgt-embed", "data": data,
           "usage": {"prompt_tokens": 3, "total_tokens": 3}})
}

async fn post(gw: &Gw, body: Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{gw}/v1/embeddings"))
        .json(&body)
        .send()
        .await
        .unwrap();
    (resp.status().as_u16(), resp.json().await.unwrap())
}

/// The bodies the mock upstream received, as JSON.
async fn received(mock: &MockServer) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r: &Request| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn dimensions_reach_an_openai_upstream() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .and(body_partial_json(
            json!({"model": "tgt-embed", "dimensions": 2}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_vectors(&[&[0.6, 0.8]])))
        .expect(1)
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;

    let (status, body) = post(&gw, json!({"model": "emb", "input": "hi", "dimensions": 2})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"][0]["embedding"].as_array().unwrap().len(), 2);
}

/// llama-server has no `dimensions` and answers with the model's own size.
/// Handing that back would give the client vectors of a length it did not
/// ask for, so it is refused, and the refusal says what came back and why.
#[tokio::test]
async fn dimensions_an_upstream_ignores_are_refused_by_name() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(openai_vectors(&[&[0.1, 0.2, 0.3, 0.4]])),
        )
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Openai, UpstreamKind::LlamaServer).await;

    let (status, body) = post(&gw, json!({"model": "emb", "input": "hi", "dimensions": 2})).await;
    assert_eq!(status, 400, "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("'dimensions'")
            && msg.contains("asked for 2")
            && msg.contains("4-dimensional"),
        "{msg}"
    );
    assert!(msg.contains("llama-server"), "the reason is named: {msg}");

    // Asking for the size the model has anyway is not an error.
    let (status, body) = post(&gw, json!({"model": "emb", "input": "hi", "dimensions": 4})).await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn dimensions_become_gemini_output_dimensionality() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/tgt-embed:batchEmbedContents"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "embeddings": [{"values": [0.6, 0.8]}, {"values": [0.8, 0.6]}]
        })))
        .expect(1)
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Gemini, UpstreamKind::Generic).await;

    let (status, body) = post(
        &gw,
        json!({"model": "emb", "input": ["a", "b"], "dimensions": 2}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let sent = received(&mock).await;
    let requests = sent[0]["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2);
    for r in requests {
        assert_eq!(r["outputDimensionality"], 2, "{r}");
    }
}

/// `base64` is a wire format of the answer, so the gateway encodes it: the
/// upstream is asked for floats (it has to be — the vectors are parsed), and
/// the client gets OpenAI's little-endian float32 bytes.
#[tokio::test]
async fn base64_is_encoded_by_the_gateway() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(openai_vectors(&[&[0.25, -1.5, 3.0]])),
        )
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;

    let (status, body) = post(
        &gw,
        json!({"model": "emb", "input": "hi", "encoding_format": "base64"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let b64 = body["data"][0]["embedding"]
        .as_str()
        .expect("a base64 string");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap();
    let floats: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(floats, vec![0.25, -1.5, 3.0]);
    assert!(
        received(&mock).await[0].get("encoding_format").is_none(),
        "the upstream is asked for floats"
    );

    // "float" and an explicit null are the default shape.
    for fmt in [json!("float"), Value::Null] {
        let (status, body) = post(
            &gw,
            json!({"model": "emb", "input": "hi", "encoding_format": fmt}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body["data"][0]["embedding"].is_array(), "{body}");
    }
}

/// What cannot be honoured is refused before anything is sent.
#[tokio::test]
async fn unusable_fields_are_a_400_before_any_upstream_call() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_vectors(&[&[0.1]])))
        .expect(0)
        .mount(&mock)
        .await;
    let gw = world(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;

    for (extra, needle) in [
        (json!({"encoding_format": "binary"}), "'encoding_format'"),
        (json!({"dimensions": 0}), "'dimensions'"),
        (json!({"dimensions": "256"}), "'dimensions'"),
        (json!({"dimensions": 1.5}), "'dimensions'"),
        (json!({"input": [[1, 2, 3]]}), "'input[0]'"),
        (json!({"input": ["ok", 7]}), "'input[1]'"),
    ] {
        let mut body = json!({"model": "emb", "input": "hi"});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, answer) = post(&gw, body.clone()).await;
        assert_eq!(status, 400, "{body} → {answer}");
        let msg = answer["error"]["message"].as_str().unwrap();
        assert!(msg.contains(needle), "{body} → {msg}");
    }
}
