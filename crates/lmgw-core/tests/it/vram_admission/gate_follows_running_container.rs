//! The gate follows the running container (second review, finding 1)

use super::*;

/// Second review, finding 1: the gate's numbers are the *running container's*. A busy
/// guarded model whose row is edited to a larger pool keeps its container —
/// the apply is refused as busy — and the ledger keeps admitting against the
/// pool that container really has: a second request that fits only the new
/// pool waits, and the pool-mode mock (still 64, like the container) never
/// overflows. Once the model restarts, the new capacity applies.
#[tokio::test]
async fn a_busy_rows_bigger_pool_applies_only_from_its_restart() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    let port = warm_pool(&f, 64, Duration::from_millis(3_000)).await;
    // Whatever A takes, B may wait it out: the order under test is the
    // ledger's, not the queue timeout's.
    set_queue_timeout(&f, 0).await;

    let send = |content: String| {
        let gw = f.gateway.clone();
        tokio::spawn(async move { sized_chat(&gw, &content, 30).await.status().as_u16() })
    };
    let a = send(words("a", 9));
    until_pool(&f, "A in flight", |p| p["in_flight"] == 1).await;

    let note = edit_chat_model(&f, |p| p.ctx_size = Some(128))
        .await
        .expect("chat-model is running");
    assert!(
        note.contains("still serving 1 request"),
        "the apply was refused as busy: {note}"
    );
    // 40 + 40 = 80: fits the edited row's 128, not the running container's 64.
    let b = send(words("b", 9));
    until_pool(&f, "B queued", |p| queued(p) == 1).await;
    let v = vram_status(&f.gateway).await;
    assert_eq!(
        pools(&v)[0]["capacity_tokens"],
        64,
        "the pool is the container's, not the edited row's: {v}"
    );

    assert_eq!((a.await.unwrap(), b.await.unwrap()), (200, 200));
    assert_eq!(f.world().pool_overflows, 0, "the old pool never overflowed");
    let served = f.world().pool_served[&port].clone();
    assert!(
        served[1].0 >= served[0].1,
        "B reached the model only after A's window ended: {served:?}"
    );

    // Idle now: the apply goes through, and the next start runs the edit.
    let note = edit_chat_model(&f, |_| {}).await.expect("still running");
    assert!(note.contains("was stopped"), "{note}");
    let second = f._second.address().port();
    {
        let mut w = f.world();
        w.pool_capacity.insert(second, 128);
        w.pool_delay.insert(second, Duration::from_millis(3_000));
    }
    let c = send(words("c", 9));
    until_pool(&f, "C in flight", |p| p["in_flight"] == 1).await;
    assert_eq!(chat_port(&f), second, "the model restarted");
    let d = send(words("d", 9));
    until_pool(&f, "C and D in flight together", |p| {
        p["in_flight"] == 2 && p["capacity_tokens"] == 128
    })
    .await;
    assert_eq!((c.await.unwrap(), d.await.unwrap()), (200, 200));
    assert_eq!(
        f.world().pool_overflows,
        0,
        "the restarted container's pool is 128, and 80 fit"
    );
}

/// Second review, finding 1, the other direction: turning `kv_unified` off on a busy
/// guarded row makes the *row* unguarded, but the container keeps running its
/// shared pool until it restarts — so it stays guarded: the clamp still binds
/// to the `n_predict` it was started with, and a request that would overflow
/// the running pool still waits.
#[tokio::test]
async fn turning_kv_unified_off_on_a_busy_row_keeps_its_container_guarded() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    guard_chat_model(&f, 64, 32).await;
    let port = warm_pool(&f, 64, Duration::from_millis(3_000)).await;
    set_queue_timeout(&f, 0).await;

    let a = {
        let gw = f.gateway.clone();
        tokio::spawn(async move { sized_chat(&gw, &words("a", 9), 30).await.status().as_u16() })
    };
    until_pool(&f, "A in flight", |p| p["in_flight"] == 1).await;

    let note = edit_chat_model(&f, |p| p.kv_unified = Some(false))
        .await
        .expect("chat-model is running");
    assert!(note.contains("still serving 1 request"), "{note}");
    let row_guarded = f
        .state
        .snapshot()
        .local_models
        .iter()
        .find(|m| m.model_id == "chat-model")
        .map(|m| m.params.pool_guarded());
    assert_eq!(row_guarded, Some(false), "the row itself is unguarded now");

    // 1 + 9 + min(100, 32) = 42: waits behind A's 40 in the running pool.
    let b = {
        let gw = f.gateway.clone();
        tokio::spawn(async move {
            let r = sized_chat(&gw, &words("b", 9), 100).await;
            let clamped = r
                .headers()
                .get("x-lmgw-max-tokens-clamped")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            (r.status().as_u16(), clamped)
        })
    };
    until_pool(&f, "B queued", |p| queued(p) == 1).await;

    assert_eq!(a.await.unwrap(), 200);
    let (b_status, b_clamped) = b.await.unwrap();
    assert_eq!(b_status, 200);
    assert_eq!(
        b_clamped.as_deref(),
        Some("32"),
        "clamped to the n_predict the container was started with"
    );
    assert_eq!(f.world().pool_overflows, 0);
    let w = f.world();
    let served = &w.pool_served[&port];
    assert!(served[1].0 >= served[0].1, "{served:?}");
    assert_eq!(w.chat_bodies[&port].last().unwrap()["max_tokens"], 32);
}
