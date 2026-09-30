//! Contention on the GPU between requests that arrive together
//! (candidate-aliases design §4.3–§4.5; §12 entries 91–93): a background
//! job's parallel requests on one primary, a guest's climb that others ride
//! or that fails, and owner climbs that need each other's memory.
//!
//! On `support/gpu_world.rs`: a fake driver whose free memory follows what
//! the fake podman loaded, containers that can be held mid-start or
//! mid-stop, and cloud fallbacks on wiremock. Requests go over HTTP through
//! the real router where the answer is the point, and through
//! `lmgw_core::vram` directly where the timing between two climbs is.

use std::time::Duration;

use lmgw_core::config::HoldFallbackMode;
use lmgw_core::runtime::descriptor::RungPos;
use lmgw_core::runtime::registry::{Marked, Origin, RuntimeState, RuntimeView};
use lmgw_core::runtime::Class;
use lmgw_core::store::NewCandidateAlias;
use lmgw_core::vram::{self, BackgroundStart, Climbed, LocalHold, Restart};
use serde_json::{json, Value};

use crate::common;
use common::{serve, Gw};

use crate::support::gpu_world;
use gpu_world::{Gpu, GIB};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A candidate alias with no facets enabled: `fallback` `None` is mode `none`.
fn alias(
    name: &str,
    candidates: &[&str],
    background: bool,
    fallback: Option<&str>,
) -> NewCandidateAlias {
    NewCandidateAlias {
        alias: name.into(),
        candidates: candidates.iter().map(|c| c.to_string()).collect(),
        background,
        fallback_mode: if fallback.is_some() {
            HoldFallbackMode::Alias
        } else {
            HoldFallbackMode::None
        },
        fallback: fallback.map(str::to_string),
        capabilities_disabled: vec![],
        capabilities_enabled: vec![],
        enabled: true,
        notes: String::new(),
    }
}

fn view(g: &Gpu, model: &str) -> Option<RuntimeView> {
    g.state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == model)
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

fn started(s: BackgroundStart) -> LocalHold {
    match s {
        BackgroundStart::Started(hold) => hold,
        BackgroundStart::Blocked(why) => panic!("expected a start, blocked by: {why}"),
    }
}

async fn guest_start(g: &Gpu, model: &str, alias: &str) -> BackgroundStart {
    vram::start_background(&g.state, &g.route(model), alias)
        .await
        .unwrap()
}

async fn owner_hold(g: &Gpu, model: &str) -> LocalHold {
    vram::admit(&g.state, &g.route(model), model)
        .await
        .unwrap()
        .expect("a local model")
}

async fn chat(gw: &Gw, model: &str) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
}

/// A request sent now, answered in the background.
fn chat_later(gw: &Gw, model: &str) -> tokio::task::JoinHandle<reqwest::Response> {
    let (gw, model) = (gw.clone(), model.to_string());
    tokio::spawn(async move { chat(&gw, &model).await })
}

async fn answer(handle: tokio::task::JoinHandle<reqwest::Response>) -> reqwest::Response {
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("the request was answered")
        .unwrap()
}

fn header<'r>(resp: &'r reqwest::Response, name: &str) -> Option<&'r str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// A 200 from the local candidate `model`, no fallback.
async fn answered_by(resp: reqwest::Response, model: &str) {
    let (status, candidate, fallback) = (
        resp.status(),
        header(&resp, "x-lmgw-candidate").map(str::to_string),
        header(&resp, "x-lmgw-fallback").map(str::to_string),
    );
    let v: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{v}");
    assert_eq!(candidate.as_deref(), Some(model), "{v}");
    assert_eq!(fallback, None, "{v}");
    assert_eq!(
        v["choices"][0]["message"]["content"],
        format!("ok from {model}")
    );
}

// ---------------------------------------------------------------------------
// §12 entry 91: a background job's parallel requests
// ---------------------------------------------------------------------------

