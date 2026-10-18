//! OpenRouter as a decision upstream (decision-models design §4.2, §4.4,
//! §4.5, §6.2), against a mock that answers as OpenRouter did on 2026-10-14:
//! its default `GET /models` lists only the models whose output is text, and
//! `?output_modalities=all` adds Jev and every other non-text model
//! (`fixtures/catalogs/openrouter_all_modalities.json`).
//!
//! - The catalog request carries `output_modalities=all`, so `/v1/models`
//!   publishes Jev as a decision model and the other non-text outputs under
//!   the tasks they are; an upstream whose option is off asks bare, and one
//!   that refuses the parameter, as Gemini's OpenAI-compatible endpoint does,
//!   names the option in its error.
//! - Price sync turns Jev's catalog price into a `per_mtok` row, $0.042 in
//!   and 0 out, and a System One answer (slice 1's documented OpenRouter
//!   body) is priced from it.
//! - A held GPU decision row falls back to `jev` and is priced under `jev`.

use lmgw_core::config::{
    upstream_scope_key, AuxKind, HoldFallbackMode, PriceScope, PriceUnit, Protocol, UpstreamKind,
};
use lmgw_core::pricing::PriceSource;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewAuxModel, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};
use crate::stream_usage::row_after;
use crate::support::decision_world::{decision_world, OPENROUTER_ANSWER};

/// OpenRouter's full list, as captured.
const ALL: &str = include_str!("../fixtures/catalogs/openrouter_all_modalities.json");

/// The fixture's entries whose outputs include text: what OpenRouter's
/// default list holds of them.
fn default_list() -> Value {
    let all: Value = serde_json::from_str(ALL).unwrap();
    let text: Vec<Value> = all["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| {
            m["architecture"]["output_modalities"]
                .as_array()
                .is_some_and(|o| o.iter().any(|x| x == "text"))
        })
        .cloned()
        .collect();
    json!({ "data": text })
}

/// A mock OpenRouter under `/api/v1`: the full list to a catalog request
/// with `output_modalities=all`, the default list to one without, and the
/// documented System One answer.
async fn openrouter() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .and(query_param("output_modalities", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ALL, "application/json"))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .and(query_param_is_missing("output_modalities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(default_list()))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/systemone"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(OPENROUTER_ANSWER, "application/json"),
        )
        .mount(&mock)
        .await;
    mock
}

/// §4.2's row on `state`: protocol `openai`, `expose_all` under the prefix
/// `openrouter`, and the alias `jev` on Jev's pinned id. Its id.
async fn add_openrouter(state: &SharedState, mock: &MockServer) -> i64 {
    let id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "openrouter".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/api/v1", mock.uri()),
            api_key: Some("sk-or".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: true,
            expose_prefix: "openrouter".into(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "jev".into(),
            upstream_id: id,
            upstream_model_id: "typesafe/jev-1.13".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    id
}

async fn get(gw: &Gw, route: &str) -> (u16, Value) {
    let resp = gw
        .client()
        .get(format!("{gw}{route}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn op(gw: &Gw, name: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The `/v1/models` entry of `id`.
fn entry_of<'a>(models: &'a Value, id: &str) -> &'a Value {
    models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{id} not in /v1/models: {models}"))
}

/// The capabilities `/v1/models` publishes for `id`.
fn caps_of<'a>(models: &'a Value, id: &str) -> &'a Value {
    &entry_of(models, id)["capabilities"]
}

/// The query strings of the catalog requests `mock` received, in order.
async fn catalog_queries(mock: &MockServer) -> Vec<Option<String>> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/models"))
        .map(|r| r.url.query().map(String::from))
        .collect()
}

/// One question for `model`.
fn ask(model: &str) -> Value {
    json!({"model": model, "state": "I was charged twice for my subscription.",
           "questions": {"refund": {"type": "noul", "instructions": "Refund?"}}})
}

