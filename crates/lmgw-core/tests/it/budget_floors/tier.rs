//! A tier gap above Standard floors at Standard (openai egress design
//! §13.2): every tier that costs at least Standard, and an answer that named
//! no price tier, counts toward budgets at the standard price, or at the
//! flex price where no standard one is; a served flex and a value lmgw does
//! not know do not.

use super::*;

const CLAUSE: &str = "budgets count it at the standard price, 0.002800 USD, a lower bound";

/// Each served value against a standard-only scope: the row stays
/// unpriced, its note keeps its opening words and gains the clause, and the
/// spend the budgets read moves by the standard price.
#[tokio::test]
async fn every_tier_at_least_standard_floors_at_the_standard_price() {
    for served in [
        Some("fast"),
        Some("priority"),
        Some("ultrafast"),
        Some("scale"),
        Some("auto"),
        None,
    ] {
        let (_m, state, gw) =
            platform(ResponseTemplate::new(200).set_body_json(answer(served))).await;
        price(&state, PriceTier::Standard, 2.0, 8.0).await;
        let (status, body) = chat(&gw, json!({}), None).await;
        assert_eq!(status, 200, "{served:?}: {body}");
        let row = row_after(&state, 0).await;
        assert_eq!(row.cost_micro, None, "{served:?}: the row stays unpriced");
        assert_eq!(row.price_source.as_deref(), Some("unknown"), "{served:?}");
        let note = row.price_note.clone().unwrap_or_default();
        assert!(note.ends_with(CLAUSE), "{served:?}: {note}");
        if served.is_none() {
            assert!(
                note.starts_with(lmgw_core::pricing::NO_TIER_NOTE),
                "the list still finds it: {note}"
            );
        }
        assert_eq!(
            floor_of(&state, row.id).await,
            Some(STANDARD_MICRO),
            "{served:?}"
        );
        assert_eq!(spend(&state).await, STANDARD_MICRO, "{served:?}");
    }
}

/// A served flex is below Standard, and a value §5.4 does not map is not
/// known to be above it: neither floors.
#[tokio::test]
async fn flex_and_an_unknown_value_have_no_floor() {
    for served in ["flex", "turbo"] {
        let (_m, state, gw) =
            platform(ResponseTemplate::new(200).set_body_json(answer(Some(served)))).await;
        price(&state, PriceTier::Standard, 2.0, 8.0).await;
        let (status, _) = chat(&gw, json!({}), None).await;
        assert_eq!(status, 200);
        let row = row_after(&state, 0).await;
        assert_eq!(row.cost_micro, None, "{served}");
        assert!(
            !row.price_note
                .clone()
                .unwrap_or_default()
                .contains("budgets count"),
            "{served}: {row:?}"
        );
        assert_eq!(floor_of(&state, row.id).await, None, "{served}");
        assert_eq!(spend(&state).await, 0, "{served}");
    }
}

/// A scope with a flex row only: flex is the cheapest tier, so its price
/// bounds every tier above it, a served `default` included.
#[tokio::test]
async fn a_flex_only_scope_floors_every_tier_above_it_at_the_flex_price() {
    for served in [Some("default"), Some("priority"), Some("auto"), None] {
        let (_m, state, gw) =
            platform(ResponseTemplate::new(200).set_body_json(answer(served))).await;
        price(&state, PriceTier::Flex, 1.0, 4.0).await;
        let (status, body) = chat(&gw, json!({}), None).await;
        assert_eq!(status, 200, "{served:?}: {body}");
        let row = row_after(&state, 0).await;
        assert_eq!(row.cost_micro, None, "{served:?}: the row stays unpriced");
        let note = row.price_note.clone().unwrap_or_default();
        assert!(
            note.ends_with("budgets count it at the flex price, 0.001400 USD, a lower bound"),
            "{served:?}: {note}"
        );
        assert_eq!(
            floor_of(&state, row.id).await,
            Some(FLEX_MICRO),
            "{served:?}"
        );
        assert_eq!(spend(&state).await, FLEX_MICRO, "{served:?}");
    }
}

/// Flex asked for and no tier named in the answer: it may have been flex,
/// so the flex price bounds it where one is on file, and nothing does where
/// none is.
#[tokio::test]
async fn no_tier_named_after_asking_for_flex_floors_at_the_flex_price() {
    for served in [Some("auto"), None] {
        let (_m, state, gw) =
            platform(ResponseTemplate::new(200).set_body_json(answer(served))).await;
        price(&state, PriceTier::Standard, 2.0, 8.0).await;
        price(&state, PriceTier::Flex, 1.0, 4.0).await;
        let (status, _) = chat(&gw, json!({"service_tier": "flex"}), None).await;
        assert_eq!(status, 200);
        let row = row_after(&state, 0).await;
        assert_eq!(row.cost_micro, None, "{served:?}");
        assert!(
            row.price_note
                .clone()
                .unwrap_or_default()
                .ends_with("budgets count it at the flex price, 0.001400 USD, a lower bound"),
            "{served:?}: {row:?}"
        );
        assert_eq!(floor_of(&state, row.id).await, Some(FLEX_MICRO));

        // Without a flex row, and without a budget to refuse it: no bound.
        let (_m, state, gw) =
            platform(ResponseTemplate::new(200).set_body_json(answer(served))).await;
        price(&state, PriceTier::Standard, 2.0, 8.0).await;
        let (status, _) = chat(&gw, json!({"service_tier": "flex"}), None).await;
        assert_eq!(status, 200);
        let row = row_after(&state, 0).await;
        assert_eq!(floor_of(&state, row.id).await, None, "{served:?}");
    }
}

/// Streamed, the floor is the same: the tier comes from the terminal
/// event (the Responses wire, openai egress design §7.2.3).
#[tokio::test]
async fn a_streamed_fast_answer_floors_too() {
    let mut done = answer(Some("priority"));
    done["service_tier"] = json!("priority");
    let chunks = crate::support::responses_wire::sse(&[
        json!({"type": "response.created", "response": {"service_tier": "auto"}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "pong"}),
        json!({"type": "response.completed", "response": done}),
    ]);
    let (_m, state, gw) = platform(
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
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    resp.text().await.unwrap();
    let row = row_after(&state, 0).await;
    assert!(row.streamed);
    assert_eq!(row.cost_micro, None);
    assert_eq!(floor_of(&state, row.id).await, Some(STANDARD_MICRO));
}

/// A budgeted key whose answers come back at Fast with no Fast row: each
/// counts at the standard price, so its budget moves and in the end
/// refuses, and the reseed reads the same spend.
#[tokio::test]
async fn a_key_s_fast_answers_count_toward_its_budget() {
    let (m, state, gw) =
        platform(ResponseTemplate::new(200).set_body_json(answer(Some("priority")))).await;
    price(&state, PriceTier::Standard, 2.0, 8.0).await;
    budgeted_key(&state, "fast", 2 * STANDARD_MICRO - 1).await;
    for n in 0..2 {
        let (status, body) = chat(&gw, json!({}), Some("lmgw-fast")).await;
        assert_eq!(status, 200, "{n}: {body}");
        row_after(&state, n).await;
    }
    let (status, body) = chat(&gw, json!({}), Some("lmgw-fast")).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "key_budget", "{body}");
    assert_eq!(m.received_requests().await.unwrap().len(), 2);
    let key: i64 = sqlx::query_scalar("SELECT id FROM api_keys WHERE name = 'fast'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(
        store::key_spend_micro(&state.db, key, "0000-01-01T00")
            .await
            .unwrap(),
        2 * STANDARD_MICRO
    );
}