/// A background job fires several requests at once at a primary that is not
/// loaded. The first holds the admission gate while it evicts an idle guest
/// model to make room; the others used to fail the gate's try-lock on it and
/// go to the cloud while their own primary came up. They wait for that start
/// instead, and every one of them is answered by it — one start.
#[tokio::test]
async fn parallel_guests_on_a_cold_primary_ride_its_one_start() {
    let g = Gpu::new(24 * GIB, 3, 2).await;
    let gw = serve(g.state.clone()).await;
    g.model("old", 16 * GIB).await;
    g.model("p", 12 * GIB).await;
    g.cloud("cloud", None).await;
    g.candidate(alias("old-jobs", &["old"], true, None)).await;
    g.candidate(alias("jobs", &["p"], true, Some("cloud")))
        .await;
    // An idle guest model, which the first request must evict for `p`.
    drop(started(guest_start(&g, "old", "old-jobs").await));
    let open = g.gate_stops();

    let first = chat_later(&gw, "jobs");
    until("the first request evicts, holding the gate", || {
        view(&g, "old").is_some_and(|v| v.state == RuntimeState::Stopping)
    })
    .await;
    let rest: Vec<_> = (0..3).map(|_| chat_later(&gw, "jobs")).collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    open.send(true).unwrap();

    for handle in std::iter::once(first).chain(rest) {
        answered_by(answer(handle).await, "p").await;
    }
    assert_eq!(g.runs(), ["old", "p"], "one start of the primary");
    assert_eq!(g.stops(), ["old"]);
    assert_eq!(
        g.world().chats,
        ["p", "p", "p", "p"],
        "nothing went to the cloud"
    );
    assert_eq!(view(&g, "p").unwrap().owner, Origin::Background);
}

/// A three-rung ladder: base 2 GiB at `-c 64`, 4 GiB at 128, 8 GiB at 512.
async fn ladder(g: &Gpu, id: &str) {
    g.ladder(id, 2 * GIB, &[(4 * GIB, 128), (8 * GIB, 512)])
        .await;
}

async fn climb_to_top(state: lmgw_core::state::SharedState, hold: LocalHold) -> Climbed {
    let climbed = vram::climb(&state, &hold, 2, "prompt 400 + 16 > 64").await;
    drop(hold);
    climbed.unwrap()
}

/// Two requests of one background job on its own ladder both need a higher
/// rung. The second rides the first one's climb — one reload serves both —
/// instead of reading the first one's queue row as "the owner waits" and
/// being denied. And a guest climb draining is no reason for another
/// guest's start of a different model to be blocked.
#[tokio::test]
async fn a_guests_climb_is_ridden_by_the_next_guest_trigger_not_denied() {
    let g = Gpu::new(24 * GIB, 4, 2).await;
    ladder(&g, "lad").await;
    g.model("p", 4 * GIB).await;
    g.background_alias("jobs", &["lad"]).await;
    g.background_alias("pjobs", &["p"]).await;
    let first = started(guest_start(&g, "lad", "jobs").await);
    let second = vram::join(
        &g.state,
        &g.route("lad"),
        "jobs",
        Origin::Background,
        Restart::Background,
    )
    .await
    .unwrap()
    .expect("loaded");
    // The running rung is still generating: the first climb waits in its
    // drain until this clears.
    g.world().busy.insert("lad".into());

    let climbing = tokio::spawn(climb_to_top(g.state.clone(), first));
    until("the first guest's climb drains", || {
        view(&g, "lad").is_some_and(|v| v.climbing.is_some())
    })
    .await;

    let ridden = tokio::spawn(climb_to_top(g.state.clone(), second));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !ridden.is_finished(),
        "the second trigger waits on the climb, it is not denied"
    );
    drop(started(guest_start(&g, "p", "pjobs").await));
    g.world().busy.clear();

    for handle in [climbing, ridden] {
        match tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the climb ended")
            .unwrap()
        {
            Climbed::Done => {}
            other => panic!("expected the climb, got {other:?}"),
        }
    }
    assert_eq!(g.runs(), ["lad", "p", "lad"], "one reload served both");
    let v = view(&g, "lad").unwrap();
    assert_eq!(v.rung.as_ref().map(|r| r.rung), Some(3));
    assert_eq!(v.owner, Origin::Background);
}

