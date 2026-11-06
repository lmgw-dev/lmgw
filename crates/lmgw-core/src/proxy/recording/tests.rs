//! What `price_call` folds into the policy's windows (A2 review 3, B2 review
//! 9): a key that exists gets its tokens; one deleted while its call ran
//! gets no window — nothing would ever read it again.

use super::{price_call, KeyRef};
use crate::ir::Usage;
use crate::state::AppState;

#[tokio::test]
async fn a_key_deleted_while_its_call_ran_gets_no_token_window() {
    let state = AppState::init_for_tests().await.unwrap();
    sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES ('live', 'h', 1)")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let (live,): (i64,) = sqlx::query_as("SELECT id FROM api_keys WHERE name = 'live'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let usage = Usage {
        prompt_tokens: Some(10),
        completion_tokens: Some(5),
        ..Default::default()
    };
    let none = crate::pricing::Quantities::default();
    let call = |key: KeyRef| {
        price_call(
            &state,
            "chatty",
            None,
            &key,
            "openai",
            &usage,
            &none,
            &Default::default(),
            &Default::default(),
            None,
        )
    };

    call(KeyRef {
        name: Some("live".into()),
        id: Some(live),
        ..Default::default()
    });
    assert!(
        state.policy.tracks(live),
        "a key that exists counts its tokens"
    );

    // The key's id outlives it on the row (`KeyRef`), but not in the gate.
    let gone = live + 1000;
    let priced = call(KeyRef {
        name: Some("gone".into()),
        id: Some(gone),
        ..Default::default()
    });
    assert_eq!(priced.key_id, Some(gone), "the row keeps the id");
    assert_eq!(priced.client_key.as_deref(), Some("gone"));
    assert!(!state.policy.tracks(gone), "no window for a deleted key");
}

/// A handler dropped while its row is being written — its client went away
/// mid-insert — neither loses the row nor leaves the in-flight gauge open
/// (`recording/write.rs`).
#[tokio::test]
async fn a_row_dropped_mid_write_still_lands_and_closes_the_gauge() {
    use futures::FutureExt;

    let state = AppState::init_for_tests().await.unwrap();
    state.telemetry.request_started();
    let ctx = super::RequestCtx::default();
    let row = super::record(
        super::LogParams {
            state: &state,
            proto: crate::ingress::ClientProto::OpenaiChat,
            ctx: &ctx,
            alias: "dropped".into(),
            route: None,
            started: std::time::Instant::now(),
            streamed: false,
            class: crate::telemetry::RequestClass::Chat,
            timings: None,
            max_tokens_clamped: None,
            fallback: None,
            rung: None,
            degraded: None,
            quantities: Default::default(),
            service_tier: Default::default(),
        },
        200,
        None,
        Usage::default(),
        None,
    );
    // Polled once — as far as the insert — then dropped, as hyper drops it.
    assert!(
        row.now_or_never().is_none(),
        "the insert is still on its way"
    );

    for _ in 0..500 {
        if state.telemetry.stats().active_requests == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        state.telemetry.stats().active_requests,
        0,
        "the gauge is closed"
    );
    let rows = crate::store::query_logs(&state.db, &Default::default())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "and the row landed");
    assert_eq!(rows[0].requested_alias, "dropped");
}

/// The per-call check reads the key again, by id and by credential
/// (client-apps design §1.6, review W2-2): a key disabled, rotated or
/// deleted since the request was resolved fails its next call — the
/// backstop behind every revocation watch.
#[tokio::test]
async fn a_call_after_a_disable_a_rotate_or_a_delete_is_refused() {
    use crate::ingress::ClientProto;
    use crate::principal::Principal;
    use crate::telemetry::RequestClass;

    let state = AppState::init_for_tests().await.unwrap();
    let hash = |k: &str| crate::config::hash_api_key(k);
    sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES ('phone', ?1, 1)")
        .bind(hash("lmgw-old"))
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let snap = state.snapshot();
    let key = snap.api_keys.iter().find(|k| k.name == "phone").unwrap();
    let ctx = super::RequestCtx {
        principal: Principal::from_key(key),
        client_key: Some("phone".into()),
        ..Default::default()
    };
    let call = || {
        super::policy_checked_call(
            &state,
            ClientProto::Chat,
            &ctx,
            "chatty",
            RequestClass::Chat,
        )
    };
    assert!(call().await.is_ok(), "the key it was resolved with");

    // Rotated: the same row, another credential.
    sqlx::query("UPDATE api_keys SET key_hash = ?1 WHERE name = 'phone'")
        .bind(hash("lmgw-new"))
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let e = call().await.unwrap_err();
    assert_eq!(e.http_status().as_u16(), 401, "{e}");
    assert!(e.to_string().contains("rotated"), "{e}");

    // Back to its own credential, then disabled: refused too.
    sqlx::query("UPDATE api_keys SET key_hash = ?1, enabled = 0 WHERE name = 'phone'")
        .bind(hash("lmgw-old"))
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    assert!(call().await.is_err());

    // Deleted: refused.
    sqlx::query("DELETE FROM api_keys WHERE name = 'phone'")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    assert!(call().await.is_err());
}

