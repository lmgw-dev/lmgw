//! Ladder climbs: the replacement admission, driven directly (ladder design
//! §3.4, §5, §7 item 12; §12 entries 8–10 and 19–30). The request path's §7
//! cases (the count beside the send, the headers, the log column) are WP4's.

use super::*;

pub(super) const LADDER: &str = "ladder-model";

/// [`fixture_n`] plus a three-rung ladder row (§4.1): one slot, a max output
/// of 16, the base at `-c 64` on a 2 GiB file, rung 2 at 128 on 4 GiB, rung 3
/// at 512 on 8 GiB. Sparse files like the fixture's own, so each rung's
/// footprint is exactly its file — and the fake driver follows whichever file
/// a `run` loaded. Seven containers: every climb and every restart takes a
/// port.
pub(super) async fn ladder_fixture(total_vram: u64, idle_seconds: i64) -> Fixture {
    let f = fixture_n(total_vram, 6 * GIB, 3 * GIB, 512, 4).await;
    let dir = f.state.snapshot().settings.router.models_dir.clone();
    {
        let mut w = f.world();
        for (file, bytes) in [
            ("ladder-base.gguf", 2 * GIB),
            ("ladder-mid.gguf", 4 * GIB),
            ("ladder-top.gguf", 8 * GIB),
        ] {
            std::fs::File::create(std::path::Path::new(&dir).join(file))
                .unwrap()
                .set_len(bytes)
                .unwrap();
            w.files.insert(file.into(), bytes);
        }
        w.size.insert(LADDER.into(), 2 * GIB);
        w.context_rules = true;
    }
    store::insert_local_model(
        &f.state.db,
        &NewLocalModel {
            model_id: LADDER.into(),
            gguf_path: "ladder-base.gguf".into(),
            params: lmgw_core::config::LlamaParams {
                ctx_size: Some(64),
                parallel: Some(1),
                n_predict: Some(16),
                ..Default::default()
            },
            args: vec![],
            idle_seconds,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![
                lmgw_core::ladder::Rung {
                    gguf_path: "ladder-mid.gguf".into(),
                    ctx_size: 128,
                },
                lmgw_core::ladder::Rung {
                    gguf_path: "ladder-top.gguf".into(),
                    ctx_size: 512,
                },
            ],
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    f
}

/// A request's admission of the ladder model through the gate — the way the
/// chat sites take theirs, so the hold carries the request's fallback policy.
pub(super) async fn ladder_hold(f: &Fixture) -> lmgw_core::vram::LocalHold {
    lmgw_core::gate::open(
        &f.state,
        LADDER,
        lmgw_core::gate::RouteCheck::Text("/v1/chat/completions"),
    )
    .await
    .unwrap()
    .hold
    .expect("the ladder model is local")
}

/// Climb to rung 3 (0-based 2) on `hold`'s behalf.
pub(super) async fn climb_to_top(
    f: &Fixture,
    hold: &lmgw_core::vram::LocalHold,
) -> Result<lmgw_core::vram::Climbed, lmgw_core::error::GatewayError> {
    lmgw_core::vram::climb(&f.state, hold, 2, "prompt 400 + 16 > 64").await
}

/// The ladder model's registry view.
pub(super) fn ladder_view(f: &Fixture) -> lmgw_core::runtime::registry::RuntimeView {
    f.state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == LADDER)
        .expect("the ladder model is resident")
}

/// The `-m` file of every start of the ladder model, in order.
pub(super) fn ladder_runs(f: &Fixture) -> Vec<String> {
    f.world()
        .run_files
        .iter()
        .filter(|(m, _, _)| m == LADDER)
        .map(|(_, file, _)| file.clone())
        .collect()
}

fn ladder_resident(v: &Value) -> Value {
    v["resident"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["model"] == LADDER)
        .cloned()
        .unwrap_or(Value::Null)
}

/// §7 item 12: the ledger charges the rung that runs, not the row's base —
/// the base's 2 GiB before the climb, rung 3's 8 GiB after it, which is also
/// what the (fake) driver sees once the old rung is gone.
#[tokio::test]
async fn the_ledger_charges_the_running_rung() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let hold = ladder_hold(&f).await;
    let before = ladder_view(&f);
    assert_eq!(before.rung.as_ref().map(|r| r.rung), Some(1));
    assert_eq!(
        ladder_resident(&vram_status(&f.gateway).await)["estimated_bytes"],
        2 * GIB
    );

    let climbed = climb_to_top(&f, &hold).await.expect("it fits");
    assert!(matches!(climbed, lmgw_core::vram::Climbed::Done));
    hold.sync().await.unwrap();

    let after = ladder_view(&f);
    assert_eq!(
        after.rung.as_ref().map(|r| (r.rung, r.of, r.gguf.as_str())),
        Some((3, 3, "ladder-top.gguf"))
    );
    assert_ne!(after.port, before.port);
    assert_eq!(hold.port(), after.port, "the trigger's claim followed");
    assert_eq!(hold.rung().map(|r| r.index), Some(2));
    let v = vram_status(&f.gateway).await;
    assert_eq!(ladder_resident(&v)["estimated_bytes"], 8 * GIB, "{v}");
    assert_eq!(v["devices"][0]["used_bytes"], 8 * GIB, "{v}");
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf", "ladder-top.gguf"]);
    assert_eq!(
        f.stops(),
        vec![LADDER.to_string()],
        "one stop, of the old rung"
    );
    assert_eq!(
        f.world().run_files.last().unwrap().2.as_deref(),
        Some("512"),
        "rendered at rung 3's context"
    );
}