/// The owner's climb queued for room is still the owner waiting: a guest's
/// climb is denied and a guest's start blocked while it is.
#[tokio::test]
async fn an_owner_climb_in_the_queue_still_denies_guest_climbs_and_starts() {
    let g = Gpu::new(24 * GIB, 4, 2).await;
    ladder(&g, "mine").await;
    ladder(&g, "lad").await;
    g.model("p", 4 * GIB).await;
    g.background_alias("jobs", &["lad"]).await;
    g.background_alias("pjobs", &["p"]).await;
    let owner_claim = owner_hold(&g, "mine").await;
    let guest = started(guest_start(&g, "lad", "jobs").await);
    g.world().busy.insert("mine".into());

    let owner_climb = tokio::spawn(climb_to_top(g.state.clone(), owner_claim));
    until("the owner's climb drains", || {
        view(&g, "mine").is_some_and(|v| v.climbing.is_some())
    })
    .await;

    match climb_to_top(g.state.clone(), guest).await {
        Climbed::Denied { why } => assert!(why.contains("chat/mine"), "{why}"),
        other => panic!("expected a denial, got {other:?}"),
    }
    match guest_start(&g, "p", "pjobs").await {
        BackgroundStart::Blocked(why) => assert!(why.contains("chat/mine"), "{why}"),
        BackgroundStart::Started(_) => panic!("a guest started while the owner's climb waits"),
    }

    g.world().busy.clear();
    assert!(matches!(owner_climb.await.unwrap(), Climbed::Done));
}