/// §6.2 acceptance step 3, against the mock: the catalog request asks for
/// every output modality, and `/v1/models` lists Jev, by its passthrough name
/// and by the alias, as a decision model on `/v1/systemone`. Every other
/// output the full list adds publishes as the task it is, never as chat; a
/// model that outputs text as well stays chat.
#[tokio::test]
async fn the_catalog_asks_for_every_modality_and_publishes_jev_as_a_decision_model() {
    let mock = openrouter().await;
    let state = AppState::init_for_tests().await.unwrap();
    add_openrouter(&state, &mock).await;
    let gw = serve(state).await;

    let (status, models) = get(&gw, "/v1/models").await;
    assert_eq!(status, 200, "{models}");
    for id in [
        "openrouter/typesafe/jev-1.13",
        "openrouter/~typesafe/jev-latest",
        "jev",
    ] {
        let caps = caps_of(&models, id);
        assert_eq!(caps["task"], "decision", "{id}: {caps}");
        assert_eq!(caps["endpoints"], json!(["/v1/systemone"]), "{id}: {caps}");
        assert_eq!(
            caps["output_modalities"],
            json!(["decisions"]),
            "{id}: {caps}"
        );
    }
    // Each task's routes are the ones OpenRouter documents (§6.2): POST
    // /embeddings, /rerank, /audio/speech, /audio/transcriptions and
    // /systemone under its base, which lmgw's routes post to. Its images
    // (POST /images) and videos (POST /videos, asynchronous) are reached by
    // no lmgw route, so those models publish none and say why; a model that
    // outputs text and images stays chat and keeps its image output.
    for (id, task, endpoints) in [
        ("typesafe/jev-router", "chat", None),
        ("liquid/lfm-2.5-24b-a2b", "chat", None),
        (
            "liquid/lfm-2.5-embedding-350m:free",
            "embedding",
            Some(json!(["/v1/embeddings"])),
        ),
        (
            "voyageai/rerank-3-lite",
            "rerank",
            Some(json!(["/v1/rerank"])),
        ),
        (
            "fish-audio/drama-3",
            "tts",
            Some(json!(["/v1/audio/speech"])),
        ),
        (
            "elevenlabs/scribe-v2",
            "asr",
            Some(json!(["/v1/audio/transcriptions"])),
        ),
        (
            "tencent/hy-image-v3.5-preview",
            "image_generation",
            Some(json!([])),
        ),
        (
            "x-ai/grok-imagine-video-1.5-lite",
            "video_generation",
            Some(json!([])),
        ),
    ] {
        let caps = caps_of(&models, &format!("openrouter/{id}"));
        assert_eq!(caps["task"], task, "{id}: {caps}");
        if let Some(e) = endpoints {
            assert_eq!(caps["endpoints"], e, "{id}: {caps}");
        }
    }
    for id in [
        "openrouter/tencent/hy-image-v3.5-preview",
        "openrouter/x-ai/grok-imagine-video-1.5-lite",
    ] {
        let notes = entry_of(&models, id)["notes"].to_string();
        assert!(
            notes.contains("No lmgw route is verified") && notes.contains("POST /images"),
            "{id}: {notes}"
        );
    }
    let banana = caps_of(&models, "openrouter/google/gemini-nano-banana-2.1");
    assert_eq!(banana["task"], "chat", "{banana}");
    assert!(
        banana["endpoints"]
            .as_array()
            .unwrap()
            .contains(&json!("/v1/chat/completions")),
        "{banana}"
    );
    assert_eq!(banana["output_modalities"], json!(["image", "text"]));
    // The Image lab offers what serves /v1/images/generations, and so not
    // OpenRouter's image models.
    let (status, lab) = get(&gw, "/image-lab/api/models").await;
    assert_eq!(status, 200, "{lab}");
    assert!(!lab.to_string().contains("hy-image"), "{lab}");
    let queries = catalog_queries(&mock).await;
    assert!(!queries.is_empty());
    assert!(
        queries
            .iter()
            .all(|q| q.as_deref() == Some("output_modalities=all")),
        "{queries:?}"
    );
}

