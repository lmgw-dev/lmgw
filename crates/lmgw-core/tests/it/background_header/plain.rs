//! A plain local model with `x-lmgw-background: on` (per-request background
//! design §4), and the header itself (§3).

use lmgw_core::runtime::registry::Origin;
use serde_json::{json, Value};

use super::*;
use crate::support::gpu_world::GIB;

/// Loaded: joined as a guest. The owner's model answers and stays the
/// owner's; nothing is started.
#[tokio::test]
async fn a_loaded_model_is_joined_and_stays_the_owners() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("m", 8 * GIB).await;
    owner_loads(&g, "m").await;

    answered_locally(guest_chat(&gw, "m").await, "m").await;
    assert_eq!(g.runs(), ["m"], "nothing started for the guest");
    assert_eq!(view(&g, "m").unwrap().owner, Origin::Owner);
}

/// Not loaded, and it fits the free VRAM: started for the guest, next to
/// the owner's model and without touching it, and the guest's until a
/// foreground request claims it.
#[tokio::test]
async fn a_model_that_fits_free_vram_is_started_as_the_guests() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("x", 8 * GIB).await;
    g.model("m", 8 * GIB).await;
    owner_loads(&g, "x").await;

    answered_locally(guest_chat(&gw, "m").await, "m").await;
    assert_eq!(g.runs(), ["x", "m"]);
    assert!(g.stops().is_empty());
    assert_eq!(view(&g, "m").unwrap().owner, Origin::Background);

    // `off` is an ordinary request: it claims the model for the owner.
    answered_locally(chat(&gw, "m", Some("off")).await, "m").await;
    assert_eq!(view(&g, "m").unwrap().owner, Origin::Owner);
}

/// Not loaded, and room only by evicting the owner's idle model: `503
/// gpu_deferred` naming what is in the way, and nothing evicted or started.
/// The same request without the header evicts as it always has.
#[tokio::test]
async fn no_room_without_evicting_the_owner_defers_and_evicts_nothing() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 16 * GIB).await;
    g.model("m", 12 * GIB).await;
    owner_loads(&g, "x").await;

    let msg = deferred(&g, guest_chat(&gw, "m").await, "m", "model").await;
    assert!(msg.contains("GPU in use by chat/x"), "{msg}");
    assert!(g.stops().is_empty(), "the owner's model was not evicted");
    assert_eq!(g.runs(), ["x"], "nothing started");

    answered_locally(chat(&gw, "m", None).await, "m").await;
    assert_eq!(g.stops(), ["x"], "an ordinary request evicts, as ever");
}

/// A deferral with a fallback on the row: the fallback answers, reason
/// `background`, and the body still names the model asked for.
#[tokio::test]
async fn the_rows_fallback_answers_instead_of_a_deferral() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 16 * GIB).await;
    g.cloud("cloud", None).await;
    g.row(with_fallback("m", "cloud"), 12 * GIB).await;
    owner_loads(&g, "x").await;

    let resp = guest_chat(&gw, "m").await;
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud"));
    assert_eq!(header(&resp, "x-lmgw-fallback-reason"), Some("background"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "from the cloud");
    assert_eq!(v["model"], "m", "the body echoes the requested name");
    let row = newest_log(&g, "m").await;
    assert_eq!(
        row.fallback_reason.as_deref(),
        Some("background"),
        "{row:?}"
    );
    assert!(g.stops().is_empty());
    assert_eq!(g.runs(), ["x"]);
}

/// While a foreground request waits for room every resident drains — on an
/// install without any background alias — so a guest is not joined to the
/// owner's busy model and starts nothing: it defers, naming the owner. The
/// owner's wait then ends as it would have.
#[tokio::test]
async fn a_guest_defers_while_the_owner_waits_for_room() {
    let (g, gw) = world_with_queue(24 * GIB, 4, 10).await;
    g.model("a", 16 * GIB).await;
    g.model("b", 16 * GIB).await;
    let busy = vram::admit(&g.state, &g.route("a"), "a")
        .await
        .unwrap()
        .expect("a is local");
    let waiting = tokio::spawn({
        let (state, route) = (g.state.clone(), g.route("b"));
        async move { vram::admit(&state, &route, "b").await }
    });
    let runtime = g.state.runtime();
    until("the owner's wait drains the card", || {
        runtime.draining_for_owner()
    })
    .await;

    let msg = deferred(&g, guest_chat(&gw, "a").await, "a", "model").await;
    assert!(msg.contains("the owner"), "{msg}");

    drop(busy);
    let b = waiting.await.unwrap().unwrap().expect("b is local");
    assert_eq!(g.stops(), ["a"]);
    drop(b);
}

