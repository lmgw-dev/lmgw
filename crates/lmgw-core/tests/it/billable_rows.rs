//! Billable units through the gateway (billable-units design §3.2, §4.5): a
//! public request's row is priced in every unit its alias has a rate for —
//! tokens plus a per-request fee on one row — a request-only sheet prices a
//! row that carries no token count, and an upstream refusal pays no fee.

use lmgw_core::config::{PriceScope, PriceUnit, Protocol, UpstreamKind};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::e2e_proxy::setup_kind;
use crate::stream_usage::row_after;

fn chat_answer() -> Value {
    json!({
        "id": "chatcmpl-up", "object": "chat.completion", "model": "tgt-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1_000_000, "completion_tokens": 100_000}
    })
}

async fn price(state: &SharedState, unit: PriceUnit, p: Prices, price: Option<f64>) {
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        "my-model",
        unit,
        &p,
        price,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

async fn tokens_3_15(state: &SharedState) {
    price(
        state,
        PriceUnit::PerMtok,
        Prices {
            price_in: Some(3.0),
            price_out: Some(15.0),
            source: PriceSource::Catalog,
            ..Default::default()
        },
        None,
    )
    .await;
}

async fn fee(state: &SharedState, per_request: f64) {
    price(
        state,
        PriceUnit::PerRequest,
        Prices {
            source: PriceSource::Catalog,
            ..Default::default()
        },
        Some(per_request),
    )
    .await;
}

async fn post(base: &impl std::fmt::Display, route: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}{route}"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// §3.3's mixed golden end to end: 1M in and 100k out at 3/15 plus
/// 0.005 per request is 4 505 000 micro, each part on the row.
#[tokio::test]
async fn a_chat_row_pays_tokens_and_its_request_fee() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_answer()))
        .mount(&mock)
        .await;
    let (state, base) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    tokens_3_15(&state).await;
    fee(&state, 0.005).await;

    let resp = post(
        &base,
        "/v1/chat/completions",
        json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let row = row_after(&state, 0).await;
    assert_eq!(row.cost_in_micro, Some(3_000_000));
    assert_eq!(row.cost_out_micro, Some(1_500_000));
    assert_eq!(row.cost_units_micro, Some(5_000));
    assert_eq!(row.cost_micro, Some(4_505_000));
    assert_eq!(row.price_per_request, Some(0.005));
    assert_eq!(row.price_source.as_deref(), Some("catalog"));
}

/// The streamed legacy relay reads no usage (§4.8): on a token-priced
/// scope its row stays unpriced, beside a fee or not, and a request-only
/// sheet prices it at the fee.
#[tokio::test]
async fn a_row_without_tokens_is_priced_by_a_request_only_sheet() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"text\":\"pong\",\"index\":0}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&mock)
        .await;
    let (state, base) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    fee(&state, 0.005).await;
    let legacy = json!({"model": "my-model", "prompt": "hi", "stream": true});

    let resp = post(&base, "/v1/completions", legacy.clone()).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await;
    let row = row_after(&state, 0).await;
    assert_eq!(row.prompt_tokens, None, "{row:?}");
    assert_eq!(row.cost_micro, Some(5_000), "{row:?}");
    assert_eq!(row.cost_in_micro, None, "no token row, no token part");

    tokens_3_15(&state).await;
    let resp = post(&base, "/v1/completions", legacy).await;
    let _ = resp.bytes().await;
    let row = row_after(&state, 1).await;
    assert_eq!(
        row.cost_micro, None,
        "a token row with no token count is unknown, not the fee alone"
    );
    assert_eq!(row.cost_units_micro, None);
    assert_eq!(
        row.price_per_request,
        Some(0.005),
        "the rate is still recorded"
    );
    assert_eq!(row.price_in, Some(3.0));
}

/// An upstream that refuses was not answered: no fee, and no false zero.
#[tokio::test]
async fn an_upstream_refusal_pays_no_fee() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "bad request", "type": "invalid_request_error"}
        })))
        .mount(&mock)
        .await;
    let (state, base) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    fee(&state, 0.005).await;

    let resp = post(
        &base,
        "/v1/chat/completions",
        json!({"model": "my-model", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let row = row_after(&state, 0).await;
    assert_eq!(row.cost_micro, None, "{row:?}");
    assert_eq!(row.price_per_request, Some(0.005));
}

/// The dashboard Chat's stream that began and then failed was answered: its
/// 502 row pays its fee rather than reading unknown (§4.5).
#[tokio::test]
async fn a_chat_tab_stream_that_failed_mid_way_pays_its_fee() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
                "data: {\"error\":{\"message\":\"overloaded\"}}\n\n",
            ),
            "text/event-stream",
        ))
        .mount(&mock)
        .await;
    let (state, base) = setup_kind(&mock.uri(), Protocol::Openai, UpstreamKind::Generic).await;
    fee(&state, 0.005).await;
    let client = base.client();
    let thread: Value = client
        .post(format!("{base}/chat/api/threads"))
        .json(&json!({ "model_alias": "my-model" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = thread["id"].as_i64().unwrap();
    let resp = client
        .post(format!("{base}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "hi there" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await;

    let row = row_after(&state, 0).await;
    assert_eq!(row.ingress_proto, "chat", "{row:?}");
    assert_eq!(row.status, 502, "{row:?}");
    assert_eq!(row.cost_micro, Some(5_000), "{row:?}");
}