/// The owner's model — their claim flips it even while a guest's claim is on
/// it — is never marked climbing by a guest, and the owner's own climb is
/// never joined (or raised) by one: refused in the lock hold that marks.
#[tokio::test]
async fn a_guest_never_marks_or_joins_a_climb_of_the_owners_model() {
    let g = Gpu::new(24 * GIB, 4, 2).await;
    ladder(&g, "lad").await;
    g.background_alias("jobs", &["lad"]).await;
    drop(started(guest_start(&g, "lad", "jobs").await));
    let reg = g.state.runtime();
    let guest = reg
        .join(Class::Chat, "lad", Origin::Background)
        .await
        .unwrap()
        .expect("loaded");
    let owner = reg
        .join(Class::Chat, "lad", Origin::Owner)
        .await
        .unwrap()
        .expect("loaded");
    let top = RungPos { index: 2, of: 3 };

    match reg.mark_climb_for_guest(&guest, top, "why") {
        Marked::Refused(why) => assert!(why.contains("owner's model"), "{why}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(
        view(&g, "lad").unwrap().climbing.is_none(),
        "nothing marked"
    );

    let _ticket = match reg.mark_climb(&owner, RungPos { index: 1, of: 3 }, "owner") {
        Marked::Ticket(t) => t,
        other => panic!("expected the owner's ticket, got {other:?}"),
    };
    match reg.mark_climb_for_guest(&guest, top, "why") {
        Marked::Refused(_) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(
        view(&g, "lad").unwrap().climbing.map(|c| c.to),
        Some(2),
        "the owner's climb was not raised"
    );
}

// ---------------------------------------------------------------------------
// §12 entry 92: a guest's climb that fails
// ---------------------------------------------------------------------------

/// A guest's ladder `lad` — base 4 GiB at `-c 64`, one rung `top` — started
/// by a guest (so its own), and `a`, the owner's idle model. `jobs` = [lad,
/// a] and `jobs-lad` = [lad], both background, no fallback. A queue timeout
/// of `queue_seconds`.
async fn guest_ladder(top: (u64, i64), queue_seconds: u64) -> (Gpu, Gw) {
    let g = Gpu::new(24 * GIB, 4, queue_seconds).await;
    let gw = serve(g.state.clone()).await;
    g.ladder("lad", 4 * GIB, &[top]).await;
    g.model("a", 4 * GIB).await;
    g.candidate(alias("jobs", &["lad", "a"], true, None)).await;
    g.candidate(alias("jobs-lad", &["lad"], true, None)).await;
    drop(started(guest_start(&g, "lad", "jobs").await));
    drop(owner_hold(&g, "a").await);
    (g, gw)
}

/// The rung a guest's request needs is larger than the card (§4.3's check
/// is advisory at save time): the loaded alternate answers, as for a primary
/// no card could hold (entry 74). With nothing else to answer, that error
/// is the answer, not a deferral. The owner gets it exactly as before.
#[tokio::test]
async fn a_guests_climb_to_a_rung_no_card_holds_goes_on_to_the_alternate() {
    let (g, gw) = guest_ladder((30 * GIB, 4096), 2).await;
    g.candidate(alias("writer", &["lad", "a"], false, None))
        .await;
    // 100 prompt tokens + 16 of output do not fit the base rung's 64.
    g.world().prompt_tokens = 100;

    answered_by(chat(&gw, "jobs").await, "a").await;

    let resp = chat(&gw, "jobs-lad").await;
    assert_eq!(header(&resp, "x-lmgw-candidate"), Some("lad"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "vram_too_large", "{v}");

    let resp = chat(&gw, "writer").await;
    assert_eq!(header(&resp, "x-lmgw-candidate"), Some("lad"));
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "vram_too_large", "{v}");
    assert_eq!(g.runs(), ["lad", "a"], "no rung was started");
}

/// The new rung does not start: the guest's request goes on to the loaded
/// alternate instead of answering with the climb's 502.
#[tokio::test]
async fn a_guests_climb_whose_rung_fails_to_start_goes_on_to_the_alternate() {
    let (g, gw) = guest_ladder((8 * GIB, 4096), 2).await;
    g.world().prompt_tokens = 100;
    g.world().fail_run.insert("lad".into());

    answered_by(chat(&gw, "jobs").await, "a").await;
    assert_eq!(g.runs(), ["lad", "a", "lad"], "the rung was tried once");
    assert!(
        view(&g, "lad").is_none(),
        "the failed climb took the model down"
    );
}

/// The running rung never goes quiet within the queue timeout: the guest's
/// request goes on to the loaded alternate instead of answering with the
/// drain's 503. The ladder serves on at its base.
#[tokio::test]
async fn a_guests_climb_whose_drain_times_out_goes_on_to_the_alternate() {
    let (g, gw) = guest_ladder((8 * GIB, 4096), 1).await;
    g.world().prompt_tokens = 100;
    g.world().busy.insert("lad".into());

    answered_by(chat(&gw, "jobs").await, "a").await;
    let v = view(&g, "lad").unwrap();
    assert_eq!(v.rung.as_ref().map(|r| r.rung), Some(1));
    assert!(v.climbing.is_none());
    assert_eq!(g.runs(), ["lad", "a"]);
}

/// llama-server refuses the prompt again on the rung its first refusal
/// climbed to (the count undercounted twice): the guest's request goes on to
/// the loaded alternate instead of answering with that `400`.
#[tokio::test]
async fn a_guests_second_backstop_refusal_goes_on_to_the_alternate() {
    let (g, gw) = guest_ladder((8 * GIB, 8192), 2).await;
    // The count says the base rung fits; the container says 5000 tokens.
    g.world().prompt_tokens = 10;
    g.world().refuse_context.insert("lad".into());

    answered_by(chat(&gw, "jobs").await, "a").await;
    assert_eq!(
        g.runs(),
        ["lad", "a", "lad"],
        "one climb, for the first refusal"
    );
    assert_eq!(
        view(&g, "lad").unwrap().rung.as_ref().map(|r| r.rung),
        Some(2)
    );
}

// ---------------------------------------------------------------------------
// §12 entry 93: owner climbs that need each other's memory
// ---------------------------------------------------------------------------

/// Two of the owner's ladders are in use and both need their top rung, and
/// neither top rung fits next to the other ladder's base. Each climb waits
/// for the other's model, which stays busy until its own climb ends: the
/// gate used to go back and forth between them until the queue timeout —
/// never, at 0, the timeout here. Now one gives way at once with
/// `vram_queue_timeout` naming the other, its model is evicted once its
/// request is over, and the other climbs.
#[tokio::test]
async fn owner_climbs_that_need_each_others_memory_end_at_once_and_one_climbs() {
    let g = Gpu::new(24 * GIB, 6, 0).await;
    g.ladder("l1", 6 * GIB, &[(20 * GIB, 512)]).await;
    g.ladder("l2", 6 * GIB, &[(20 * GIB, 512)]).await;
    let (h1, h2) = (owner_hold(&g, "l1").await, owner_hold(&g, "l2").await);
    let climb = |hold: LocalHold| {
        let state = g.state.clone();
        tokio::spawn(async move {
            let climbed = vram::climb(&state, &hold, 1, "prompt 400 + 16 > 64").await;
            // The request ends, and its claim with it.
            drop(hold);
            climbed
        })
    };
    let (c1, c2) = (climb(h1), climb(h2));
    let ended = |c: tokio::task::JoinHandle<_>| async move {
        tokio::time::timeout(Duration::from_secs(10), c)
            .await
            .expect("the climbs did not wait on each other")
            .unwrap()
    };
    let (r1, r2) = (ended(c1).await, ended(c2).await);

    let (winner, loser, refused) = match (r1, r2) {
        (Ok(Climbed::Done), Err(e)) => ("l1", "l2", e),
        (Err(e), Ok(Climbed::Done)) => ("l2", "l1", e),
        other => panic!("expected one climb and one refusal, got {other:?}"),
    };
    assert_eq!(refused.kind(), "vram_queue_timeout", "{refused}");
    let msg = refused.to_string();
    assert!(msg.contains(&format!("the climb of '{winner}'")), "{msg}");
    assert_eq!(
        view(&g, winner).unwrap().rung.as_ref().map(|r| r.rung),
        Some(2)
    );
    assert!(view(&g, loser).is_none(), "evicted for the winner");
    assert!(g.stops().iter().any(|m| m == loser));
    assert!(!g.state.runtime().draining_for_owner());
}

/// Two owner climbs where one of them fits next to the other's running rung
/// are not a deadlock: nobody gives way, the one that fits climbs first, and
/// the other then evicts it once its request is over — both climb.
#[tokio::test]
async fn owner_climbs_that_can_go_one_after_the_other_both_climb() {
    let g = Gpu::new(24 * GIB, 6, 0).await;
    g.ladder("small", 6 * GIB, &[(16 * GIB, 512)]).await;
    g.ladder("big", 6 * GIB, &[(20 * GIB, 512)]).await;
    let holds = [owner_hold(&g, "small").await, owner_hold(&g, "big").await];
    let climbs: Vec<_> = holds
        .into_iter()
        .map(|hold| {
            let state = g.state.clone();
            tokio::spawn(async move {
                let climbed = vram::climb(&state, &hold, 1, "prompt 400 + 16 > 64").await;
                drop(hold);
                climbed
            })
        })
        .collect();
    for c in climbs {
        match tokio::time::timeout(Duration::from_secs(10), c)
            .await
            .expect("the climbs ended")
            .unwrap()
        {
            Ok(Climbed::Done) => {}
            other => panic!("expected the climb, got {other:?}"),
        }
    }
    assert_eq!(
        view(&g, "big").unwrap().rung.as_ref().map(|r| r.rung),
        Some(2)
    );
}