/// The replacement admission counts the running rung as freed and makes the
/// rest of the room the usual way: lmgw's other idle model is evicted, LRU —
/// and the ladder model is never its own victim.
#[tokio::test]
async fn a_climb_evicts_an_idle_other_model_but_never_the_ladder_model() {
    let f = ladder_fixture(12 * GIB, 0).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    let hold = ladder_hold(&f).await;
    // 12 − 6 (chat) − 2 (the base) = 4 free; with the base counted as freed,
    // 6 — short of rung 3's 8.5 with headroom, so chat has to go.
    climb_to_top(&f, &hold).await.expect("room was made");
    assert_eq!(
        f.stops(),
        vec!["chat-model".to_string(), LADDER.to_string()],
        "chat evicted by the admission, then the old rung stopped by the climb"
    );
    hold.sync().await.unwrap();
    assert_eq!(ladder_view(&f).rung.unwrap().rung, 3);
}

/// Admission refuses the new rung: a busy model holds the room past the
/// queue timeout. The request gets the VRAM error naming the rung, and the
/// old rung is not stopped: it keeps serving, mark cleared.
#[tokio::test]
async fn a_refused_climb_leaves_the_running_rung_serving() {
    let f = ladder_fixture(12 * GIB, 0).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    set_slot_busy(&f, true);
    set_queue_timeout(&f, 1).await;
    let hold = ladder_hold(&f).await;
    let before = ladder_view(&f);

    let err = climb_to_top(&f, &hold).await.unwrap_err();
    let lmgw_core::error::GatewayError::VramQueueTimeout { model, holding, .. } = &err else {
        panic!("expected the VRAM queue timeout: {err}");
    };
    assert_eq!(model, "ladder-model rung 3/3 (ladder-top.gguf)");
    assert!(holding.contains("chat/chat-model"), "{holding}");

    let after = ladder_view(&f);
    assert!(after.climbing.is_none(), "the mark is cleared");
    assert_eq!(after.generation, before.generation, "the same container");
    assert_eq!(after.port, before.port);
    assert_eq!(after.rung.unwrap().rung, 1);
    assert!(f.stops().is_empty(), "{:?}", f.stops());
    let _send = hold.begin_send().expect("the running rung serves on");
}

/// The drain times out (§12 entry 10): a send still in flight on the running
/// rung past the climb's budget — or, once lmgw's own sends are done, a slot
/// still generating for a client on the container's port. A named 503 in the
/// queue timeout's code; the mark is cleared and the rung serves on.
#[tokio::test]
async fn a_drain_timeout_is_a_named_503_and_clears_the_mark() {
    let f = ladder_fixture(16 * GIB, 0).await;
    set_queue_timeout(&f, 1).await;
    let trigger = ladder_hold(&f).await;
    let other = ladder_hold(&f).await;
    let in_flight = other.begin_send().unwrap();
    assert_eq!(ladder_view(&f).sends, 1);

    let err = climb_to_top(&f, &trigger).await.unwrap_err();
    assert!(
        matches!(
            err,
            lmgw_core::error::GatewayError::LadderDrainTimeout { sends: 1, .. }
        ),
        "{err}"
    );
    assert_eq!(err.http_status(), 503);
    assert_eq!(err.kind(), "vram_queue_timeout");
    assert!(
        err.to_string().contains("rung 3/3 (ladder-top.gguf)"),
        "{err}"
    );
    assert!(ladder_view(&f).climbing.is_none());
    drop(in_flight);

    // lmgw's sends are done, but the container's own slots are not.
    f.world().busy.insert(LADDER.into());
    let err = climb_to_top(&f, &trigger).await.unwrap_err();
    assert!(
        matches!(
            err,
            lmgw_core::error::GatewayError::LadderDrainTimeout {
                sends: 0,
                slots: 1,
                ..
            }
        ),
        "{err}"
    );
    let view = ladder_view(&f);
    assert!(view.climbing.is_none());
    assert_eq!(view.rung.unwrap().rung, 1);
    assert!(f.stops().is_empty());
    let _send = trigger.begin_send().expect("serving on");
}

