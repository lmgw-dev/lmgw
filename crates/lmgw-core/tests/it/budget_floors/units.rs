//! What counts as priced for a budget (openai egress design §13.3, decision
//! 24, the owner's ruling of 2026-10-24: a budget is enforced only where
//! the model's pricing is known): a model turn needs an input and an output token
//! price, an encoder call an input price, and a per-request fee alone is
//! enough only for an image or audio call. A manual 0 is a price.

use super::*;

/// A token row on [`ALIAS`] with `price_in` and `price_out` as given.
async fn token_row(state: &SharedState, price_in: Option<f64>, price_out: Option<f64>) {
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        ALIAS,
        PriceUnit::PerMtok,
        PriceTier::Standard,
        &Prices {
            price_in,
            price_out,
            source: PriceSource::Manual,
            ..Default::default()
        },
        None,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// A fee of 0.01 a request on [`ALIAS`], and nothing else.
async fn request_row(state: &SharedState) {
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        ALIAS,
        PriceUnit::PerRequest,
        PriceTier::Standard,
        &Prices {
            source: PriceSource::Manual,
            ..Default::default()
        },
        Some(0.01),
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// `POST route` with `body` as the budgeted key.
async fn post(gw: &Gw, route: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .anon()
        .post(format!("{gw}{route}"))
        .bearer_auth("lmgw-flexer")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

fn embed() -> Value {
    json!({"model": ALIAS, "input": "hi"})
}

fn image() -> Value {
    json!({"model": ALIAS, "prompt": "a cat"})
}

fn refused(status: u16, body: &Value) -> &str {
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "budget_unpriced", "{body}");
    body["error"]["message"].as_str().unwrap_or_default()
}

fn passed(status: u16, body: &Value) {
    assert_ne!(body["error"]["code"], "budget_unpriced", "{status}: {body}");
}

async fn world() -> (MockServer, SharedState, Gw) {
    let (m, state, gw) =
        platform(ResponseTemplate::new(200).set_body_json(answer(Some("default")))).await;
    for p in ["/embeddings", "/images/generations"] {
        Mock::given(method("POST"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "model": "tgt-model", "created": 1,
                "data": [{"object": "embedding", "index": 0, "embedding": [0.1],
                          "b64_json": "AAAA"}],
                "usage": {"prompt_tokens": 1, "total_tokens": 1},
            })))
            .mount(&m)
            .await;
    }
    budgeted_key(&state, "flexer", 1_000_000).await;
    (m, state, gw)
}

#[tokio::test]
async fn an_input_price_alone_prices_an_embedding_and_not_a_chat_turn() {
    let (_m, state, gw) = world().await;
    token_row(&state, Some(1.0), None).await;
    let (status, body) = chat(&gw, json!({}), Some("lmgw-flexer")).await;
    let msg = refused(status, &body);
    assert!(
        msg.contains("has no standard output token price on file (none is)"),
        "{msg}"
    );
    let (status, body) = post(&gw, "/v1/embeddings", embed()).await;
    passed(status, &body);
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn an_output_price_alone_does_not_price_a_chat_turn_or_an_embedding() {
    let (_m, state, gw) = world().await;
    token_row(&state, None, Some(8.0)).await;
    let (status, body) = chat(&gw, json!({}), Some("lmgw-flexer")).await;
    let msg = refused(status, &body);
    assert!(
        msg.contains("no standard input token price on file"),
        "{msg}"
    );
    let (status, body) = post(&gw, "/v1/embeddings", embed()).await;
    let msg = refused(status, &body);
    assert!(
        msg.contains("no standard input token price on file"),
        "{msg}"
    );
}

#[tokio::test]
async fn a_per_request_fee_alone_prices_an_image_call_and_not_a_chat_turn() {
    let (_m, state, gw) = world().await;
    request_row(&state).await;
    let (status, body) = chat(&gw, json!({}), Some("lmgw-flexer")).await;
    let msg = refused(status, &body);
    assert!(
        msg.contains("has no standard input and output token price on file (none is)"),
        "{msg}"
    );
    let (status, body) = post(&gw, "/v1/embeddings", embed()).await;
    refused(status, &body);
    let (status, body) = post(&gw, "/v1/images/generations", image()).await;
    passed(status, &body);
}

#[tokio::test]
async fn token_prices_alone_do_not_price_an_image_call() {
    let (_m, state, gw) = world().await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let (status, body) = post(&gw, "/v1/images/generations", image()).await;
    let msg = refused(status, &body);
    assert!(
        msg.contains("has no standard price per image or per request on file (token prices are not read for image calls)"),
        "{msg}"
    );
}

#[tokio::test]
async fn a_manual_zero_is_a_price() {
    let (_m, state, gw) = world().await;
    token_row(&state, Some(0.0), Some(0.0)).await;
    let (status, body) = chat(&gw, json!({}), Some("lmgw-flexer")).await;
    assert_eq!(status, 200, "{body}");
}

/// The refusal names the tier it needs and the one that is on file.
#[tokio::test]
async fn the_refusal_names_the_tier_needed_and_the_one_on_file() {
    let (_m, state, gw) = world().await;
    price(&state, PriceTier::Fast, 4.0, 16.0).await;
    let (status, body) = chat(&gw, json!({}), Some("lmgw-flexer")).await;
    let msg = refused(status, &body);
    assert!(
        msg.contains(
            "has no standard input and output token price on file (a fast one is), so its \
             spend could not be counted"
        ),
        "{msg}"
    );
}
