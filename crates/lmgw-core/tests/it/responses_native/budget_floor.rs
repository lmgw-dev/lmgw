//! Budget lower bounds on `/v1/responses` (openai egress design §13):
//! flex without a flex price is refused to a budgeted caller on the native
//! path and in the loop (§13.3, decision 24), and a native stream cut before its
//! terminal event floors from lmgw's own count of it (§13.4).

use futures::StreamExt;

use super::*;

/// A key `flexer` with a budget, its bearer `lmgw-flexer`.
async fn budgeted(state: &SharedState) {
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, budget_micro, budget_period)
         VALUES ('flexer', ?1, 1, 1000000, 'month')",
    )
    .bind(lmgw_core::config::hash_api_key("lmgw-flexer"))
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// `POST /v1/responses` as `flexer`.
async fn post_as_flexer(gw: &Gw, body: Value) -> (u16, Value) {
    let resp = gw
        .anon()
        .post(format!("{gw}/v1/responses"))
        .bearer_auth("lmgw-flexer")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or_default())
}

/// [`price`] is a standard row only: flex has no price on the alias's
/// scope, so the platform refuses a budgeted flex request on both paths
/// before anything leaves; the row names the route.
#[tokio::test]
async fn flex_without_a_flex_price_is_refused_natively_and_in_the_loop() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    budgeted(&state).await;
    for (n, body) in [
        json!({"model": ALIAS, "input": "hi", "service_tier": "flex"}),
        // A built-in label keeps it in the loop (`routing`).
        json!({"model": ALIAS, "input": "hi", "service_tier": "flex",
               "tools": [{"type": "mcp", "server_label": "docs"}]}),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, body) = post_as_flexer(&gw, body).await;
        assert_eq!(status, 403, "{n}: {body}");
        assert_eq!(body["error"]["code"], "budget_unpriced", "{n}: {body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .starts_with(
                    "key 'flexer' has a budget, and 'gpt-test' on upstream 'native-up' has no \
                     flex input and output token price on file (a standard one is)"
                ),
            "{n}: {body}"
        );
        let r = row(&state, n + 1).await;
        assert_eq!(r.error_kind.as_deref(), Some("budget_unpriced"), "{r:?}");
        assert_eq!(r.upstream_name.as_deref(), Some("native-up"), "{r:?}");
    }
    assert!(up.responses().is_empty(), "nothing native");
    assert_eq!(up.chats(), 0, "no loop turn");

    // Without flex: answered.
    let (status, body) = post_as_flexer(&gw, json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200, "{body}");
}

/// A flagged compatible row has no tier dimension: nothing to refuse.
#[tokio::test]
async fn a_compatible_row_is_not_refused() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::OpenaiCompatible, &base).await;
    budgeted(&state).await;
    let (status, body) = post_as_flexer(
        &gw,
        json!({"model": ALIAS, "input": "hi", "service_tier": "flex"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(up.responses().len(), 1);
}

/// The row's `budget_floor_micro`, which no row view reads.
async fn floor_of(state: &SharedState, id: i64) -> Option<i64> {
    sqlx::query_scalar("SELECT budget_floor_micro FROM request_logs WHERE id = ?1")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// The upstream ended the stream before `response.completed`: the token
/// columns stay NULL, the row unpriced, and the budgets count a local count
/// of the prompt sent and the delta relayed at the standard sheet, the
/// prompt at the cache-read rate. The key's spend moves by it, and the
/// reseed reads the same.
#[tokio::test]
async fn a_stream_the_upstream_cut_floors_from_a_local_count() {
    let (base, _up) = upstream(Says::Cuts).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    budgeted(&state).await;
    let prompt = "Count these words, please.";
    let resp = gw
        .anon()
        .post(format!("{gw}/v1/responses"))
        .bearer_auth("lmgw-flexer")
        .json(&json!({"model": ALIAS, "input": prompt, "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.text().await.unwrap();
    let r = row(&state, 1).await;
    assert_eq!(r.error_kind.as_deref(), Some("incomplete"), "{r:?}");
    assert_eq!(
        (r.prompt_tokens, r.completion_tokens, r.cost_micro),
        (None, None, None),
        "a local count is never shown as usage: {r:?}"
    );
    let (clause, micro) = cut_floor(prompt, "Hel");
    assert!(micro > 0);
    assert_eq!(
        r.price_note,
        Some(format!(
            "stream ended before the upstream reported usage; {clause}"
        ))
    );
    assert_eq!(floor_of(&state, r.id).await, Some(micro));
    let key: i64 = sqlx::query_scalar("SELECT id FROM api_keys WHERE name = 'flexer'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(
        store::key_spend_micro(&state.db, key, "0000-01-01T00")
            .await
            .unwrap(),
        micro,
        "the reseed counts it"
    );
}

/// The client left mid-stream: the same floor, behind the note that says
/// the client left.
#[tokio::test]
async fn a_stream_its_client_left_floors_from_a_local_count() {
    let (base, up) = upstream(Says::Holds).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": ALIAS, "input": "hi", "stream": true,
                      "instructions": "Answer in one word."}))
        .send()
        .await
        .unwrap();
    let mut stream = resp.bytes_stream();
    let mut read = Vec::new();
    while !String::from_utf8_lossy(&read).contains("response.output_text.delta") {
        read.extend_from_slice(&stream.next().await.expect("the delta comes").unwrap());
    }
    drop(stream);
    patience::until("the upstream stream to close", || {
        up.closed.load(Ordering::SeqCst) == 1
    })
    .await;
    let r = row(&state, 1).await;
    assert_eq!(r.error_kind.as_deref(), Some("canceled"), "{r:?}");
    let (clause, micro) = cut_floor("Answer in one word.\nhi", "Hel");
    assert_eq!(
        r.price_note,
        Some(format!(
            "the client left before the upstream reported usage; {clause}"
        ))
    );
    assert_eq!(floor_of(&state, r.id).await, Some(micro));
    assert_eq!(r.cost_micro, None);
}

/// A terminal event whose `usage` is null (`response.failed`) reported no
/// usage either: the same local count, behind the note that says so.
#[tokio::test]
async fn a_terminal_event_without_usage_floors_from_a_local_count() {
    let (base, _up) = upstream(Says::FailsWithoutUsage).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": ALIAS, "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    resp.text().await.unwrap();
    let r = row(&state, 1).await;
    assert_eq!(r.error_kind.as_deref(), Some("upstream"), "{r:?}");
    assert_eq!((r.prompt_tokens, r.cost_micro), (None, None), "{r:?}");
    let (clause, micro) = cut_floor("hi", "Hel");
    assert_eq!(
        r.price_note,
        Some(format!(
            "the upstream's terminal event reported no usage; {clause}"
        ))
    );
    assert_eq!(floor_of(&state, r.id).await, Some(micro));
}

/// A flagged compatible row's cut stream has no floor yet: lmgw knows no
/// tokenizer for what an aggregator serves.
#[tokio::test]
async fn a_compatible_row_s_cut_stream_has_no_floor() {
    let (base, _up) = upstream(Says::Cuts).await;
    let (state, gw) = gateway(Protocol::OpenaiCompatible, &base).await;
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": ALIAS, "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    resp.text().await.unwrap();
    let r = row(&state, 1).await;
    assert_eq!(
        r.price_note.as_deref(),
        Some("stream ended before the upstream reported usage")
    );
    assert_eq!(floor_of(&state, r.id).await, None);
}