/// §12 entry 8: VRAM outside lmgw's control is short, and the request may
/// fall back — its fallback answers with `external_vram`, and the running rung
/// is not touched: no mark, no stop.
#[tokio::test]
async fn an_outside_shortfall_answers_a_climb_with_the_fallback_and_stops_nothing() {
    let f = ladder_fixture(12 * GIB, 0).await;
    f.attribute(6 * GIB);
    let _cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    let hold = ladder_hold(&f).await;
    assert!(hold.policy().is_some(), "a request that may fall back");
    let before = ladder_view(&f);

    // 12 − 6 outside − 2 (the base) = 4 free; lmgw's own 2 on top is 6, short
    // of rung 3's 8.5 even with every lmgw model gone.
    match climb_to_top(&f, &hold).await.expect("answered") {
        lmgw_core::vram::Climbed::Fallback {
            alias,
            route,
            reason,
        } => {
            assert_eq!(alias, "cloud-chat");
            assert_eq!(route.upstream.name, "cloud");
            assert_eq!(reason, lmgw_core::gate::FallbackReason::ExternalVram);
        }
        other => panic!("expected the fallback: {other:?}"),
    }
    let after = ladder_view(&f);
    assert_eq!(after.generation, before.generation);
    assert!(after.climbing.is_none());
    assert!(f.stops().is_empty());
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf"]);
}

/// §3.1 "GPU hold active": no climb. The request's fallback answers with
/// `hold`, or — without one — `gpu_hold`. Nothing is stopped or started.
#[tokio::test]
async fn under_the_hold_a_climb_falls_back_or_is_refused_and_starts_nothing() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let hold = ladder_hold(&f).await;
    engage_hold(&f).await;
    assert_eq!(ladder_view(&f).in_flight, 1, "the claim keeps it resident");

    let err = climb_to_top(&f, &hold).await.unwrap_err();
    assert_eq!(err.kind(), "gpu_hold", "{err}");

    let _cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    match climb_to_top(&f, &hold).await.expect("answered") {
        lmgw_core::vram::Climbed::Fallback { alias, reason, .. } => {
            assert_eq!(alias, "cloud-chat");
            assert_eq!(reason, lmgw_core::gate::FallbackReason::Hold);
        }
        other => panic!("expected the fallback: {other:?}"),
    }
    assert!(ladder_view(&f).climbing.is_none());
    assert!(f.stops().is_empty());
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf"]);
}

/// §12 entry 10: a tool loop's claim, idle while another request climbs the
/// model. The entry carries it: its next send syncs onto the new rung in
/// place — no acquire, no start — and the idle reaper never sees the model
/// idle under it.
#[tokio::test]
async fn an_idle_claim_follows_a_climb_without_an_acquire_and_is_never_reaped() {
    let f = ladder_fixture(16 * GIB, 1).await;
    let idle = ladder_hold(&f).await;
    let trigger = ladder_hold(&f).await;
    let old_port = idle.port();

    climb_to_top(&f, &trigger).await.unwrap();
    let new_port = ladder_view(&f).port;
    assert_ne!(new_port, old_port);
    assert_eq!(idle.port(), old_port, "stale until its next send");
    assert_eq!(ladder_view(&f).in_flight, 2, "both claims were carried");

    idle.sync().await.unwrap();
    assert_eq!(idle.port(), new_port);
    assert_eq!(idle.rung().map(|r| r.index), Some(2));
    assert_eq!(
        idle.gate_facts().unwrap().per_request_ctx(),
        Some(512),
        "judged on rung 3 from now on"
    );
    assert_eq!(ladder_runs(&f).len(), 2, "no acquire, no start");
    let _send = idle.begin_send().expect("and sends there");

    drop(trigger);
    // Idle for longer than the row's idle_seconds — but claimed.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    lmgw_core::runtime::lifecycle::reap_idle(&f.state).await;
    assert_eq!(ladder_view(&f).rung.unwrap().rung, 3, "not reaped");
    assert!(!f.stops().iter().skip(1).any(|m| m == LADDER));
}

