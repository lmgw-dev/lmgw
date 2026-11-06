//! A client that goes away from a native answer still leaves its row,
//! written by a task of its own (openai egress design §5.2).

use futures::StreamExt;

use super::*;

/// Open a stream and read until `until` has passed, then go.
async fn read_then_leave(gw: &Gw, until: &str) {
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": ALIAS, "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut stream = resp.bytes_stream();
    let mut read = Vec::new();
    while !String::from_utf8_lossy(&read).contains(until) {
        let chunk = stream.next().await.expect("the stream gets that far");
        read.extend_from_slice(&chunk.unwrap());
    }
}

/// Gone mid-stream: the upstream's connection closes, which ends the
/// generation, and the row says `canceled` with no usage, since none had
/// arrived.
#[tokio::test]
async fn a_client_gone_mid_stream_leaves_a_canceled_row() {
    for protocol in NATIVE {
        let (base, up) = upstream(Says::Holds).await;
        let (state, gw) = gateway(protocol, &base).await;
        read_then_leave(&gw, "response.output_text.delta").await;

        patience::until("the upstream stream to close", || {
            up.closed.load(Ordering::SeqCst) == 1
        })
        .await;
        let row = row(&state, 1).await;
        assert_eq!(row.error_kind.as_deref(), Some("canceled"), "{row:?}");
        assert_eq!(row.status, 200, "{row:?}");
        assert!(row.streamed);
        assert_eq!(
            (row.prompt_tokens, row.completion_tokens),
            (None, None),
            "{row:?}"
        );
    }
}

/// Gone after `response.completed`: the answer was whole and its usage
/// known, so the row is whole too.
#[tokio::test]
async fn a_client_gone_after_the_completed_event_keeps_the_usage() {
    let (base, _up) = upstream(Says::CompletesThenHolds).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    read_then_leave(&gw, "response.completed").await;
    let row = row(&state, 1).await;
    assert_eq!(row.error_kind, None, "{row:?}");
    assert_metered(&row);
}

/// A whole answer the client stopped waiting for is read on all the same
/// (*changed in WP10*, openai egress design §14.9): the upstream works on it
/// either way, so the row is the `499` with the answer's usage, counted as
/// answered, and the upstream connection is not cut.
#[tokio::test]
async fn a_client_gone_before_a_whole_answer_still_gets_its_usage() {
    let (base, up) = upstream(Says::Slowly).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let gone = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .timeout(SLOW_ANSWER / 4)
        .json(&json!({"model": ALIAS, "input": "hi"}))
        .send()
        .await;
    assert!(gone.is_err(), "the client gave up first");
    let row = row(&state, 1).await;
    assert_eq!(up.responses().len(), 1);
    assert_eq!(up.closed.load(Ordering::SeqCst), 0, "read to its end");
    assert_eq!(row.status, 499, "{row:?}");
    assert_eq!(
        row.error_kind.as_deref(),
        Some("client_disconnected"),
        "{row:?}"
    );
    assert_metered(&row);
}

/// A whole answer slower than the upstream's timeout is a `timeout` row.
#[tokio::test]
async fn a_whole_answer_is_bounded_by_the_upstream_timeout() {
    let (base, _up) = upstream(Says::Slowly).await;
    let (state, gw) = gateway_timing_out(Protocol::Openai, &base, 50).await;
    let (status, body) = post(&gw, json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 504, "{body}");
    let row = row(&state, 1).await;
    assert_eq!(row.error_kind.as_deref(), Some("timeout"), "{row:?}");
}

/// The records of native responses lmgw holds.
async fn records(state: &SharedState) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM native_responses")
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// OpenAI does not store a response cut before its terminal event, so lmgw
/// keeps no record of it either: a later fetch is lmgw's own not found, not
/// the upstream's relayed 404.
#[tokio::test]
async fn a_cut_stream_leaves_no_record_to_fetch() {
    for says in [Says::Holds, Says::Cuts] {
        let (base, up) = upstream(says).await;
        let (state, gw) = gateway(Protocol::Openai, &base).await;
        if says == Says::Holds {
            read_then_leave(&gw, "response.output_text.delta").await;
        } else {
            let text = gw
                .client()
                .post(format!("{gw}/v1/responses"))
                .json(&json!({"model": ALIAS, "input": "hi", "stream": true}))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap_or_default();
            assert!(text.contains("response.output_text.delta"), "{text}");
        }
        row(&state, 1).await;
        patience::until_async("the record to go", || async { records(&state).await == 0 }).await;
        let resp = gw
            .client()
            .get(format!("{gw}/v1/responses/resp_upstream"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let body: Value = resp.json().await.unwrap();
        assert_ne!(body["error"]["message"], Value::Null, "{body}");
        assert_eq!(
            up.responses().len(),
            1,
            "the fetch never reached the upstream"
        );
    }
}
