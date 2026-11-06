//! A client that goes once it read the answer's last frame, while the
//! upstream still has its usage to send (OpenAI sends it a chunk of its
//! own): the model is done, so the client's going is not a `canceled`, and
//! the row keeps what the answer cost.

use futures::StreamExt;

use super::*;

/// Open a stream on `path` with `body`, read it up to the answer's last
/// frame, and go.
async fn read_to_the_finish(gw: &Gw, path: &str, body: Value) {
    let resp = gw
        .client()
        .post(format!("{gw}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let mut stream = resp.bytes_stream();
    let mut read = Vec::new();
    while !String::from_utf8_lossy(&read).contains(r#""finish_reason":"stop""#) {
        let chunk = stream.next().await.expect("the stream reaches its finish");
        read.extend_from_slice(&chunk.unwrap());
    }
}

#[tokio::test]
async fn a_chat_stream_whose_client_goes_after_the_finish_keeps_its_usage() {
    let (g, gw, _model) = local(Says::Finishes).await;
    read_to_the_finish(&gw, "/v1/chat/completions", chat_body(true, json!({}))).await;
    let row = newest_row(&g.state).await;
    assert_eq!(row.error_kind, None, "{row:?}");
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens),
        (Some(11), Some(2)),
        "{row:?}"
    );
    claim_released(&g).await;
}

/// The legacy relay records no usage; its row is whole all the same.
#[tokio::test]
async fn a_legacy_completion_whose_client_goes_after_the_finish_is_not_canceled() {
    let (g, gw, _model) = local(Says::Finishes).await;
    let body = json!({"model": "m", "stream": true, "prompt": "Once upon a time"});
    read_to_the_finish(&gw, "/v1/completions", body).await;
    let row = newest_row(&g.state).await;
    assert_eq!(row.error_kind, None, "{row:?}");
    claim_released(&g).await;
}
