//! Hosted-tool fees on the native path (openai egress design §6.1): a
//! Response whose `output` holds a billable hosted call leaves its row
//! unpriced (never the token part alone) with a note naming the calls and
//! what the tokens would cost, and the Prices page lists the model.

use super::*;

/// Two web searches and a code-interpreter session, beside an MCP call
/// (no per-call fee) and a client-run computer call.
const CALLS: &[&str] = &[
    "web_search_call",
    "mcp_call",
    "web_search_call",
    "code_interpreter_call",
    "computer_call",
];

const NOTE: &str = "hosted tool fees not priced yet: 2 web_search_call, 1 code_interpreter_call; \
                    tokens alone would be 0.002750 USD, which budgets count";

fn assert_unpriced_for_its_calls(r: &store::RequestLogRow) {
    assert_eq!(r.cost_micro, None, "never the token part alone: {r:?}");
    assert_eq!(r.price_source.as_deref(), Some("unknown"), "{r:?}");
    assert_eq!(r.price_note.as_deref(), Some(NOTE), "{r:?}");
    assert_eq!(
        (r.prompt_tokens, r.completion_tokens),
        (Some(1000), Some(200)),
        "the tokens are still known: {r:?}"
    );
    assert_eq!(r.error_kind, None, "an answer, not a failure: {r:?}");
}

#[tokio::test]
async fn a_billable_hosted_call_leaves_the_row_unpriced_with_a_note() {
    for protocol in NATIVE {
        let (base, _up) = upstream(Says::AnswersWith(CALLS)).await;
        let (state, gw) = gateway(protocol, &base).await;
        let (status, body) = post(
            &gw,
            json!({"model": ALIAS, "input": "look it up",
                   "tools": [{"type": "web_search"},
                             {"type": "code_interpreter", "container": {"type": "auto"}}]}),
        )
        .await;
        assert_eq!(status, 200, "{protocol:?}: {body}");
        assert_eq!(
            body["output"][0]["type"], "web_search_call",
            "relayed: {body}"
        );
        assert_unpriced_for_its_calls(&row(&state, 1).await);

        // Listed, with the calls counted.
        let listed = lmgw_core::ops::unpriced_hosted_tools(&state).await.unwrap();
        assert_eq!(listed.len(), 1, "{protocol:?}: {listed:?}");
        let l = &listed[0];
        assert_eq!((l.name.as_str(), l.requests), (ALIAS, 1));
        assert!(l.scope_key.ends_with(":gpt-test"), "{l:?}");
        let calls: Vec<(&str, i64)> = l
            .calls
            .iter()
            .map(|c| (c.item_type.as_str(), c.calls))
            .collect();
        assert_eq!(
            calls,
            [("web_search_call", 2), ("code_interpreter_call", 1)]
        );

        // A second one adds up.
        post(&gw, json!({"model": ALIAS, "input": "again"})).await;
        row(&state, 2).await;
        let listed = lmgw_core::ops::unpriced_hosted_tools(&state).await.unwrap();
        assert_eq!(listed[0].requests, 2, "{listed:?}");
        assert_eq!(listed[0].calls[0].calls, 4, "{listed:?}");
    }
}

#[tokio::test]
async fn a_streamed_answer_counts_the_calls_of_its_completed_event() {
    let (base, _up) = upstream(Says::AnswersWith(CALLS)).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": ALIAS, "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.text().await.unwrap();
    let r = row(&state, 1).await;
    assert!(r.streamed);
    assert_unpriced_for_its_calls(&r);
}

/// Calls that bill no fee of their own leave the row priced as usual.
#[tokio::test]
async fn calls_without_a_fee_of_their_own_are_priced_as_tokens() {
    let (base, _up) = upstream(Says::AnswersWith(&["mcp_call", "computer_call"])).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let (status, _) = post(&gw, json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200);
    let r = row(&state, 1).await;
    assert_metered(&r);
    assert_eq!(r.price_note, None, "{r:?}");
    assert!(lmgw_core::ops::unpriced_hosted_tools(&state)
        .await
        .unwrap()
        .is_empty());
}

/// The list reads the partial index of noted rows, not the whole log.
#[tokio::test]
async fn the_hosted_fee_query_reads_the_noted_rows_index() {
    let (base, _up) = upstream(Says::Answers).await;
    let (state, _gw) = gateway(Protocol::Openai, &base).await;
    let plan: Vec<String> = sqlx::query(sqlx::AssertSqlSafe(format!(
        "EXPLAIN QUERY PLAN {}",
        store::HOSTED_FEE_TRAFFIC_SQL
    )))
    .bind("x")
    .fetch_all(&state.db)
    .await
    .unwrap()
    .iter()
    .map(|r| sqlx::Row::get::<String, _>(r, "detail"))
    .collect();
    assert!(
        plan.iter()
            .any(|d| d.contains("idx_request_logs_price_note")),
        "{plan:?}"
    );
}

