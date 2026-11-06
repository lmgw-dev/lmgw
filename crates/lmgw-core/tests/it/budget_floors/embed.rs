//! The gateway's own embedding work (corpus and knowledge ingest, re-embed,
//! probes) is under the budget too: an unpriced cloud `embed/…` alias is
//! refused like any other unpriced send (openai egress design §13.3,
//! decision 24), and the refusal reaches the job as its error.

use lmgw_core::config::PriceTier;
use lmgw_core::quickdoc::InProcessEmbedder;
use lmgw_core::store;
use quickdoc_core::embed::Embedder;

use super::*;

async fn embedding_platform() -> (MockServer, SharedState, Gw) {
    let m = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "model": "tgt-model",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}],
            "usage": {"prompt_tokens": 3, "total_tokens": 3},
        })))
        .mount(&m)
        .await;
    let (state, gw) = setup_kind(&m.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    (m, state, gw)
}

async fn global_budget(state: &SharedState) {
    let mut s = state.snapshot().settings.clone();
    s.global_budget_micro = 1_000_000;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

#[tokio::test]
async fn an_ingest_embed_on_an_unpriced_cloud_alias_is_refused_under_the_global_budget() {
    let (m, state, _gw) = embedding_platform().await;
    global_budget(&state).await;
    let e = InProcessEmbedder::probe(state.clone(), ALIAS)
        .await
        .expect_err("refused");
    assert_eq!(e.kind(), "budget_unpriced", "{e}");
    assert!(
        m.received_requests().await.unwrap().is_empty(),
        "nothing sent"
    );

    // The same job after an input price is entered: it goes through, and
    // the embed call of an ingest batch with it.
    store::upsert_price(
        &state.db,
        lmgw_core::config::PriceScope::Alias,
        ALIAS,
        PriceUnit::PerMtok,
        PriceTier::Standard,
        &Prices {
            price_in: Some(0.1),
            source: PriceSource::Manual,
            ..Default::default()
        },
        None,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let embedder = InProcessEmbedder::probe(state.clone(), ALIAS)
        .await
        .expect("priced");
    embedder.embed(&["a chunk".to_string()]).await.unwrap();
}

#[tokio::test]
async fn an_ingest_without_a_budget_is_not_refused() {
    let (_m, state, _gw) = embedding_platform().await;
    InProcessEmbedder::probe(state.clone(), ALIAS)
        .await
        .expect("no budget, no refusal");
}
