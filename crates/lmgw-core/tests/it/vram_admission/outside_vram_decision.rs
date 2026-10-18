//! Outside-VRAM fallback for a decision row (decision-models design §2.4,
//! candidate-aliases design §4.7): the verdict that swaps a request to its
//! row's fallback asks first whether that fallback can take it
//! (`gate::fallback_serves`), with the route's own check — for
//! `/v1/systemone` that is `RouteCheck::Decision`. A fallback whose catalog
//! says it outputs text cannot, so the request waits for the local model as
//! it would with no fallback; one that outputs decisions answers at once.

use super::*;
use crate::support::decision_world::{public_name, TYPESAFE_ANSWERS};

/// The fixture's embedder row as a decision row, its fallback `alias`.
async fn as_decision_row_falling_back_to(f: &Fixture, alias: &str) {
    let aux = f.state.snapshot().aux_models[0].clone();
    store::update_aux_model(
        &f.state.db,
        aux.id,
        &NewAuxModel {
            model_id: aux.model_id.clone(),
            gguf_path: aux.gguf_path.clone(),
            kind: AuxKind::Decision,
            pooling: None,
            ctx_size: aux.ctx_size,
            ubatch_size: None,
            batch_size: None,
            mmproj_path: None,
            backend: None,
            threads: None,
            args: aux.args.clone(),
            idle_seconds: aux.idle_seconds,
            enabled: aux.enabled,
            image: aux.image.clone(),
            extra_run_args: aux.extra_run_args.clone(),
            warm_start: aux.warm_start,
            hold_fallback_mode: HoldFallbackMode::Alias,
            hold_fallback: Some(alias.into()),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// A cloud upstream whose catalog lists `jev` as a decision model and `gpt`
/// as a text one, with the aliases `decider` and `chatter` on them.
async fn cloud_decider(f: &Fixture) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": [
                {"id": "jev", "architecture": {"output_modalities": ["decisions"]}},
                {"id": "gpt", "architecture": {"output_modalities": ["text"]}},
            ]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(TYPESAFE_ANSWERS[0], "application/json"),
        )
        .mount(&server)
        .await;
    let upstream_id = store::insert_upstream(
        &f.state.db,
        &store::NewUpstream {
            name: "cloud".into(),
            protocol: lmgw_core::config::Protocol::Openai,
            kind: lmgw_core::config::UpstreamKind::Generic,
            base_url: format!("{}/v1", server.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    for (alias, model) in [("decider", "jev"), ("chatter", "gpt")] {
        store::insert_alias(
            &f.state.db,
            &store::NewAlias {
                alias: alias.into(),
                upstream_id,
                upstream_model_id: model.into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: None,
            },
        )
        .await
        .unwrap();
    }
    f.state.reload_snapshot().await.unwrap();
    server
}

async fn ask(f: &Fixture, model: &str) -> reqwest::Response {
    f.gateway
        .client()
        .post(format!("{}/v1/systemone", f.gateway))
        .json(
            &json!({"model": model, "state": "Help! My payouts have been failing.",
                      "questions": {"is_urgent": {"type": "noul",
                                                  "instructions": "Does this convey urgency?"}}}),
        )
        .send()
        .await
        .unwrap()
}

/// Outside use leaves no room for the decision row. Its fallback `chatter`
/// outputs text, so the verdict's re-check refuses it and the request waits
/// for the card until the queue gives up, as it would with no fallback,
/// and nothing is sent to the cloud. Given `decider`, which outputs
/// decisions, the same request goes there at once, the headers saying so.
#[tokio::test]
async fn outside_use_sends_a_decision_only_to_a_fallback_that_answers_system_one() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(6 * GIB);
    let cloud = cloud_decider(&f).await;
    set_queue_timeout(&f, 1).await;
    as_decision_row_falling_back_to(&f, "chatter").await;
    // Its name as the decision row it now is (`decide/…`, §3.10).
    let model = public_name(&f.state, "embed-model");
    let resp = ask(&f, &model).await;
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().get("x-lmgw-fallback").is_none());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["lmgw_code"], "vram_queue_timeout", "{body}");
    let sent = cloud.received_requests().await.unwrap();
    assert!(
        !sent.iter().any(|r| r.url.path().ends_with("/systemone")),
        "nothing goes to a fallback that cannot answer"
    );

    as_decision_row_falling_back_to(&f, "decider").await;
    let waits = f.state.vram.waits_begun();
    let resp = ask(&f, &model).await;
    assert_eq!(f.state.vram.waits_begun(), waits, "never queued");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("decider")
    );
    assert_eq!(fallback_reason(&resp), Some("external_vram"));
    assert_eq!(resp.text().await.unwrap(), TYPESAFE_ANSWERS[0]);
    assert!(f.runs().is_empty(), "{:?}", f.runs());
}