/// §3.5 through eviction: a climbed model, idle, evicted for another model —
/// its next start is the base.
#[tokio::test]
async fn an_evicted_climbed_ladder_starts_again_at_its_base() {
    let f = ladder_fixture(12 * GIB, 0).await;
    let hold = ladder_hold(&f).await;
    climb_to_top(&f, &hold).await.unwrap();
    drop(hold);

    // 12 − 8 (rung 3) = 4 free; chat needs 6.5, so the idle ladder goes.
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert!(f
        .state
        .runtime()
        .list()
        .iter()
        .all(|v| v.model_id != LADDER));

    let _hold = ladder_hold(&f).await;
    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf", "ladder-top.gguf", "ladder-base.gguf"]
    );
    assert_eq!(ladder_view(&f).rung.unwrap().rung, 1);
}

/// §3.5 through the dead-container recovery: rung 3's container dies under a
/// claim; the recovery restarts the model — at the base, never at the rung
/// that died — and the send is retried there.
#[tokio::test]
async fn a_dead_climbed_container_recovers_at_the_base() {
    let mut f = ladder_fixture(16 * GIB, 0).await;
    let hold = ladder_hold(&f).await;
    climb_to_top(&f, &hold).await.unwrap();
    hold.sync().await.unwrap();
    let dead = hold.port();
    f.kill_container_on(dead).await;

    let route = f.state.snapshot().resolve(LADDER).unwrap();
    let resp = lmgw_core::vram::send_local(Some(&hold), &route, None, |r| {
        Ok(reqwest::Client::new()
            .post(format!("{}/chat/completions", r.upstream.base_url))
            .json(&json!({"model": LADDER,
                          "messages": [{"role": "user", "content": "hi"}]})))
    })
    .await
    .expect("recovered");
    assert_eq!(resp.status(), 200);
    assert_ne!(hold.port(), dead);
    assert_eq!(hold.rung().map(|r| r.index), Some(0), "back at the base");
    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf", "ladder-top.gguf", "ladder-base.gguf"]
    );
}

/// §12 entry 22: the new rung will not start. The trigger gets a 502 naming
/// the rung; a request that was waiting on the climb goes back through
/// admission and cold-starts the base; the trigger's stale claim does the
/// same on its next sync.
#[tokio::test]
async fn a_rung_that_will_not_start_fails_the_trigger_and_waiters_start_the_base() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let hold = ladder_hold(&f).await;
    f.world().fail_run_file = Some("ladder-top.gguf".into());
    let (open, gate) = tokio::sync::watch::channel(false);
    *f.podman.gate.lock().unwrap() = Some(gate);

    let climbing = tokio::spawn({
        let state = f.state.clone();
        async move {
            let climbed = lmgw_core::vram::climb(&state, &hold, 2, "why").await;
            (climbed, hold)
        }
    });
    for _ in 0..3_000 {
        if ladder_view(&f).state.as_str() == "starting" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(ladder_view(&f).state.as_str(), "starting");
    // A new request arrives mid-climb and parks on it.
    let waiting = tokio::spawn({
        let state = f.state.clone();
        async move {
            lmgw_core::gate::open(
                &state,
                LADDER,
                lmgw_core::gate::RouteCheck::Text("/v1/chat/completions"),
            )
            .await
            .map(|o| o.hold.map(|h| h.rung()))
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished());
    open.send_replace(true);

    let (climbed, hold) = climbing.await.unwrap();
    let err = climbed.unwrap_err();
    assert_eq!(err.http_status(), 502);
    let msg = err.to_string();
    assert!(
        msg.contains("climbing 'ladder-model' to rung 3/3 (ladder-top.gguf) failed"),
        "{msg}"
    );
    assert!(msg.contains("no such file"), "{msg}");

    let waited = waiting.await.unwrap().expect("the waiter was admitted");
    assert_eq!(
        waited.flatten().map(|r| r.index),
        Some(0),
        "it started the base"
    );
    hold.sync().await.unwrap();
    assert_eq!(hold.rung().map(|r| r.index), Some(0));
    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf", "ladder-top.gguf", "ladder-base.gguf"],
        "one base start for everyone"
    );
}
