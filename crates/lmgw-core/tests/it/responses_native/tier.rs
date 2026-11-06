//! The service tier on the native path (openai egress design §5.2, §5.6):
//! the platform's Response object says which tier served the request, the
//! row records it and is priced with that tier's rows, and a tier with no
//! row leaves it unpriced with a note, never priced at Standard. A compatible
//! row has no tier dimension, whatever its provider sends.

use lmgw_core::config::PriceTier;

use super::*;

/// [`price`]'s sheet again, at `tier`.
async fn price_at(state: &SharedState, tier: PriceTier) {
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        ALIAS,
        PriceUnit::PerMtok,
        tier,
        &Prices {
            price_in: Some(1.0),
            price_out: Some(4.0),
            price_cache_read: Some(0.25),
            price_cache_write: Some(1.25),
            source: PriceSource::Manual,
        },
        None,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

#[tokio::test]
async fn a_flex_answer_with_only_a_standard_row_is_unpriced_with_a_note() {
    let (base, _up) = upstream(Says::AnswersAt(Some("flex"))).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let (status, body) = post(
        &gw,
        json!({"model": ALIAS, "input": "hi", "service_tier": "flex"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["service_tier"], "flex", "relayed as sent: {body}");

    let r = row(&state, 1).await;
    assert_eq!(r.service_tier.as_deref(), Some("flex"), "{r:?}");
    assert_eq!(r.cost_micro, None, "never priced at standard: {r:?}");
    assert_eq!(r.price_source.as_deref(), Some("unknown"), "{r:?}");
    assert_eq!(
        r.price_note.as_deref(),
        Some(
            "served at service tier flex (requested: flex); no flex price on file for \
             'gpt-test', a standard one is"
        ),
        "{r:?}"
    );
    assert_eq!(r.prompt_tokens, Some(1000), "the tokens are still known");

    // The Prices page lists it, and a flex row takes it off the list and
    // prices what comes next.
    let gaps = lmgw_core::ops::unpriced_tiers(&state).await.unwrap();
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!(
        (
            gaps[0].name.as_str(),
            gaps[0].service_tier.as_str(),
            gaps[0].requests
        ),
        (ALIAS, "flex", 1)
    );
    assert_eq!(gaps[0].scope_kind, "upstream_model");
    price_at(&state, PriceTier::Flex).await;
    assert!(lmgw_core::ops::unpriced_tiers(&state)
        .await
        .unwrap()
        .is_empty());
    let (status, _) = post(&gw, json!({"model": ALIAS, "input": "again"})).await;
    assert_eq!(status, 200);
    let r = row(&state, 2).await;
    // 300 × 1 + 600 × 0.25 + 100 × 1.25 + 200 × 4
    assert_eq!(r.cost_micro, Some(300 + 150 + 125 + 800), "{r:?}");
    assert_eq!(r.price_note, None, "{r:?}");
}

/// `priority` is Fast (the renaming); a Fast row prices it.
#[tokio::test]
async fn a_priority_answer_is_priced_with_the_fast_rows() {
    let (base, _up) = upstream(Says::AnswersAt(Some("priority"))).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    price_at(&state, PriceTier::Fast).await;
    let (status, _) = post(&gw, json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200);
    let r = row(&state, 1).await;
    assert_eq!(r.service_tier.as_deref(), Some("priority"));
    assert_eq!(r.cost_micro, Some(300 + 150 + 125 + 800), "{r:?}");
}

/// An answer that names no tier is an unknown tier, not Standard (§5.2).
#[tokio::test]
async fn an_answer_without_a_tier_is_unpriced_not_standard() {
    let (base, _up) = upstream(Says::AnswersAt(None)).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let (status, body) = post(&gw, json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200);
    assert!(
        body.get("service_tier").is_none(),
        "nothing invented: {body}"
    );
    let r = row(&state, 1).await;
    assert_eq!(r.service_tier, None);
    assert_eq!(r.cost_micro, None, "{r:?}");
    assert!(
        r.price_note
            .as_deref()
            .is_some_and(|n| n.starts_with("the upstream reported no service tier")),
        "{r:?}"
    );
    // No row can be entered for it: listed without a tier, so it can be found.
    let gaps = lmgw_core::ops::unpriced_tiers(&state).await.unwrap();
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!(
        (gaps[0].service_tier.as_str(), gaps[0].reported.as_str()),
        ("", "")
    );
}

/// The platform names the serving tier only to a request that asked for one,
/// so a request that set none is sent `auto` (decision 14): the row's note
/// still names what the client asked, which is nothing. A tier the client
/// set goes out as set; a compatible row is sent nothing.
#[tokio::test]
async fn the_platform_is_asked_for_the_tier_when_the_client_set_none() {
    let (base, up) = upstream(Says::AnswersAt(Some("flex"))).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let (status, _) = post(&gw, json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200);
    let (status, _) = post(
        &gw,
        json!({"model": ALIAS, "input": "hi", "service_tier": "flex"}),
    )
    .await;
    assert_eq!(status, 200);
    let sent = up.responses();
    assert_eq!(sent[0]["service_tier"], "auto", "{sent:?}");
    assert_eq!(sent[1]["service_tier"], "flex", "{sent:?}");
    let r = row(&state, 1).await;
    assert!(
        !r.price_note.unwrap_or_default().contains("requested"),
        "auto is not the client's request"
    );

    let (base, up) = upstream(Says::AnswersAt(Some("flex"))).await;
    let (_state, gw) = gateway(Protocol::OpenaiCompatible, &base).await;
    let (status, _) = post(&gw, json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200);
    assert!(up.responses()[0].get("service_tier").is_none());
}

/// On a compatible row the dimension does not exist: a provider's
/// `service_tier` is relayed untouched (passthrough) but neither recorded
/// nor priced by.
#[tokio::test]
async fn a_compatible_row_has_no_tier() {
    let (base, _up) = upstream(Says::AnswersAt(Some("flex"))).await;
    let (state, gw) = gateway(Protocol::OpenaiCompatible, &base).await;
    let (status, _) = post(&gw, json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200);
    let r = row(&state, 1).await;
    assert_eq!(r.service_tier, None, "{r:?}");
    assert_eq!(r.cost_micro, Some(COST_MICRO), "standard rows: {r:?}");
    assert_eq!(r.price_note, None);
}

/// A stream cut before its terminal event never reported usage: its row
/// keeps the `incomplete` error and says why it is unpriced in the price
/// note, not as a tier it never heard of.
#[tokio::test]
async fn a_cut_stream_says_why_in_its_price_note() {
    let (base, _up) = upstream(Says::Cuts).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": ALIAS, "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    let _ = resp.text().await;
    let r = row(&state, 1).await;
    assert_eq!(r.error_kind.as_deref(), Some("incomplete"), "{r:?}");
    // What the budgets counted of it follows (§13.4, `budget_floor`).
    let (floor, _) = cut_floor("hi", "Hel");
    assert_eq!(
        r.price_note,
        Some(format!(
            "stream ended before the upstream reported usage; {floor}"
        )),
        "{r:?}"
    );
    assert_eq!(r.cost_micro, None);
}

/// The synthesized loop on the platform (a chain lmgw stored): the turn's
/// chat answer says `default`, the turn's row records it and is priced at
/// Standard, and the Responses object lmgw builds carries it as OpenAI's
/// would (§5.7). The stream's `response.completed` is the same object (one
/// encoder for both modes, `ResponsesEncoder`).
#[tokio::test]
async fn the_synthesized_loop_relays_and_prices_the_turns_tier() {
    let (base, up) = upstream(Says::Answers).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    super::routing::store_response(&state).await;
    let (status, body) = post(
        &gw,
        json!({"model": ALIAS, "input": "and then?",
               "previous_response_id": super::routing::STORED}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(up.chats(), 1, "the loop answered: {body}");
    assert_eq!(body["service_tier"], "default", "{body}");
    let r = row(&state, 1).await;
    assert_eq!(r.service_tier.as_deref(), Some("default"), "{r:?}");
    // 5 in × 2 + 3 out × 8
    assert_eq!(r.cost_micro, Some(10 + 24), "{r:?}");
}

/// A whole answer without usage: the same, in the body's words, and on the
/// platform the budgets count a local count of what was sent and answered
/// (openai egress design §13.4).
#[tokio::test]
async fn a_body_without_usage_says_why_in_its_price_note() {
    let (base, _up) = upstream(Says::NoUsage).await;
    let (state, gw) = gateway(Protocol::Openai, &base).await;
    let (status, _) = post(&gw, json!({"model": ALIAS, "input": "hi"})).await;
    assert_eq!(status, 200);
    let r = row(&state, 1).await;
    assert_eq!(r.error_kind.as_deref(), Some("incomplete"));
    let (clause, _) = cut_floor("hi", "Hello");
    assert_eq!(
        r.price_note,
        Some(format!("the upstream's answer reported no usage; {clause}")),
        "{r:?}"
    );
}