/// Under the GPU hold the flag changes nothing: the row's fallback answers
/// with reason `hold`, and a row without one gets the hold's own `503
/// gpu_hold` — not a deferral.
#[tokio::test]
async fn under_the_hold_a_guest_gets_the_holds_answer() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("m", 8 * GIB).await;
    g.cloud("cloud", None).await;
    g.row(with_fallback("f", "cloud"), 8 * GIB).await;
    hold_on(&g).await;

    let resp = guest_chat(&gw, "m").await;
    assert_eq!(resp.status(), 503);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "gpu_hold", "{v}");
    let row = newest_log(&g, "m").await;
    assert_eq!(row.error_kind.as_deref(), Some("gpu_hold"), "{row:?}");

    let resp = guest_chat(&gw, "f").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud"));
    assert_eq!(header(&resp, "x-lmgw-fallback-reason"), Some("hold"));
    assert!(g.runs().is_empty(), "nothing local starts under the hold");
}

/// A model no card could hold is the configuration error it is — `507
/// vram_too_large` — never a deferral and never the fallback.
#[tokio::test]
async fn a_model_no_card_holds_is_vram_too_large_not_a_deferral() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.cloud("cloud", None).await;
    g.row(with_fallback("m", "cloud"), 30 * GIB).await;

    let resp = guest_chat(&gw, "m").await;
    assert_eq!(resp.status(), 507);
    assert_eq!(header(&resp, "x-lmgw-fallback"), None);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "vram_too_large", "{v}");
    assert!(g.runs().is_empty());
}

/// A cloud model has no GPU to defer: the flag changes nothing.
#[tokio::test]
async fn a_cloud_model_is_unaffected() {
    let (g, gw) = world(24 * GIB, 1).await;
    g.cloud("cloud", None).await;

    let resp = guest_chat(&gw, "cloud").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-fallback"), None);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "from the cloud");
}

/// Any value but `on` and `off` is a `400` naming the header, in the route's
/// own shape — OpenAI's, Anthropic's, llama.cpp's — before anything starts.
/// A route that admits no model ignores it.
#[tokio::test]
async fn a_value_that_is_not_on_or_off_is_a_400_in_the_routes_shape() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("m", 8 * GIB).await;
    let named = "x-lmgw-background: expected 'on' or 'off', got 'yes'";

    let resp = chat(&gw, "m", Some("yes")).await;
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "bad_request", "{v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains(named),
        "{v}"
    );

    let resp = gw
        .client()
        .post(format!("{gw}/v1/messages"))
        .header(BACKGROUND, "yes")
        .json(&json!({"model": "m", "max_tokens": 16,
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["error"]["type"], "invalid_request_error", "{v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains(named),
        "{v}"
    );

    let resp = gw
        .client()
        .post(format!("{gw}/tokenize"))
        .header(BACKGROUND, "yes")
        .json(&json!({"model": "m", "content": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["lmgw_code"], "bad_request", "{v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains(named),
        "{v}"
    );

    let resp = gw
        .client()
        .get(format!("{gw}/v1/models"))
        .header(BACKGROUND, "yes")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "a route that admits nothing ignores it");
    assert!(g.runs().is_empty(), "refused before anything started");
}

/// A guest on the owner's ladder whose prompt needs a higher rung may not
/// climb it (candidate-aliases §9): no byte reached the client (the ladder's
/// send is held until its count's verdict), and with no alternates and no
/// fallback the request defers. The rung stays as it was and nothing
/// restarts.
#[tokio::test]
async fn a_guest_may_not_climb_the_owners_ladder_and_defers() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.ladder("lad", 4 * GIB, &[(8 * GIB, 512)]).await;
    owner_loads(&g, "lad").await;
    // 100 prompt tokens + 16 of output do not fit the base rung's 64.
    g.world().prompt_tokens = 100;

    let msg = deferred(&g, guest_chat(&gw, "lad").await, "lad", "model").await;
    assert!(msg.contains("owner's model"), "{msg}");
    assert_eq!(view(&g, "lad").unwrap().rung.map(|r| r.rung), Some(1));
    assert_eq!(g.runs(), ["lad"], "no climb started");
    assert!(g.stops().is_empty());
}

/// A prompt above the model's top rung is the plain `400` for a guest too:
/// there are no alternates to pick instead (§4 step 7).
#[tokio::test]
async fn a_guest_above_the_top_rung_gets_the_plain_400() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.ladder("lad", 4 * GIB, &[(8 * GIB, 512)]).await;
    owner_loads(&g, "lad").await;
    g.world().prompt_tokens = 10_000;

    let resp = guest_chat(&gw, "lad").await;
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "context_length_exceeded", "{v}");
    assert_eq!(g.runs(), ["lad"]);
}

