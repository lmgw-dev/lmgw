//! A budget is enforced only on what is priced (openai egress design §13.3,
//! decision 24): a budgeted caller's request on a cloud model whose scope
//! prices nothing it is billed in is refused before the send, on the route
//! that would have answered, with a code of its own, `budget_unpriced`.
//! Flex without a flex price is one case: Standard is no lower bound for
//! flex, so a flex answer on a scope with no flex price would spend what no
//! budget sees.

use lmgw_core::store::{NewAlias, NewUpstream};

use crate::support::gpu_world::{Gpu, GIB};

use super::*;

const MESSAGE: &str = "key 'flexer' has a budget, and 'tgt-model' on upstream 'test-up' has no \
                       flex input and output token price on file (a standard one is), so its \
                       spend could not be counted: enter one on the Prices page or with \
                       lmgw__price_set (a manual price of 0 is fine for a free upstream, such \
                       as one on your LAN), or send service_tier default";

/// The refusal of a request on a scope with no price at all.
const NO_PRICE: &str = "key 'flexer' has a budget, and 'tgt-model' on upstream 'test-up' has no \
                        standard input and output token price on file (none is), so its spend \
                        could not be counted: enter one on the Prices page or with \
                        lmgw__price_set (a manual price of 0 is fine for a free upstream, such \
                        as one on your LAN)";

fn flex() -> Value {
    json!({"service_tier": "flex"})
}

/// The refusal as the client and the row see it: nothing was sent.
async fn assert_refused(m: &MockServer, state: &SharedState, status: u16, body: &Value) {
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "budget_unpriced", "{body}");
    assert_eq!(body["error"]["type"], "permission_error", "{body}");
    assert_eq!(body["error"]["message"], MESSAGE, "{body}");
    assert!(
        m.received_requests().await.unwrap().is_empty(),
        "nothing sent"
    );
    let row = row_after(state, 0).await;
    assert_eq!(
        row.error_kind.as_deref(),
        Some("budget_unpriced"),
        "{row:?}"
    );
    assert_eq!(
        row.upstream_name.as_deref(),
        Some("test-up"),
        "names the route"
    );
    assert_eq!(row.status, 403);
}

#[tokio::test]
async fn a_budgeted_key_s_flex_request_without_a_flex_price_is_refused() {
    let (m, state, gw) =
        platform(ResponseTemplate::new(200).set_body_json(answer(Some("flex")))).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    budgeted_key(&state, "flexer", 1_000_000).await;
    let (status, body) = chat(&gw, flex(), Some("lmgw-flexer")).await;
    assert_refused(&m, &state, status, &body).await;

    // Streamed: refused before the stream opens, as a plain error.
    let (status, body) = chat(
        &gw,
        json!({"service_tier": "flex", "stream": true}),
        Some("lmgw-flexer"),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "budget_unpriced", "{body}");

    // Any other tier is not refused: the standard price bounds it.
    let (status, _) = chat(
        &gw,
        json!({"service_tier": "priority"}),
        Some("lmgw-flexer"),
    )
    .await;
    assert_eq!(status, 200);
}

/// The global budget counts as "budgeted" (decided 2026-10-24, §13.6):
/// every request is under it, a keyless one included.
#[tokio::test]
async fn the_global_budget_refuses_it_too() {
    let (m, state, gw) =
        platform(ResponseTemplate::new(200).set_body_json(answer(Some("flex")))).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let mut s = state.snapshot().settings.clone();
    s.global_budget_micro = 1_000_000;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let (status, body) = chat(&gw, flex(), None).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "budget_unpriced", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("this gateway has a budget, and 'tgt-model'"),
        "{body}"
    );
    assert!(m.received_requests().await.unwrap().is_empty());
}

/// Not refused without a budget, or with a flex price on file.
#[tokio::test]
async fn it_passes_without_a_budget_or_with_a_flex_row() {
    // No budget.
    let (_m, state, gw) =
        platform(ResponseTemplate::new(200).set_body_json(answer(Some("flex")))).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    assert_eq!(chat(&gw, flex(), None).await.0, 200);

    // A flex row: priced at it.
    let (_m, state, gw) =
        platform(ResponseTemplate::new(200).set_body_json(answer(Some("flex")))).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    price(&state, PriceTier::Flex, 1.0, 4.0).await;
    budgeted_key(&state, "flexer", 1_000_000).await;
    assert_eq!(chat(&gw, flex(), Some("lmgw-flexer")).await.0, 200);
    assert_eq!(row_after(&state, 0).await.cost_micro, Some(FLEX_MICRO));
}

