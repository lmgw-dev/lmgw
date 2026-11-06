//! A native answer's row carries what its Response object reported: the
//! token totals with their cache and reasoning details, priced on the
//! alias's sheet, whole and streamed, on both protocols that forward.

use super::*;

#[tokio::test]
async fn a_whole_native_answer_is_recorded_with_its_tokens_and_cost() {
    for protocol in NATIVE {
        let (base, up) = upstream(Says::Answers).await;
        let (state, gw) = gateway(protocol, &base).await;
        let (status, body) = post(&gw, json!({"model": ALIAS, "input": "hi"})).await;
        assert_eq!(status, 200, "{protocol:?}: {body}");
        assert_eq!(body["id"], "resp_upstream", "relayed as sent: {body}");
        assert_eq!(up.responses().len(), 1, "{protocol:?}");
        assert_eq!(up.chats(), 0, "{protocol:?}: not synthesized");

        let row = row(&state, 1).await;
        assert_metered(&row);
        assert!(!row.streamed);
        assert_eq!(row.error_kind, None, "{row:?}");
        assert_eq!(
            row.egress_proto.as_deref(),
            Some(protocol.as_str()),
            "{row:?}"
        );
    }
}

#[tokio::test]
async fn a_streamed_native_answer_is_recorded_from_its_completed_event() {
    for protocol in NATIVE {
        let (base, _up) = upstream(Says::Answers).await;
        let (state, gw) = gateway(protocol, &base).await;
        let resp = gw
            .client()
            .post(format!("{gw}/v1/responses"))
            .json(&json!({"model": ALIAS, "input": "hi", "stream": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.unwrap();
        // The stream reaches the client untouched, every event in order.
        let created = text.find("event: response.created").expect("created");
        let delta = text
            .find("event: response.output_text.delta")
            .expect("delta");
        let done = text.find("event: response.completed").expect("completed");
        assert!(created < delta && delta < done, "{text}");

        let row = row(&state, 1).await;
        assert_metered(&row);
        assert!(row.streamed);
        assert_eq!(row.error_kind, None, "{row:?}");
    }
}

/// A budget sees native spend as it sees the loop's: the second request
/// meets a gateway budget the first one spent.
#[tokio::test]
async fn native_spend_counts_toward_the_budget() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let mut settings = state.snapshot().settings.clone();
    settings.global_budget_micro = COST_MICRO - 1;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let (status, body) = post(&gw, json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200, "{body}");
    row(&state, 1).await;
    let (status, body) = post(&gw, json!({"model": ALIAS, "input": "again"})).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "key_budget", "{body}");
    assert_eq!(up.responses().len(), 1, "the refused request never left");
}

/// A stream the upstream ended before `response.completed` reported no
/// usage: the row has unknown tokens and cost, never zero, and says so.
#[tokio::test]
async fn a_stream_cut_before_its_completed_event_leaves_the_usage_unknown() {
    let (base, _up) = upstream(Says::Cuts).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": ALIAS, "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(!text.contains("response.completed"), "{text}");

    let row = row(&state, 1).await;
    assert!(row.streamed);
    assert_eq!(
        (row.error_kind.as_deref(), row.error_msg.as_deref()),
        (
            Some("incomplete"),
            Some("stream ended before the upstream reported usage")
        ),
        "{row:?}"
    );
    assert_eq!(row.status, 200, "the client got all there was: {row:?}");
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens, row.cost_micro),
        (None, None, None),
        "{row:?}"
    );
}

/// A whole answer with no `usage` to read is marked like a cut stream.
#[tokio::test]
async fn a_whole_answer_without_usage_is_marked_incomplete() {
    let (base, _up) = upstream(Says::NoUsage).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let (status, body) = post(&gw, json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200, "{body}");
    let row = row(&state, 1).await;
    assert_eq!(
        (row.error_kind.as_deref(), row.error_msg.as_deref()),
        (
            Some("incomplete"),
            Some("the upstream's answer reported no usage")
        ),
        "{row:?}"
    );
    assert_eq!((row.prompt_tokens, row.cost_micro), (None, None), "{row:?}");
}

/// A body that breaks after `response.completed` leaves the whole answer's
/// row as it was: the usage arrived, and nothing after it undoes that.
#[tokio::test]
async fn a_transport_error_after_the_completed_event_keeps_the_whole_row() {
    let (base, _up) = upstream(Says::CompletesThenBreaks).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": ALIAS, "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await;
    let row = row(&state, 1).await;
    assert_eq!(row.error_kind, None, "{row:?}");
    assert_metered(&row);
}
