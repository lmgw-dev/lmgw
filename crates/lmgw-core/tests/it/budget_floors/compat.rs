//! A cloud row that is not `openai` (an `openai_compatible` aggregator,
//! Gemini, Anthropic) leaks nothing to a hang-up either (openai egress
//! design §13.4): a whole answer is still read by a task of
//! its own, and where no usage ever comes, the budgets count an estimate at
//! ~4 characters a token as a floor, the row's cost staying unknown.

use std::time::Duration;

use super::*;

const ANSWER: Duration = Duration::from_millis(500);
const PATIENCE: Duration = Duration::from_millis(100);

/// A compatible upstream answering every chat with `template`.
async fn compat(template: ResponseTemplate) -> (MockServer, SharedState, Gw) {
    let m = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(template)
        .mount(&m)
        .await;
    let (state, gw) = setup_kind(&m.uri(), Protocol::OpenaiCompatible, UpstreamKind::Generic).await;
    (m, state, gw)
}

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

fn no_usage() -> Value {
    json!({
        "id": "chatcmpl-up", "object": "chat.completion", "model": "tgt-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong"},
                     "finish_reason": "stop"}],
    })
}

fn clause(prompt: i64, output: i64) -> (String, i64) {
    let micro = prompt * 2 + output * 8;
    (
        format!(
            "budgets count an estimate of {prompt} prompt and {output} output tokens \
             (~4 characters a token) at the standard price, {:.6} USD, a lower bound",
            micro as f64 / 1_000_000.0
        ),
        micro,
    )
}

/// A whole answer with usage is read after the hang-up and kept.
#[tokio::test]
async fn a_compatible_row_s_whole_answer_its_client_left_keeps_its_usage() {
    let (m, state, gw) = compat(
        ResponseTemplate::new(200)
            .set_body_json(chat_answer())
            .set_delay(ANSWER),
    )
    .await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    hang_up(
        &gw,
        json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    tokio::time::sleep(ANSWER).await;
    let row = row_after(&state, 0).await;
    assert_eq!(row.status, 499, "{row:?}");
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens, row.cost_micro),
        (Some(1000), Some(100), Some(STANDARD_MICRO)),
        "{row:?}"
    );
    assert_eq!(spend(&state).await, STANDARD_MICRO);
    assert_eq!(m.received_requests().await.unwrap().len(), 1);
}

/// A whole answer without usage: the estimate is the budgets' floor.
#[tokio::test]
async fn a_compatible_row_s_answer_without_usage_its_client_left_floors_on_an_estimate() {
    let (_m, state, gw) = compat(
        ResponseTemplate::new(200)
            .set_body_json(no_usage())
            .set_delay(ANSWER),
    )
    .await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    hang_up(
        &gw,
        json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    tokio::time::sleep(ANSWER).await;
    let row = row_after(&state, 0).await;
    assert_eq!(row.status, 499, "{row:?}");
    assert_eq!((row.prompt_tokens, row.cost_micro), (None, None), "{row:?}");
    // "user: hi" is 8 characters, "pong" is 4.
    let (clause, micro) = clause(2, 1);
    // The upstream did answer; only its usage is missing.
    assert_eq!(
        row.price_note,
        Some(format!(
            "the client left; the answer reported no usage; {clause}"
        ))
    );
    assert_eq!(floor_of(&state, row.id).await, Some(micro));
    assert_eq!(spend(&state).await, micro);
}

/// A stream cut before any usage: the relayed text is counted.
#[tokio::test]
async fn a_compatible_row_s_stream_cut_before_its_usage_floors_on_an_estimate() {
    let chunks: String = [
        json!({"object": "chat.completion.chunk",
               "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hel"}}]}),
        json!({"object": "chat.completion.chunk",
               "choices": [{"index": 0, "delta": {"content": "lo there"},
                            "finish_reason": "stop"}]}),
    ]
    .iter()
    .map(|c| format!("data: {c}\n\n"))
    .collect();
    let (_m, state, gw) = compat(
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(chunks),
    )
    .await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    let resp = gw
        .anon()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": ALIAS, "stream": true,
                      "messages": [{"role": "user", "content": "Say hello."}]}))
        .send()
        .await
        .unwrap();
    resp.text().await.unwrap();
    let row = row_after(&state, 0).await;
    assert_eq!((row.prompt_tokens, row.cost_micro), (None, None), "{row:?}");
    // "user: Say hello." is 16 characters, "Hello there" is 11.
    let (clause, micro) = clause(4, 3);
    assert_eq!(
        row.price_note,
        Some(format!(
            "stream ended before the upstream reported usage; {clause}"
        ))
    );
    assert_eq!(floor_of(&state, row.id).await, Some(micro));
}

/// A stream whose 2xx never came: the prompt, estimated.
#[tokio::test]
async fn a_compatible_row_s_stream_whose_2xx_never_came_estimates_its_prompt() {
    let (_m, state, gw) = compat(
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
    assert_eq!((row.prompt_tokens, row.cost_micro), (None, None));
    let (clause, micro) = clause(4, 0);
    assert!(
        row.price_note
            .as_deref()
            .unwrap_or_default()
            .ends_with(&clause),
        "{row:?}"
    );
    assert_eq!(floor_of(&state, row.id).await, Some(micro));
}