/// A budgeted key on a cloud model with no price at all is refused, flex
/// or not (decision 24, replacing the earlier decision that let it pass).
#[tokio::test]
async fn a_budgeted_key_on_an_unpriced_model_is_refused() {
    let (m, state, gw) =
        platform(ResponseTemplate::new(200).set_body_json(answer(Some("default")))).await;
    budgeted_key(&state, "flexer", 1_000_000).await;
    let (status, body) = chat(&gw, json!({}), Some("lmgw-flexer")).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "budget_unpriced", "{body}");
    assert_eq!(body["error"]["type"], "permission_error", "{body}");
    assert_eq!(body["error"]["message"], NO_PRICE, "{body}");
    let (status, body) = chat(&gw, flex(), Some("lmgw-flexer")).await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("has no flex input and output token price on file (none is)"),
        "{body}"
    );
    assert!(
        m.received_requests().await.unwrap().is_empty(),
        "nothing sent"
    );

    // Priced: passes.
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    assert_eq!(chat(&gw, json!({}), Some("lmgw-flexer")).await.0, 200);
}

/// The global budget refuses an unpriced model too, a keyless request
/// included; without any budget it passes.
#[tokio::test]
async fn the_global_budget_refuses_an_unpriced_model_and_no_budget_passes() {
    let (_m, state, gw) =
        platform(ResponseTemplate::new(200).set_body_json(answer(Some("default")))).await;
    assert_eq!(chat(&gw, json!({}), None).await.0, 200, "no budget");
    let mut s = state.snapshot().settings.clone();
    s.global_budget_micro = 1_000_000;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let (status, body) = chat(&gw, json!({}), None).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "budget_unpriced", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("this gateway has a budget, and 'tgt-model' on upstream 'test-up'"),
        "{body}"
    );
}

/// The routes outside the chat path are checked on the alias's route:
/// embeddings and legacy completions on an unpriced cloud model.
#[tokio::test]
async fn embeddings_and_legacy_completions_on_an_unpriced_model_are_refused() {
    let (m, state, gw) =
        platform(ResponseTemplate::new(200).set_body_json(answer(Some("default")))).await;
    budgeted_key(&state, "flexer", 1_000_000).await;
    for (route, body) in [
        ("/v1/embeddings", json!({"model": ALIAS, "input": "hi"})),
        ("/v1/completions", json!({"model": ALIAS, "prompt": "hi"})),
    ] {
        let resp = gw
            .anon()
            .post(format!("{gw}{route}"))
            .bearer_auth("lmgw-flexer")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "{route}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "budget_unpriced", "{route}: {body}");
    }
    assert!(
        m.received_requests().await.unwrap().is_empty(),
        "nothing sent"
    );
}

/// A local model is free: a budgeted key is never refused on it.
#[tokio::test]
async fn a_local_model_is_never_refused_for_a_price() {
    let gpu = Gpu::new(8 * GIB, 2, 5).await;
    gpu.model("chat-model", GIB).await;
    budgeted_key(&gpu.state, "flexer", 1_000_000).await;
    let gw = crate::common::serve(gpu.state.clone()).await;
    let resp = gw
        .anon()
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("lmgw-flexer")
        .json(&json!({"model": "chat-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or_default();
    assert_ne!(body["error"]["code"], "budget_unpriced", "{status}: {body}");
}

/// The route the hold swapped to is the one checked: a held local model
/// whose fallback is a platform alias with no flex price refuses the flex
/// request on the fallback's scope, under the fallback's headers.
#[tokio::test]
async fn the_route_a_fallback_answers_on_is_the_one_checked() {
    let gpu = Gpu::new(8 * GIB, 2, 5).await;
    gpu.model("chat-model", GIB).await;
    let m = MockServer::start().await;
    let up = store::insert_upstream(
        &gpu.state.db,
        &NewUpstream {
            name: "test-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: m.uri(),
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
    .unwrap();
    store::insert_alias(
        &gpu.state.db,
        &NewAlias {
            alias: ALIAS.into(),
            upstream_id: up,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    price(&gpu.state, PriceTier::Standard, 2.0, 8.0).await;
    crate::support::fallbacks::every_chat_model_falls_back_to(&gpu.state, Some(ALIAS)).await;
    budgeted_key(&gpu.state, "flexer", 1_000_000).await;
    lmgw_core::ops::hold_set(&gpu.state, true).await.unwrap();
    let gw = crate::common::serve(gpu.state.clone()).await;

    let resp = gw
        .anon()
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("lmgw-flexer")
        .json(&json!({"model": "chat-model", "service_tier": "flex",
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some(ALIAS)
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["message"], MESSAGE, "{body}");
    assert!(m.received_requests().await.unwrap().is_empty());
    assert!(gpu.runs().is_empty(), "the held model never started");
}
