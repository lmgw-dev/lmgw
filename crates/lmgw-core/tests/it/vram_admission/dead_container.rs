//! The dead container (§3.2)

use super::*;

/// A `ready` entry is only ever proof that the container was up when the claim
/// was taken. It can die between that instant and the forward — OOM killer, a
/// `podman stop` from a shell, llama-server crashing on a malformed request —
/// and lmgw finds out the way every other client would: the connection is
/// refused.
///
/// The registry lock cannot close that window (it is a fact about the world,
/// not about the map), so the forward closes it: force-stop the entry, which
/// is honest because the container is already dead, re-acquire through the
/// same admit path, and re-issue the request **once** against the container
/// that came up. Exactly once — a retry loop against a model that cannot start
/// would turn one dead container into an unbounded series of them.
#[tokio::test]
async fn a_dead_container_is_force_stopped_re_acquired_and_the_request_retried_once() {
    let mut f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;

    // Warm it up the normal way, so the registry holds a `ready` entry with a
    // real port.
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
    let dead_port = f.first.address().port();

    // …and now the container dies, without lmgw being told.
    f.kill_first_container().await;
    assert_eq!(
        f.state.runtime().list()[0].port,
        dead_port,
        "lmgw still believes the corpse is the endpoint — that is the premise"
    );

    let resp = chat(&f.gateway).await;
    assert_eq!(
        resp.status(),
        200,
        "the retry has to answer the request, not surface the corpse: {}",
        resp.text().await.unwrap_or_default()
    );

    assert_eq!(
        f.stops(),
        vec!["chat-model".to_string()],
        "the dead entry is stopped exactly once, forced"
    );
    assert_eq!(
        f.runs(),
        vec!["chat-model".to_string(), "chat-model".to_string()],
        "and re-acquired exactly once — not a retry loop"
    );

    // The request landed on the container the *re-acquire* started (the next
    // port the allocator had), which is the only way it could have succeeded:
    // the retry is rebuilt against the fresh port, never replayed at the old
    // one.
    let live = f.state.runtime().list();
    assert_eq!(live.len(), 1);
    assert_ne!(live[0].port, dead_port);
    assert_eq!(live[0].port, f._second.address().port());
    assert!(f
        ._second
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .any(|r| r.url.path() == "/v1/chat/completions"));
    assert_eq!(
        live[0].in_flight, 0,
        "the replacement claim was released with the request, and the stale \
         guard did not decrement it a second time"
    );
}

/// A stale hold's recovery stops only the container it failed against
/// (ladder design §12 entry 20).
///
/// The shape: a tool loop holds a claim across turns; the owner force-stops
/// the model; the next request starts it again on a new container and is
/// still being served there when the loop's next turn meets the dead port.
/// The recovery used to force-stop "the model" — which by then was the newer
/// container, cutting off the request that had started it — and then cold
/// start a third one. Now the stop is generation-checked: the newer container
/// is left alone and the loop's claim simply joins it.
#[tokio::test]
async fn a_stale_holds_recovery_never_stops_the_container_that_replaced_its_dead_one() {
    let mut f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let route = f.state.snapshot().resolve("chat-model").unwrap();
    let stale = lmgw_core::vram::admit(&f.state, &route, "chat-model")
        .await
        .unwrap()
        .expect("a local model is held");
    assert_eq!(stale.port(), f.first.address().port());

    // The owner's override stop, and the container is gone with it.
    f.state
        .runtime()
        .stop(lmgw_core::runtime::Class::Chat, "chat-model", true)
        .await
        .unwrap();
    f.kill_first_container().await;
    // Another request starts the model again and is still being served.
    let live = lmgw_core::vram::admit(&f.state, &route, "chat-model")
        .await
        .unwrap()
        .expect("started again");
    assert_eq!(live.port(), f._second.address().port());

    // The loop's next turn: its hold still names the dead port.
    let resp = lmgw_core::vram::send_local(Some(&stale), &route, None, |r| {
        Ok(reqwest::Client::new()
            .post(format!("{}/chat/completions", r.upstream.base_url))
            .json(&json!({"model": "chat-model",
                          "messages": [{"role": "user", "content": "hi"}]})))
    })
    .await
    .expect("recovered onto the container that runs now");
    assert_eq!(resp.status(), 200);
    assert_eq!(stale.port(), live.port(), "the stale claim joined it");
    assert_eq!(
        f.stops(),
        vec!["chat-model".to_string()],
        "only the owner's stop — the newer container was spared"
    );
    assert_eq!(
        f.runs(),
        vec!["chat-model".to_string(), "chat-model".to_string()],
        "and nothing was started for the recovery"
    );
    assert_eq!(f.state.runtime().list()[0].in_flight, 2);
}

/// A model whose container is still coming up is not an eviction candidate.
///
/// It looks like the perfect victim — `in_flight == 0`, because the request
/// that claimed it is still inside `acquire` — and stopping it would *succeed*.
/// That is the bug: a start in progress has a waiter by construction, so
/// evicting it kills an already-admitted request, and it does so in the window
/// where the victim is charged to the ledger in full but has not put a byte on
/// the card yet. Waiting is the correct move, and the queue timeout is what
/// makes the wait visible instead of infinite.
#[tokio::test]
async fn a_starting_model_is_never_evicted_out_from_under_its_own_start() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let (open, gate) = tokio::sync::watch::channel(false);
    *f.podman.gate.lock().unwrap() = Some(gate);

    let base = f.gateway.clone();
    let chatting = tokio::spawn(async move { chat(&base).await.status() });

    // Park the chat model in `starting`: podman run has been issued and the
    // entry is in the map, with no in-flight claim on it yet.
    for _ in 0..3_000 {
        let live = f.state.runtime().list();
        if live.iter().any(|v| v.state.as_str() == "starting") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let live = f.state.runtime().list();
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(live[0].state.as_str(), "starting");
    assert_eq!(live[0].in_flight, 0, "…with nothing claiming it, yet");

    // 6 GiB committed to the start + 3.5 GiB wanted does not fit in 8 GiB, so
    // this request goes looking for a victim and must find none.
    assert_eq!(
        embed(&f.gateway).await.status(),
        503,
        "the newcomer waits for the start it cannot evict, then says so"
    );
    assert!(
        f.stops().is_empty(),
        "the start was evicted out from under itself: {:?}",
        f.stops()
    );

    open.send_replace(true);
    assert_eq!(
        chatting.await.unwrap(),
        200,
        "and the request that was starting it still gets its answer"
    );
    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
}