/// The row's fallback answers a guest whose model cannot take the request
/// once it was admitted, as it answers a deferral at admission (§11.8):
/// a dead container of the owner's model, which a guest never brings back
/// (`guest::lost`), and a climb the guest may not make.
#[tokio::test]
async fn a_lost_model_and_a_denied_climb_are_answered_by_the_rows_fallback() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.cloud("cloud", None).await;
    g.row(with_fallback("m", "cloud"), 8 * GIB).await;
    owner_loads(&g, "m").await;
    let port = view(&g, "m").unwrap().port;
    g.kill(port).await;

    let resp = guest_chat(&gw, "m").await;
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud"));
    assert_eq!(header(&resp, "x-lmgw-fallback-reason"), Some("background"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "from the cloud");
    assert_eq!(g.runs(), ["m"], "the owner's dead model was not restarted");

    g.ladder("lad", 4 * GIB, &[(8 * GIB, 512)]).await;
    sqlx::query(
        "UPDATE local_models SET fallback = 'cloud'
         WHERE model_id = 'lad'",
    )
    .execute(&g.state.db)
    .await
    .unwrap();
    g.state.reload_snapshot().await.unwrap();
    owner_loads(&g, "lad").await;
    g.world().prompt_tokens = 100;

    let resp = guest_chat(&gw, "lad").await;
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud"));
    assert_eq!(header(&resp, "x-lmgw-fallback-reason"), Some("background"));
    assert_eq!(view(&g, "lad").unwrap().rung.map(|r| r.rung), Some(1));
    assert_eq!(g.runs(), ["m", "lad"], "no climb started");
}

/// A model no card could hold is `vram_too_large` under contention too —
/// while the owner waits for room — never a deferral and never the fallback
/// (§4 step 5, §11.14): the card's last known total judges its size before
/// the wait could answer it as one. With VRAM admission off no estimate is
/// trusted, so its size is not judged: the guest goes to its fallback, as
/// before.
#[tokio::test]
async fn a_model_no_card_holds_is_vram_too_large_while_the_card_is_busy() {
    let (g, gw) = world_with_queue(24 * GIB, 4, 10).await;
    g.model("a", 16 * GIB).await;
    g.model("b", 16 * GIB).await;
    g.cloud("cloud", None).await;
    g.row(with_fallback("huge", "cloud"), 30 * GIB).await;
    let busy = vram::admit(&g.state, &g.route("a"), "a")
        .await
        .unwrap()
        .expect("a is local");
    let waiting = tokio::spawn({
        let (state, route) = (g.state.clone(), g.route("b"));
        async move { vram::admit(&state, &route, "b").await }
    });
    let runtime = g.state.runtime();
    until("the owner's wait drains the card", || {
        runtime.draining_for_owner()
    })
    .await;

    let too_large = |resp: reqwest::Response| async move {
        assert_eq!(resp.status(), 507, "{:?}", resp.headers());
        assert_eq!(header(&resp, "x-lmgw-fallback"), None);
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["error"]["code"], "vram_too_large", "{v}");
    };
    too_large(guest_chat(&gw, "huge").await).await;

    drop(busy);
    drop(waiting.await.unwrap().unwrap().expect("b is local"));
    let mut s = g.state.snapshot().settings.clone();
    s.vram.enabled = false;
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
    let resp = guest_chat(&gw, "huge").await;
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud"));
    assert_eq!(header(&resp, "x-lmgw-fallback-reason"), Some("background"));
    assert_eq!(g.runs(), ["a", "b"], "nothing started for the guest");
}