/// The row's note keeps every reason it is unpriced: the tier's words first,
/// so the no-tier list still reads them by their opening, then the hosted
/// calls', which the hosted list finds anywhere; an answer whose usage never
/// came replaces the tier's (moot then) but never the hosted calls'.
#[test]
fn the_price_note_joins_the_reasons_and_keeps_the_hosted_calls() {
    use crate::pricing::{HostedCalls, NO_TIER_NOTE};
    let mut calls = HostedCalls::default();
    calls.add("web_search_call", 2);
    let hosted = calls.note(None, "USD");
    let tier = format!("{NO_TIER_NOTE}; unpriced rather than assumed standard");
    let priced = |tier_note: Option<&str>, hosted_note: Option<&str>| super::Priced {
        client_key: None,
        key_id: None,
        cost: crate::pricing::Cost::unknown(),
        tier_note: tier_note.map(str::to_string),
        hosted_note: hosted_note.map(str::to_string),
        floor_note: None,
        priced_alias: None,
    };

    let both = priced(Some(&tier), Some(&hosted)).price_note(None).unwrap();
    assert!(both.starts_with(NO_TIER_NOTE), "{both}");
    assert_eq!(HostedCalls::from_note(&both), calls);

    let cut = "stream ended before the upstream reported usage";
    let note = priced(Some(&tier), Some(&hosted))
        .price_note(Some(cut))
        .unwrap();
    assert_eq!(
        note,
        format!("{cut}; {hosted}"),
        "the tier is moot, the calls are not"
    );
    assert_eq!(HostedCalls::from_note(&note), calls);

    assert_eq!(
        priced(Some(&tier), None).price_note(Some(cut)).as_deref(),
        Some(cut)
    );
    assert_eq!(
        priced(None, Some(&hosted)).price_note(None),
        Some(hosted.clone())
    );
    assert_eq!(priced(Some(&tier), None).price_note(None), Some(tier));
    assert_eq!(priced(None, None).price_note(None), None);
}

/// A tier gap above Standard beside a hosted call (openai egress design
/// §13.2): the fees stay unpriced either way, and the budgets count the
/// floor sheet's price of what the call did price.
#[tokio::test]
async fn a_tier_gap_with_hosted_calls_floors_at_the_standard_price() {
    use crate::config::{PriceScope, PriceTier, PriceUnit, Protocol};
    use crate::pricing::{HostedCalls, PriceSource, Prices, ServedTier};

    let state = AppState::init_for_tests().await.unwrap();
    crate::store::upsert_price(
        &state.db,
        PriceScope::Alias,
        "chatty",
        PriceUnit::PerMtok,
        PriceTier::Standard,
        &Prices {
            price_in: Some(2.0),
            price_out: Some(8.0),
            source: PriceSource::Manual,
            ..Default::default()
        },
        None,
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let usage = Usage {
        prompt_tokens: Some(1000),
        completion_tokens: Some(100),
        ..Default::default()
    };
    let mut calls = HostedCalls::default();
    calls.add("web_search_call", 1);
    let served = ServedTier::of(Protocol::Openai, Some("priority".into()), None);
    let priced = price_call(
        &state,
        "chatty",
        None,
        &KeyRef::default(),
        "responses",
        &usage,
        &crate::pricing::Quantities::default(),
        &served,
        &calls,
        None,
    );
    assert_eq!(priced.cost.total_micro, None);
    assert_eq!(priced.cost.budget_floor_micro, Some(2000 + 800));
    let note = priced.price_note(None).unwrap();
    assert!(
        note.starts_with("served at service tier priority; no fast price on file"),
        "{note}"
    );
    assert!(
        note.contains("budgets count it at the standard price, 0.002800 USD, a lower bound; "),
        "{note}"
    );
    assert_eq!(HostedCalls::from_note(&note), calls);
}
