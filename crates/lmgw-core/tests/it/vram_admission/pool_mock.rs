//! The pool mock itself

use super::*;

/// Proves the mock's own overflow arithmetic before anything in lmgw is asked
/// to respect it: two requests to the same **unguarded** row (no ladder, no
/// `kv_unified` guard — exactly what exists on `chat-model` today) that
/// overlap in time and together exceed the port's configured pool capacity
/// must trip `pool_overflows`. Nothing in lmgw is expected to prevent this —
/// there is no gate yet — so this only exercises the fixture, ahead of the
/// gate that will (a later change) make an equivalent request wait instead.
#[tokio::test]
async fn the_pool_mock_flags_an_overlap_that_exceeds_its_capacity() {
    let f = fixture(64 * GIB, GIB, GIB, 512).await;

    // Warm the model on a real (non-overlapping) request first, so the two
    // that follow race a resident container rather than a cold start.
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let port = f.first.address().port();
    assert_eq!(
        f.world().runs,
        vec!["chat-model".to_string()],
        "chat-model is the only model started, so it is on the first port"
    );

    {
        let mut w = f.world();
        // chat()'s body renders to "user: hi" (2 words) and sends no
        // max_tokens, so each request's need is 2 + the mock's stand-in
        // default (16) = 18: one alone fits comfortably under 30, but two
        // overlapping ones sum to 36 and clear it.
        w.pool_capacity.insert(port, 30);
        w.pool_delay.insert(port, Duration::from_millis(300));
    }

    let (a, b) = tokio::join!(chat(&f.gateway), chat(&f.gateway));
    let statuses = [a.status().as_u16(), b.status().as_u16()];
    // The mock answers the overflowing one with fact 3's verbatim 500; lmgw's
    // own `GatewayError::Upstream` mapping (`error.rs` `http_status`) folds any
    // upstream status outside its client-attributable list into 502 before it
    // reaches this client — so 502, not 500, is what a caller here sees.
    assert!(
        statuses.contains(&502),
        "the overflowing request's upstream 500 surfaces as lmgw's 502: {statuses:?}"
    );
    assert!(
        f.world().pool_overflows >= 1,
        "two overlapping requests over capacity must trip the overflow counter"
    );
}