/// A stream cut after a web search and before `response.completed` made a
/// call whose fee is in no usage: its row stays on the hosted list, its
/// note naming the call beside the reason the usage never came.
#[tokio::test]
async fn a_stream_cut_after_a_search_is_listed_with_its_call() {
    let (base, _up) = upstream(Says::SearchesThenCuts).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": ALIAS, "input": "look", "stream": true,
                      "tools": [{"type": "web_search"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("web_search_call"), "{text}");
    let r = row(&state, 1).await;
    assert_eq!(r.error_kind.as_deref(), Some("incomplete"), "{r:?}");
    assert_eq!(r.cost_micro, None, "{r:?}");
    // The budgets count a local count of it (§13.4); the fee stays named.
    let (floor, _) = cut_floor("look", "Hel");
    assert_eq!(
        r.price_note.as_deref(),
        Some(
            format!(
                "stream ended before the upstream reported usage; {floor}; hosted tool fees not \
                 priced yet: 1 web_search_call; the tokens are unpriced as well"
            )
            .as_str()
        ),
        "{r:?}"
    );
    let listed = lmgw_core::ops::unpriced_hosted_tools(&state).await.unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].calls[0].item_type, "web_search_call");
    assert_eq!(listed[0].calls[0].calls, 1);
}

/// A budgeted client key that names a hosted tool on every request leaves
/// every row unpriced, but the token part counts toward its budget as a
/// lower bound, so the budget moves and in the end refuses; the rows and
/// the rollup's cost stay unpriced (openai egress design §6.1).
#[tokio::test]
async fn the_token_part_of_a_hosted_row_counts_toward_a_budget() {
    let (base, up) = upstream(Says::AnswersWith(&["web_search_call"])).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    // Two answers' tokens reach it.
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, budget_micro, budget_period)
         VALUES ('searcher', ?1, 1, ?2, 'month')",
    )
    .bind(lmgw_core::config::hash_api_key("lmgw-searcher"))
    .bind(2 * COST_MICRO - 1)
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let send = |n: usize| {
        let gw = gw.clone();
        async move {
            let resp = gw
                .anon()
                .post(format!("{gw}/v1/responses"))
                .bearer_auth("lmgw-searcher")
                .json(&json!({"model": ALIAS, "input": format!("search {n}"),
                              "tool_choice": {"type": "web_search"},
                              "tools": [{"type": "web_search"}]}))
                .send()
                .await
                .unwrap();
            let status = resp.status().as_u16();
            (status, resp.json::<Value>().await.unwrap_or_default())
        }
    };
    for n in 1..=2 {
        let (status, body) = send(n).await;
        assert_eq!(status, 200, "{n}: {body}");
        let r = row(&state, n).await;
        assert_eq!(r.cost_micro, None, "the row stays unpriced: {r:?}");
        assert!(
            r.price_note
                .as_deref()
                .unwrap_or_default()
                .ends_with("which budgets count"),
            "{r:?}"
        );
    }
    let (status, body) = send(3).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "key_budget", "{body}");
    assert_eq!(up.responses().len(), 2, "the refused request never left");

    // The bound is in the rollup beside a cost that stays unpriced, so a
    // re-seed from it (a restart, the minute's reseed) still sees it.
    let (cost, floor, unknown): (i64, i64, i64) = sqlx::query_as(
        "SELECT SUM(cost_micro), SUM(budget_floor_micro), SUM(cost_unknown_requests)
           FROM usage_hourly WHERE alias = ?1 AND outcome = 'ok'",
    )
    .bind(ALIAS)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!((cost, floor, unknown), (0, 2 * COST_MICRO, 2));
}

/// Each row's note carries its own token amount, so no two rows share one:
/// the list still has one entry per alias and upstream model, its requests
/// counted and the calls each note names summed.
#[tokio::test]
async fn the_list_has_one_entry_per_model_whatever_each_note_says() {
    let (base, _up) = upstream(Says::Answers).await;
    let (state, _gw) = gateway(Protocol::Openai, &base).await;
    let up = store::list_upstreams(&state.db).await.unwrap()[0].id;
    for (tokens, searches, model) in [
        (Some(1_000), 1, "gpt-test"),
        (Some(2_500), 3, "gpt-test"),
        (None, 2, "gpt-test"),
        (Some(700), 1, "gpt-other"),
    ] {
        let mut calls = lmgw_core::pricing::HostedCalls::default();
        calls.add("web_search_call", searches);
        store::insert_request_log(
            &state.db,
            &store::NewRequestLog {
                ingress_proto: "responses".into(),
                requested_alias: ALIAS.into(),
                upstream_id: Some(up),
                upstream_model: Some(model.into()),
                status: 200,
                price_note: Some(calls.note(tokens, "USD")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    let listed = lmgw_core::ops::unpriced_hosted_tools(&state).await.unwrap();
    let got: Vec<(String, i64, i64)> = listed
        .iter()
        .map(|l| (l.scope_key.clone(), l.requests, l.calls[0].calls))
        .collect();
    assert_eq!(
        got,
        [
            (lmgw_core::config::upstream_scope_key(up, "gpt-test"), 3, 6),
            (lmgw_core::config::upstream_scope_key(up, "gpt-other"), 1, 1),
        ]
    );
}
