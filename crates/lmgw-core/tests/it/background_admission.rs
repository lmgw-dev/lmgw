//! Background traffic's admission half (candidate-aliases design §4.1–§4.5,
//! §9; §12 entries 45–48): the join that never starts a model, the guest's
//! start that never disturbs the owner, draining for the owner, the restart
//! rules of a candidate's hold, and the guest's climb.
//!
//! Driven through `lmgw_core::vram` directly — the calls the request gate
//! makes — on `support/gpu_world.rs`: a fake driver whose free memory
//! follows what the fake podman loaded, so an eviction really makes room and
//! a refusal really means the card is full. The walk over a candidate list
//! is the request gate's, and is tested with it.

use std::time::Duration;

use lmgw_core::error::GatewayError;
use lmgw_core::runtime::registry::{Origin, RuntimeView};
use lmgw_core::vram::{self, BackgroundStart, Climbed, LocalHold, Restart};
use serde_json::json;

use crate::support::gpu_world;
use gpu_world::{Gpu, GIB};

fn view(g: &Gpu, model: &str) -> Option<RuntimeView> {
    g.state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == model)
}

async fn owner_hold(g: &Gpu, model: &str) -> LocalHold {
    vram::admit(&g.state, &g.route(model), model)
        .await
        .unwrap()
        .expect("a local model")
}

async fn guest_start(g: &Gpu, model: &str, alias: &str) -> BackgroundStart {
    vram::start_background(&g.state, &g.route(model), alias)
        .await
        .unwrap()
}

fn started(s: BackgroundStart) -> LocalHold {
    match s {
        BackgroundStart::Started(hold) => *hold,
        BackgroundStart::Blocked(why) => panic!("expected a start, blocked by: {why}"),
    }
}

fn blocked(s: BackgroundStart) -> String {
    match s {
        BackgroundStart::Blocked(why) => why,
        BackgroundStart::Started(h) => panic!("expected a refusal, got a hold on {}", h.model_id()),
    }
}

async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// One chat send through the hold's dead-container policy
/// (`vram::send_local`).
async fn send(g: &Gpu, hold: &LocalHold, model: &str) -> Result<reqwest::Response, GatewayError> {
    let client = g.state.http.clone();
    vram::send_local(Some(hold), &g.route(model), None, |r| {
        Ok(client
            .post(format!("{}/chat/completions", r.upstream.base()))
            .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})))
    })
    .await
}

// ---------------------------------------------------------------------------
// The join (entry 46)
// ---------------------------------------------------------------------------

/// A candidate is claimed only while loaded, and an absent one is never
/// started by the claim — whoever asks. The hold remembers whose claim it is
/// and how it may come back.
#[tokio::test]
async fn a_candidate_is_joined_only_when_loaded_and_never_started() {
    let g = Gpu::new(24 * GIB, 2, 2).await;
    g.model("p", 8 * GIB).await;
    let route = g.route("p");

    for (origin, restart) in [
        (Origin::Background, Restart::No),
        (Origin::Owner, Restart::Admit),
    ] {
        let none = vram::join(&g.state, &route, "jobs", origin, restart)
            .await
            .unwrap();
        assert!(none.is_none(), "not loaded, so not joined");
    }
    assert!(g.runs().is_empty(), "a join starts nothing");

    let mine = owner_hold(&g, "p").await;
    assert_eq!(
        (mine.origin(), mine.restart()),
        (Origin::Owner, Restart::Admit)
    );
    let guest = vram::join(&g.state, &route, "jobs", Origin::Background, Restart::No)
        .await
        .unwrap()
        .expect("a loaded model is joined");
    assert_eq!(
        (guest.origin(), guest.restart()),
        (Origin::Background, Restart::No)
    );
    assert_eq!(guest.alias(), "jobs");
    assert_eq!(guest.port(), mine.port());
    assert_eq!(view(&g, "p").unwrap().owner, Origin::Owner);
    assert_eq!(g.runs(), vec!["p"]);
}

// ---------------------------------------------------------------------------
// The guest's start (§4.3 step 2, §4.4)
// ---------------------------------------------------------------------------

