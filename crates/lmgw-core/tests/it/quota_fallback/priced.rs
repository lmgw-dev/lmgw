//! The priced check (decision 24) on the route a non-chat request is served
//! on (openai egress design §14.9): a held embedder whose fallback is an
//! unpriced cloud encoder is refused `budget_unpriced` under a budget, as a
//! request naming that encoder would be.

use lmgw_core::config::AuxKind;
use lmgw_core::runtime::Class;

use super::*;

/// The embedder `bge`, its fallback the unpriced cloud encoder
/// `cloud-embed`, a global budget; the encoder's mock and `bge`'s public
/// name.
async fn embedder_with_unpriced_fallback(g: &Gpu) -> (MockServer, String) {
    g.aux("bge", AuxKind::Embed, GIB).await;
    let cloud = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "model": "gpt-test",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
            "usage": {"prompt_tokens": 2, "total_tokens": 2},
        })))
        .mount(&cloud)
        .await;
    upstream_alias(g, "cloud-embed", &cloud.uri(), Protocol::Openai).await;
    set_fallback(&g.state, "aux_models", "bge", Some("cloud-embed")).await;
    let name = {
        let snap = g.state.snapshot();
        snap.aux_public_name(&snap.aux_models[0])
    };
    let mut s = g.state.snapshot().settings.clone();
    s.global_budget_micro = 1_000_000;
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
    (cloud, name)
}

/// A row switched to the CPU whose container still runs on the GPU under
/// the hold hands the request to its fallback at resolve (the gate's static
/// walk): the priced check runs on that fallback too (review B7).
#[tokio::test]
async fn a_cpu_row_still_on_the_gpu_under_the_hold_is_checked_on_its_fallback() {
    let (g, gw) = world().await;
    let (cloud, name) = embedder_with_unpriced_fallback(&g).await;
    let snap = g.state.snapshot();
    let rt = lmgw_core::runtime::descriptor::model_runtime(&snap, Class::Aux, "bge").unwrap();
    let spec = lmgw_core::runtime::lifecycle::acquire_spec(&g.state, &snap, &rt);
    let busy = g.state.runtime().acquire(&spec).await.unwrap();
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    sqlx::query("UPDATE aux_models SET backend = 'cpu' WHERE model_id = 'bge'")
        .execute(&g.state.db)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();

    let resp = gw
        .client()
        .post(format!("{gw}/v1/embeddings"))
        .json(&json!({"model": name, "input": "hello"}))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "budget_unpriced", "{status} {v}");
    assert!(
        cloud.received_requests().await.unwrap().is_empty(),
        "nothing sent"
    );
    drop(busy);
}

#[tokio::test]
async fn a_held_embedder_whose_fallback_is_unpriced_is_refused_under_a_budget() {
    let (g, gw) = world().await;
    let (cloud, name) = embedder_with_unpriced_fallback(&g).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();

    let resp = gw
        .client()
        .post(format!("{gw}/v1/embeddings"))
        .json(&json!({"model": name, "input": "hello"}))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "budget_unpriced", "{status} {v}");
    assert!(
        cloud.received_requests().await.unwrap().is_empty(),
        "nothing sent"
    );
    assert!(g.runs().is_empty(), "{:?}", g.runs());
}