/// An upstream that refuses the parameter (Gemini's OpenAI-compatible
/// endpoint answers 400 "Unknown name \"output_modalities\"") fails its
/// catalog with the option named, and so does its Test; with the option
/// turned off through `upstream_set` the request goes bare and the catalog
/// lists. `GET /api/upstreams` says which way the row asks.
#[tokio::test]
async fn an_upstream_that_refuses_the_parameter_names_the_option_and_lists_once_it_is_off() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/openai/models"))
        .and(query_param("output_modalities", "all"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": 400, "status": "INVALID_ARGUMENT",
            "message": "Invalid JSON payload received. Unknown name \"output_modalities\": \
                        Cannot bind query parameter. Field 'output_modalities' could not be \
                        found in request message."}})))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/openai/models"))
        .and(query_param_is_missing("output_modalities"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list",
            "data": [{"id": "models/gemini-3-flash", "object": "model", "owned_by": "google"}]})),
        )
        .mount(&mock)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    let id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "gemini-compat".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1beta/openai", mock.uri()),
            api_key: Some("g-key".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: true,
            expose_prefix: "gc".into(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state).await;
    let view = |v: &Value| {
        v["upstreams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["id"] == id)
            .unwrap()["catalog_all_modalities"]
            .clone()
    };

    let (_, ups) = get(&gw, "/api/upstreams").await;
    assert_eq!(view(&ups), json!(true), "a row starts on: {ups}");
    let (status, body) = get(&gw, &format!("/api/upstream-models?id={id}")).await;
    assert_ne!(status, 200, "{body}");
    assert!(
        body.to_string().contains("400") && body.to_string().contains("catalog_all_modalities"),
        "{body}"
    );
    let (status, body) = op(&gw, "upstream_test", json!({"id": id})).await;
    assert!(
        body.to_string().contains("catalog_all_modalities"),
        "{status}: {body}"
    );

    let (status, saved) = op(
        &gw,
        "upstream_set",
        json!({"action": "update", "id": id, "catalog_all_modalities": false}),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    let (_, ups) = get(&gw, "/api/upstreams").await;
    assert_eq!(view(&ups), json!(false), "{ups}");
    let (status, body) = get(&gw, &format!("/api/upstream-models?id={id}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["models"], json!(["models/gemini-3-flash"]), "{body}");
    assert_eq!(
        catalog_queries(&mock).await.last(),
        Some(&None),
        "the last catalog request went bare"
    );

    // The dashboard's form turns it back on.
    let (status, saved) = op(
        &gw,
        "upstream_set_full",
        json!({"action": "update", "id": id, "catalog_all_modalities": true}),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    let (_, ups) = get(&gw, "/api/upstreams").await;
    assert_eq!(view(&ups), json!(true), "{ups}");
}

/// §6.2 acceptance step 2 and slice 1's wiremock OpenRouter, now priced from
/// the catalog (§4.4): price sync writes Jev's `per_mtok` row from
/// OpenRouter's per-token strings, $0.042 in and 0 out, under the alias and,
/// the upstream being `expose_all`, under the upstream scope its passthrough
/// name prices through; the moving id no alias names gets the upstream scope
/// alone. The documented answer's 275 input tokens cost 275 × 0.042
/// micro-dollars and its 20 output tokens nothing, through `jev` and through
/// both passthrough names.
///
/// What the catalog's token fields cannot price is left unpriced, with no
/// row: the speech, transcription, rerank, image and video models (billed per
/// character, second, search unit or image), and a router's `-1`. The free
/// embedder (every field 0) keeps its 0 row.
#[tokio::test]
async fn price_sync_prices_jev_from_the_catalog_and_its_answer_is_priced_from_that_row() {
    let mock = openrouter().await;
    let state = AppState::init_for_tests().await.unwrap();
    let id = add_openrouter(&state, &mock).await;
    let gw = serve(state.clone()).await;

    let (status, summary) = op(&gw, "prices_sync", json!({})).await;
    assert_eq!(status, 200, "{summary}");
    let or = summary["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["upstream_id"] == id)
        .unwrap_or_else(|| panic!("{summary}"));
    assert_eq!(or["unpriced_models"], 6, "{or}");
    let rows = store::list_prices(&state.db).await.unwrap();
    let catalog_key = |r: &&lmgw_core::config::PriceRow| {
        r.source == PriceSource::Catalog && r.scope_key.starts_with(&format!("{id}:"))
    };
    let mut priced: Vec<&str> = rows
        .iter()
        .filter(catalog_key)
        .map(|r| r.scope_key.as_str())
        .collect();
    priced.sort_unstable();
    let key = |m: &str| upstream_scope_key(id, m);
    assert_eq!(
        priced,
        [
            key("google/gemini-nano-banana-2.1"),
            key("liquid/lfm-2.5-24b-a2b"),
            key("liquid/lfm-2.5-embedding-350m:free"),
            key("typesafe/jev-1.13"),
            key("~typesafe/jev-latest"),
        ],
        "{summary}"
    );
    for (scope, key) in [
        (PriceScope::Alias, "jev".to_string()),
        (PriceScope::UpstreamModel, key("typesafe/jev-1.13")),
        (PriceScope::UpstreamModel, key("~typesafe/jev-latest")),
    ] {
        let row = rows
            .iter()
            .find(|r| r.scope_kind == scope && r.scope_key == key)
            .unwrap_or_else(|| panic!("no price row for {key}: {summary}"));
        assert_eq!(
            (row.unit, row.source),
            (PriceUnit::PerMtok, PriceSource::Catalog)
        );
        let (price_in, price_out) = (row.price_in.unwrap(), row.price_out.unwrap());
        assert!((price_in - 0.042).abs() < 1e-12, "{key}: {price_in}");
        assert_eq!(price_out, 0.0, "{key}");
    }

    let in_micro = (275.0_f64 * 0.042).round() as i64;
    for (n, model) in [
        "jev",
        "openrouter/typesafe/jev-1.13",
        "openrouter/~typesafe/jev-latest",
    ]
    .iter()
    .enumerate()
    {
        let resp = gw
            .client()
            .post(format!("{gw}/v1/systemone"))
            .json(&ask(model))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{model}");
        assert_eq!(resp.text().await.unwrap(), OPENROUTER_ANSWER);
        let row = row_after(&state, n).await;
        assert_eq!(row.requested_alias, *model);
        assert_eq!(
            (row.prompt_tokens, row.completion_tokens),
            (Some(275), Some(20)),
            "{row:?}"
        );
        assert_eq!(
            (row.cost_micro, row.cost_in_micro, row.cost_out_micro),
            (Some(in_micro), Some(in_micro), Some(0)),
            "{row:?}"
        );
        assert_eq!(row.price_source.as_deref(), Some("catalog"), "{row:?}");
    }
}

/// §4.5 and §6.2's fallback test: under the hold a GPU decision row whose
/// fallback is `jev` answers through OpenRouter, says so in
/// `x-lmgw-fallback`, sends Jev's pinned id upstream, starts no container,
/// and its row is priced under `jev` from the synced catalog row while
/// `requested_alias` stays the row's own name.
#[tokio::test]
async fn a_held_gpu_decision_row_falls_back_to_jev_and_is_priced_under_jev() {
    let mock = openrouter().await;
    let w = decision_world("http://127.0.0.1:1").await;
    add_openrouter(&w.state, &mock).await;
    store::insert_aux_model(
        &w.state.db,
        &NewAuxModel {
            model_id: "laya-gpu".into(),
            gguf_path: "laya.gguf".into(),
            kind: AuxKind::Decision,
            pooling: None,
            ctx_size: None,
            ubatch_size: None,
            batch_size: None,
            mmproj_path: None,
            backend: None,
            threads: None,
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: HoldFallbackMode::Alias,
            hold_fallback: Some("jev".into()),
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    lmgw_core::catalog::sync_prices(&w.state).await.unwrap();
    lmgw_core::ops::hold_set(&w.state, true).await.unwrap();

    let name = w.name("laya-gpu");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/systemone", w.gw))
        .json(&ask(&name))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let h = |n: &str| {
        resp.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    assert_eq!(
        (h("x-lmgw-fallback"), h("x-lmgw-fallback-reason")),
        (Some("jev".into()), Some("hold".into()))
    );
    assert_eq!(resp.text().await.unwrap(), OPENROUTER_ANSWER);

    let sent: Vec<String> = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/systemone"))
        .map(|r| String::from_utf8(r.body.clone()).unwrap())
        .collect();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        sent[0].contains(r#""model":"typesafe/jev-1.13""#),
        "{sent:?}"
    );
    assert_eq!(w.podman.runs(), 0, "the held row's container never starts");

    let row = row_after(&w.state, 0).await;
    assert_eq!(row.requested_alias, name);
    assert_eq!(row.fallback_reason.as_deref(), Some("hold"), "{row:?}");
    assert_eq!(row.upstream_name.as_deref(), Some("openrouter"), "{row:?}");
    assert_eq!(row.upstream_model.as_deref(), Some("typesafe/jev-1.13"));
    let in_micro = (275.0_f64 * 0.042).round() as i64;
    assert_eq!(
        (row.cost_micro, row.cost_in_micro, row.cost_out_micro),
        (Some(in_micro), Some(in_micro), Some(0)),
        "{row:?}"
    );
    assert_eq!(row.price_source.as_deref(), Some("catalog"), "{row:?}");
}