/// A primary that fits the free VRAM is started for the guest, and the
/// container is the guest's until the owner claims it.
#[tokio::test]
async fn a_background_start_into_free_vram_is_the_guests() {
    let g = Gpu::new(24 * GIB, 2, 2).await;
    g.model("p", 8 * GIB).await;

    let hold = started(guest_start(&g, "p", "jobs").await);
    assert_eq!(
        (hold.origin(), hold.restart()),
        (Origin::Background, Restart::Background)
    );
    assert_eq!(view(&g, "p").unwrap().owner, Origin::Background);
    let frame = serde_json::to_value(view(&g, "p").unwrap()).unwrap();
    assert_eq!(frame["owner"], "background");

    // Already up: the next guest joins it, and the owner's claim makes it theirs.
    let again = started(guest_start(&g, "p", "jobs").await);
    assert_eq!(again.port(), hold.port());
    let mine = owner_hold(&g, "p").await;
    assert_eq!(view(&g, "p").unwrap().owner, Origin::Owner);
    drop((hold, again, mine));
    assert_eq!(g.runs(), vec!["p"]);
}

/// §7 item 8: to make room, a guest evicts an idle guest — and never an idle
/// model of the owner's, even when that one would have made room alone.
#[tokio::test]
async fn a_background_start_evicts_an_idle_guest_but_never_an_idle_owner_model() {
    let g = Gpu::new(24 * GIB, 4, 2).await;
    g.model("mine", 12 * GIB).await;
    g.model("other", 8 * GIB).await;
    g.model("p", 8 * GIB).await;

    drop(owner_hold(&g, "mine").await);
    drop(started(guest_start(&g, "other", "batch").await));
    // 20 of 24 GiB taken: p needs one of the two to go.
    let hold = started(guest_start(&g, "p", "jobs").await);
    assert_eq!(g.stops(), vec!["other"], "the idle guest, and only it");
    assert!(view(&g, "mine").is_some(), "the owner's model stays");
    assert_eq!(view(&g, "p").unwrap().owner, Origin::Background);
    drop(hold);
}

/// When only the owner's models are in the way, the guest is blocked —
/// naming them, in words that read after "GPU in use by " — and nothing is
/// stopped or started.
#[tokio::test]
async fn a_background_start_blocked_by_the_owner_names_his_model_and_stops_nothing() {
    let g = Gpu::new(20 * GIB, 3, 2).await;
    g.model("mine", 12 * GIB).await;
    g.model("p", 10 * GIB).await;
    drop(owner_hold(&g, "mine").await);

    let why = blocked(guest_start(&g, "p", "jobs").await);
    assert!(why.starts_with("chat/mine ("), "{why}");
    assert!(why.contains("chat/p needs 10.0 GiB"), "{why}");
    assert!(g.stops().is_empty());
    assert_eq!(g.runs(), vec!["mine"]);
}

/// A card lmgw cannot measure is not free VRAM to a guest: blocked, saying
/// how to fix it. A model that is loaded anyway is still used.
#[tokio::test]
async fn without_measurement_a_guest_only_uses_what_is_loaded() {
    let g = Gpu::new(24 * GIB, 2, 2).await;
    g.model("p", 8 * GIB).await;
    let mut s = g.state.snapshot().settings.clone();
    s.vram.enabled = false;
    lmgw_core::store::save_settings(&g.state.db, &s)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();

    let why = blocked(guest_start(&g, "p", "jobs").await);
    assert!(why.contains("vram.enabled"), "{why}");
    assert!(g.runs().is_empty());

    let mine = owner_hold(&g, "p").await;
    let guest = started(guest_start(&g, "p", "jobs").await);
    assert_eq!(guest.port(), mine.port(), "joined, not started");
    assert_eq!(g.runs(), vec!["p"]);
}

