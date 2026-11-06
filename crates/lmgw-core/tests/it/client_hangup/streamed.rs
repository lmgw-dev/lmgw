//! A streamed answer whose client goes: the model's connection closes at
//! once, the model's claim goes with it, and the row says `canceled` —
//! whether or not the stream had written the client anything since.

use futures::StreamExt;

use super::*;

/// Open a stream on `path` with `body`, read its first bytes, and go.
async fn open_and_leave(gw: &Gw, path: &str, body: Value) -> Instant {
    let resp = gw
        .client()
        .post(format!("{gw}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let mut stream = resp.bytes_stream();
    let first = stream.next().await.expect("the stream opens").unwrap();
    assert!(!first.is_empty());
    drop(stream);
    Instant::now()
}

/// What follows a client's going on every stream here.
async fn ends(g: &Gpu, model: &Model, gone: Instant) {
    model_stops(model, gone).await;
    claim_released(g).await;
    let row = newest_row(&g.state).await;
    assert_eq!(row.error_kind.as_deref(), Some("canceled"), "{row:?}");
    assert!(row.streamed);
}

#[tokio::test]
async fn a_thinking_model_whose_reasoning_the_client_reads_stops_when_it_goes() {
    let (g, gw, model) = local(Says::Reasoning).await;
    let gone = open_and_leave(&gw, "/v1/chat/completions", chat_body(true, json!({}))).await;
    ends(&g, &model, gone).await;
}

/// The incident: a structured answer whose model streams frames that carry
/// nothing for the client, so lmgw writes it nothing — the client's going
/// is heard all the same.
#[tokio::test]
async fn a_stream_that_writes_the_client_nothing_stops_when_it_goes() {
    let (g, gw, model) = local(Says::Nothing).await;
    let gone = open_and_leave(&gw, "/v1/chat/completions", chat_body(true, json_schema())).await;
    ends(&g, &model, gone).await;
}

#[tokio::test]
async fn a_model_still_prefilling_stops_when_its_client_goes() {
    let (g, gw, model) = local(Says::Silence).await;
    let gone = open_and_leave(&gw, "/v1/chat/completions", chat_body(true, json!({}))).await;
    ends(&g, &model, gone).await;
}

#[tokio::test]
async fn a_messages_stream_that_writes_the_client_nothing_stops_when_it_goes() {
    let (g, gw, model) = local(Says::Nothing).await;
    let body = json!({
        "model": "m",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let gone = open_and_leave(&gw, "/v1/messages", body).await;
    ends(&g, &model, gone).await;
}

/// The legacy relay pipes the upstream's bytes as they come: with none
/// coming, it writes nothing either.
#[tokio::test]
async fn a_legacy_completion_still_prefilling_stops_when_its_client_goes() {
    let (g, gw, model) = local(Says::Silence).await;
    let resp = gw
        .client()
        .post(format!("{gw}/v1/completions"))
        .json(&json!({"model": "m", "stream": true, "prompt": "Once upon a time"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    // Nothing comes before the first token: the head is all there is.
    drop(resp);
    ends(&g, &model, Instant::now()).await;
}
