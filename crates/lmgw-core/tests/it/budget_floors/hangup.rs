//! A client that hangs up before the platform answered (openai egress
//! design §13.4): a whole answer is still read, by a
//! task of its own, and its row is the `499` with the answer's usage and
//! cost; a stream whose 2xx never reached it counts its prompt locally, the
//! budgets' lower bound.

use std::time::Duration;

use super::*;

/// How long the upstream takes to answer, and how long the client waits.
const ANSWER: Duration = Duration::from_millis(500);
const PATIENCE: Duration = Duration::from_millis(100);

/// `POST /v1/chat/completions` with `body`, given up on after [`PATIENCE`].
async fn hang_up(gw: &Gw, body: Value) {
    let sent = reqwest::Client::builder()
        .timeout(PATIENCE)
        .build()
        .unwrap()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&body)
        .send()
        .await;
    assert!(sent.is_err(), "the client gave up first: {sent:?}");
}

/// The row once there is one, after the upstream answered.
async fn row_later(state: &SharedState) -> lmgw_core::store::RequestLogRow {
    tokio::time::sleep(ANSWER).await;
    row_after(state, 0).await
}

#[tokio::test]
async fn a_whole_platform_answer_its_client_left_keeps_its_usage() {
    let (m, state, gw) = platform(
        ResponseTemplate::new(200)
            .set_body_json(answer(Some("default")))
            .set_delay(ANSWER),
    )
    .await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    hang_up(
        &gw,
        json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    let row = row_later(&state).await;
    assert_eq!(row.status, 499, "{row:?}");
    assert_eq!(row.error_kind.as_deref(), Some("client_disconnected"));
    assert_eq!(
        row.error_msg.as_deref(),
        Some("client disconnected before the answer was sent")
    );
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens, row.cost_micro),
        (Some(1000), Some(100), Some(STANDARD_MICRO)),
        "the answer's usage, priced: {row:?}"
    );
    assert_eq!(spend(&state).await, STANDARD_MICRO);
    assert_eq!(m.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_platform_stream_whose_2xx_never_came_counts_its_prompt() {
    let (_m, state, gw) = platform(
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string("data: [DONE]\n\n")
            .set_delay(ANSWER),
    )
    .await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    hang_up(
        &gw,
        json!({"model": ALIAS, "stream": true,
               "messages": [{"role": "user", "content": "Say hello."}]}),
    )
    .await;
    let row = row_after(&state, 0).await;
    assert_eq!(row.status, 499, "{row:?}");
    assert_eq!(row.error_kind.as_deref(), Some("client_disconnected"));
    assert_eq!((row.prompt_tokens, row.cost_micro), (None, None));
    let p = tiktoken_rs::bpe_for_model("tgt-model")
        .or_else(|_| tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase))
        .unwrap()
        .encode_ordinary("Say hello.")
        .len() as i64;
    let micro = p * 2;
    assert_eq!(
        row.price_note,
        Some(format!(
            "the client left before the upstream answered; the upstream reported no usage; \
             budgets count a local count of {p} prompt and 0 output tokens at the standard \
             price, {:.6} USD, a lower bound",
            micro as f64 / 1_000_000.0
        ))
    );
    assert_eq!(floor_of(&state, row.id).await, Some(micro));
}

/// A failed whole Response its client left (`record_gone`): the row keeps
/// the error and the usage the Response reported (decision 22).
#[tokio::test]
async fn a_failed_whole_response_its_client_left_keeps_its_usage() {
    let mut failed = answer(Some("default"));
    failed["status"] = json!("failed");
    failed["error"] = json!({"code": "server_error", "message": "The model broke."});
    let (_m, state, gw) = platform(
        ResponseTemplate::new(200)
            .set_body_json(failed)
            .set_delay(ANSWER),
    )
    .await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    hang_up(
        &gw,
        json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    let row = row_later(&state).await;
    assert_eq!(row.status, 502, "{row:?}");
    assert_eq!(row.error_kind.as_deref(), Some("upstream"));
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens, row.cost_micro),
        (Some(1000), Some(100), Some(STANDARD_MICRO)),
        "the failed Response's usage, priced: {row:?}"
    );
}