/// A model larger than the card is a configuration error, never a deferral
/// (§4.7's rule).
#[tokio::test]
async fn a_primary_larger_than_the_card_is_vram_too_large() {
    let g = Gpu::new(8 * GIB, 1, 2).await;
    g.model("p", 12 * GIB).await;
    match vram::start_background(&g.state, &g.route("p"), "jobs").await {
        Err(GatewayError::VramTooLarge { .. }) => {}
        other => panic!("expected vram_too_large, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Draining for the owner (§4.5, entry 47)
// ---------------------------------------------------------------------------

/// §7 item 9: an owner request waits for room a busy model holds. While it
/// waits, every resident model drains for the owner — a guest cannot join one, and
/// a guest's start is blocked by the admission in progress (a try-lock,
/// never a wait) — while the owner's own claims go through. The in-flight work
/// finishes, the owner's model starts, and the mark is gone.
#[tokio::test]
async fn an_owner_waiting_for_room_drains_every_resident_and_blocks_guests() {
    let g = Gpu::new(24 * GIB, 4, 10).await;
    g.model("a", 16 * GIB).await;
    g.model("b", 16 * GIB).await;
    g.model("p", 2 * GIB).await;
    g.background_alias("jobs", &["p"]).await;

    let busy = owner_hold(&g, "a").await;
    let waiting = tokio::spawn({
        let (state, route) = (g.state.clone(), g.route("b"));
        async move { vram::admit(&state, &route, "b").await }
    });
    let runtime = g.state.runtime();
    until("the owner's wait drains the card", || {
        runtime.draining_for_owner()
    })
    .await;
    assert!(view(&g, "a").unwrap().draining_for_owner);

    let why = blocked(guest_start(&g, "p", "jobs").await);
    assert!(
        why.contains("the owner") || why.contains("admission gate"),
        "{why}"
    );
    let route_a = g.route("a");
    let refused = vram::join(&g.state, &route_a, "jobs", Origin::Background, Restart::No)
        .await
        .unwrap();
    assert!(
        refused.is_none(),
        "a guest takes no new work on a draining model"
    );
    let mine = vram::join(&g.state, &route_a, "a", Origin::Owner, Restart::Admit)
        .await
        .unwrap();
    assert!(mine.is_some(), "the owner is never refused");
    drop(mine);

    drop(busy);
    let b = waiting.await.unwrap().unwrap().expect("b is local");
    assert!(!g.state.runtime().draining_for_owner());
    assert_eq!(g.stops(), vec!["a"]);
    assert!(!view(&g, "b").unwrap().draining_for_owner);
    drop(b);
    assert!(
        g.runs().iter().all(|m| m != "p"),
        "the guest started nothing"
    );
}

/// The mark clears however the owner's wait ends: a timeout, and a waiting
/// request whose client went away.
#[tokio::test]
async fn draining_clears_on_a_timeout_and_on_a_dropped_wait() {
    let g = Gpu::new(24 * GIB, 4, 1).await;
    g.model("a", 16 * GIB).await;
    g.model("b", 16 * GIB).await;
    g.background_alias("jobs", &["a"]).await;
    let busy = owner_hold(&g, "a").await;

    match vram::admit(&g.state, &g.route("b"), "b").await {
        Err(GatewayError::VramQueueTimeout { .. }) => {}
        other => panic!("expected the queue timeout, got {other:?}"),
    }
    assert!(
        !g.state.runtime().draining_for_owner(),
        "cleared on timeout"
    );

    let mut s = g.state.snapshot().settings.clone();
    s.vram.queue_timeout_seconds = 30;
    lmgw_core::store::save_settings(&g.state.db, &s)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    let waiting = tokio::spawn({
        let (state, route) = (g.state.clone(), g.route("b"));
        async move { vram::admit(&state, &route, "b").await }
    });
    let runtime = g.state.runtime();
    until("the owner waits again", || runtime.draining_for_owner()).await;
    waiting.abort();
    let _ = waiting.await;
    assert!(
        !g.state.runtime().draining_for_owner(),
        "cleared when the waiting request is dropped"
    );
    drop(busy);
}

/// On an install without a background alias nothing drains: there is no
/// guest to drain for, and the frame stays what it was.
#[tokio::test]
async fn without_a_background_alias_an_owner_wait_drains_nothing() {
    let g = Gpu::new(24 * GIB, 4, 1).await;
    g.model("a", 16 * GIB).await;
    g.model("b", 16 * GIB).await;
    let busy = owner_hold(&g, "a").await;
    let waiting = tokio::spawn({
        let (state, route) = (g.state.clone(), g.route("b"));
        async move { vram::admit(&state, &route, "b").await }
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!g.state.runtime().draining_for_owner());
    let frame = serde_json::to_value(view(&g, "a").unwrap()).unwrap();
    assert!(frame.get("draining_for_owner").is_none(), "{frame}");
    assert!(waiting.await.unwrap().is_err(), "it timed out, as ever");
    drop(busy);
}

// ---------------------------------------------------------------------------
// Restart rules (design decision)
// ---------------------------------------------------------------------------

/// An alternate the alias only joined is never restarted: its container —
/// another alias's guest start — dies under the request, and the answer is
/// `candidate_lost` for the gate to pick again — not a restart, and not a
/// wrapped 502.
#[tokio::test]
async fn a_joined_alternate_that_dies_is_candidate_lost_not_restarted() {
    let g = Gpu::new(24 * GIB, 3, 2).await;
    g.model("a", 8 * GIB).await;
    let theirs = started(guest_start(&g, "a", "other-jobs").await);
    let guest = vram::join(
        &g.state,
        &g.route("a"),
        "jobs",
        Origin::Background,
        Restart::No,
    )
    .await
    .unwrap()
    .expect("loaded");

    g.kill(guest.port()).await;
    match send(&g, &guest, "a").await {
        Err(GatewayError::CandidateLost { model, detail }) => {
            assert_eq!(model, "a");
            assert!(detail.contains("'jobs'"), "{detail}");
        }
        other => panic!("expected candidate_lost, got {other:?}"),
    }
    assert_eq!(g.runs(), vec!["a"], "nothing restarted it");
    assert_eq!(
        g.stops(),
        vec!["a"],
        "the guest's dead container was stopped"
    );
    drop(theirs);
}

/// A guest never stops the owner's model (§12 entry 90): its container stops
/// answering the guest's send, and the guest's request is `candidate_lost` —
/// nothing stopped, the owner's claim untouched. The owner's own send then
/// recovers it as it always has.
#[tokio::test]
async fn a_guest_never_stops_the_owners_model_when_it_stops_answering() {
    let g = Gpu::new(24 * GIB, 3, 2).await;
    g.model("a", 8 * GIB).await;
    let mine = owner_hold(&g, "a").await;
    let guest = vram::join(
        &g.state,
        &g.route("a"),
        "jobs",
        Origin::Background,
        Restart::Background,
    )
    .await
    .unwrap()
    .expect("loaded");

    g.kill(guest.port()).await;
    match send(&g, &guest, "a").await {
        Err(GatewayError::CandidateLost { model, detail }) => {
            assert_eq!(model, "a");
            assert!(detail.contains("owner's model"), "{detail}");
        }
        other => panic!("expected candidate_lost, got {other:?}"),
    }
    assert!(g.stops().is_empty(), "a guest stopped nothing");
    assert_eq!(g.runs(), vec!["a"], "and started nothing");
    assert_eq!(view(&g, "a").unwrap().owner, Origin::Owner);
    drop(guest);

    let resp = send(&g, &mine, "a").await.expect("the owner recovers it");
    assert_eq!(resp.status(), 200);
    assert_eq!(g.stops(), vec!["a"]);
    assert_eq!(g.runs(), vec!["a", "a"]);
}

/// A guest's primary that dies is restarted by the guest's rule: when it
/// fits it comes back and the send is retried there …
#[tokio::test]
async fn a_guests_dead_primary_restarts_when_it_fits_without_the_owner() {
    let g = Gpu::new(24 * GIB, 3, 2).await;
    g.model("p", 8 * GIB).await;
    let hold = started(guest_start(&g, "p", "jobs").await);
    g.kill(hold.port()).await;
    let resp = send(&g, &hold, "p").await.expect("retried on the restart");
    assert_eq!(resp.status(), 200);
    assert_eq!(g.runs(), vec!["p", "p"]);
    assert_eq!(view(&g, "p").unwrap().owner, Origin::Background);
}

/// … and when the room it left is gone (a game took the card meanwhile), it
/// is `candidate_lost`, saying what is in the way — never an eviction or a
/// wait on the owner's behalf.
#[tokio::test]
async fn a_guests_dead_primary_that_no_longer_fits_is_candidate_lost() {
    let g = Gpu::new(24 * GIB, 3, 2).await;
    g.model("p", 8 * GIB).await;
    let hold = started(guest_start(&g, "p", "jobs").await);
    g.world().outside = 20 * GIB;
    g.kill(hold.port()).await;
    match send(&g, &hold, "p").await {
        Err(GatewayError::CandidateLost { detail, .. }) => {
            assert!(detail.contains("applications outside lmgw"), "{detail}")
        }
        other => panic!("expected candidate_lost, got {other:?}"),
    }
    assert_eq!(g.runs(), vec!["p"]);
}

// ---------------------------------------------------------------------------
// The guest's climb (§9, §4.3 "Ladders", entry 45)
// ---------------------------------------------------------------------------

/// A three-rung ladder: base 2 GiB at `-c 64`, 4 GiB at 128, `top` GiB at 512.
async fn ladder(g: &Gpu, top: u64) {
    g.ladder("lad", 2 * GIB, &[(4 * GIB, 128), (top * GIB, 512)])
        .await;
}

async fn climb_to_top(g: &Gpu, hold: &LocalHold) -> Climbed {
    vram::climb(&g.state, hold, 2, "prompt 400 + 16 > 64")
        .await
        .unwrap()
}

fn denied(c: Climbed) -> String {
    match c {
        Climbed::Denied { why } => why,
        other => panic!("expected a denial, got {other:?}"),
    }
}

/// §7 item 10: a guest never climbs the owner's ladder — denied before
/// anything is marked, the running rung untouched.
#[tokio::test]
async fn a_guest_never_climbs_the_owners_ladder() {
    let g = Gpu::new(24 * GIB, 4, 2).await;
    ladder(&g, 8).await;
    g.background_alias("jobs", &["lad"]).await;
    let mine = owner_hold(&g, "lad").await;
    let guest = vram::join(
        &g.state,
        &g.route("lad"),
        "jobs",
        Origin::Background,
        Restart::Background,
    )
    .await
    .unwrap()
    .expect("loaded");

    let why = denied(climb_to_top(&g, &guest).await);
    assert!(why.contains("owner's model"), "{why}");
    let v = view(&g, "lad").unwrap();
    assert_eq!(v.rung.as_ref().map(|r| r.rung), Some(1));
    assert!(v.climbing.is_none(), "nothing was marked");
    assert_eq!(g.runs(), vec!["lad"]);
    drop(mine);
}

/// A guest climbs its own primary into free VRAM, and the entry stays the
/// guest's across the climb.
#[tokio::test]
async fn a_guest_climbs_its_own_primary_into_free_vram() {
    let g = Gpu::new(24 * GIB, 4, 2).await;
    ladder(&g, 8).await;
    g.background_alias("jobs", &["lad"]).await;
    let hold = started(guest_start(&g, "lad", "jobs").await);

    match climb_to_top(&g, &hold).await {
        Climbed::Done => {}
        other => panic!("expected the climb, got {other:?}"),
    }
    let v = view(&g, "lad").unwrap();
    assert_eq!(v.rung.as_ref().map(|r| r.rung), Some(3));
    assert_eq!(v.owner, Origin::Background);
}

/// A rung that does not fit the free VRAM is denied — the owner's idle model
/// that would have made room is not evicted, and nothing is waited for.
#[tokio::test]
async fn a_guests_climb_that_needs_the_owners_room_is_denied_without_eviction() {
    let g = Gpu::new(12 * GIB, 4, 2).await;
    g.model("mine", 4 * GIB).await;
    ladder(&g, 9).await;
    g.background_alias("jobs", &["lad"]).await;
    drop(owner_hold(&g, "mine").await);
    let hold = started(guest_start(&g, "lad", "jobs").await);

    let why = denied(climb_to_top(&g, &hold).await);
    assert!(why.contains("free VRAM"), "{why}");
    assert!(g.stops().is_empty(), "the owner's idle model stays");
    assert_eq!(
        view(&g, "lad").unwrap().rung.as_ref().map(|r| r.rung),
        Some(1)
    );
}

/// §9 "only the primary": a guest-owned ladder that is another background
/// alias's primary is not this alias's to reload when it uses it as an
/// alternate.
#[tokio::test]
async fn a_guest_never_climbs_an_alternate() {
    let g = Gpu::new(24 * GIB, 4, 2).await;
    ladder(&g, 8).await;
    g.model("p", 4 * GIB).await;
    g.background_alias("lads", &["lad"]).await;
    g.background_alias("jobs", &["p", "lad"]).await;
    drop(started(guest_start(&g, "lad", "lads").await));
    let alt = vram::join(
        &g.state,
        &g.route("lad"),
        "jobs",
        Origin::Background,
        Restart::No,
    )
    .await
    .unwrap()
    .expect("loaded");

    let why = denied(climb_to_top(&g, &alt).await);
    assert!(why.contains("alternate of 'jobs'"), "{why}");
    assert_eq!(
        view(&g, "lad").unwrap().rung.as_ref().map(|r| r.rung),
        Some(1)
    );
}