/// A card total the last reading saw that is smaller than the card's now
/// never refuses a start that reads the card itself (§11.14): with nothing
/// in its way the guest holds the gate, reads the grown card and starts a
/// model the cached total could not hold.
#[tokio::test]
async fn a_stale_smaller_card_total_does_not_refuse_a_start_that_reads() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("x", 8 * GIB).await;
    g.model("big", 30 * GIB).await;
    // A ledger read: 24 GiB is the card's last known total.
    owner_loads(&g, "x").await;
    g.card(48 * GIB);

    answered_locally(guest_chat(&gw, "big").await, "big").await;
    assert_eq!(g.runs(), ["x", "big"]);
    assert_eq!(view(&g, "big").unwrap().owner, Origin::Background);
}

/// The GPU hold switched on while the owner's plain request waits for room:
/// answered as the resolve-time swap answers it (§11.9, *changed
/// 2026-10-22*) — the row's fallback with reason `hold`, not `gpu_hold`
/// while a fallback is configured.
#[tokio::test]
async fn the_hold_coming_on_while_the_owner_waits_answers_with_the_rows_fallback() {
    let (g, gw) = world_with_queue(24 * GIB, 3, 30).await;
    g.model("a", 16 * GIB).await;
    g.cloud("cloud", None).await;
    g.row(with_fallback("m", "cloud"), 16 * GIB).await;
    let busy = vram::admit(&g.state, &g.route("a"), "a")
        .await
        .unwrap()
        .expect("a is local");
    let waiting = tokio::spawn({
        let gw = gw.clone();
        async move { chat(&gw, "m", None).await }
    });
    let runtime = g.state.runtime();
    until("the owner's request waits for room", || {
        runtime.draining_for_owner()
    })
    .await;
    hold_on(&g).await;

    let resp = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("answered once the wait sees the hold, not at the queue timeout")
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud"));
    assert_eq!(header(&resp, "x-lmgw-fallback-reason"), Some("hold"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "from the cloud");
    assert_eq!(g.runs(), ["a"], "nothing started for it");
    drop(busy);
}

/// The GPU hold switched on while a guest waits for its model's start in
/// flight: answered as a plain request under the hold is (§11.9) — the
/// row's fallback with reason `hold` — not with `gpu_hold` while a fallback
/// is configured.
#[tokio::test]
async fn the_hold_coming_on_mid_start_answers_with_the_rows_fallback() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.cloud("cloud", None).await;
    g.row(with_fallback("m", "cloud"), 8 * GIB).await;
    let open = g.gate_runs();
    let direct = tokio::spawn({
        let (state, route) = (g.state.clone(), g.route("m"));
        async move { vram::admit(&state, &route, "m").await.map(|h| h.is_some()) }
    });
    until("the owner's start is in flight", || {
        view(&g, "m")
            .is_some_and(|v| v.state == lmgw_core::runtime::registry::RuntimeState::Starting)
    })
    .await;
    let guest = tokio::spawn({
        let gw = gw.clone();
        async move { guest_chat(&gw, "m").await }
    });
    // The guest parks on the start it joins.
    tokio::time::sleep(Duration::from_millis(300)).await;
    hold_on(&g).await;
    open.send(true).unwrap();

    let resp = tokio::time::timeout(Duration::from_secs(10), guest)
        .await
        .expect("the guest is answered")
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud"));
    assert_eq!(header(&resp, "x-lmgw-fallback-reason"), Some("hold"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "from the cloud");
    let _ = direct.await;
}
