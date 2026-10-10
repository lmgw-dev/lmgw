//! lmgw__local_model_test on a ladder row (ladder design §6, §8 WP6): every
//! rung tested in turn, on the fake podman. A row without a ladder keeps its
//! own single-probe test untouched — that path is exercised elsewhere in
//! `vram_admission` (e.g. `chat-model`, never a ladder), not duplicated here.

use super::*;

/// The happy path: base admitted and probed, then `vram::climb` to each
/// higher rung in turn, probed again — the same mechanics a real request's
/// climb uses, just driven directly instead of by a request that outgrew its
/// rung. Ends stopped, so the next real request starts clean at the base.
#[tokio::test]
async fn local_model_test_climbs_every_rung_then_resets_to_the_base() {
    let f = ladder_fixture(64 * GIB, 60).await;

    let out = crate::common::model_test_wire(
        lmgw_core::modelinfo::local_model_test(&f.state, LADDER, None)
            .await
            .unwrap(),
    );
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["was_running_rung"], Value::Null, "{out}");
    assert_eq!(out["reset_to_base"], true, "{out}");

    let rungs = out["rungs"].as_array().unwrap_or_else(|| panic!("{out}"));
    assert_eq!(rungs.len(), 3, "{out}");
    for (i, (gguf, ctx)) in [
        ("ladder-base.gguf", 64),
        ("ladder-mid.gguf", 128),
        ("ladder-top.gguf", 512),
    ]
    .into_iter()
    .enumerate()
    {
        let r = &rungs[i];
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["rung"], i + 1, "{r}");
        assert_eq!(r["of"], 3, "{r}");
        assert_eq!(r["gguf_path"], gguf, "{r}");
        assert_eq!(r["ctx_size"], ctx, "{r}");
        assert!(r["load_seconds"].as_f64().unwrap() >= 0.0, "{r}");
    }

    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf", "ladder-mid.gguf", "ladder-top.gguf"],
        "the base, then one climb per higher rung"
    );
    assert_eq!(
        f.stops(),
        vec![LADDER.to_string(); 3],
        "each climb stops the rung it replaces, plus the reset at the end"
    );
    assert!(
        f.state
            .runtime()
            .list()
            .iter()
            .all(|e| e.model_id != LADDER),
        "gone, so the next request starts clean at the base"
    );
}

/// §6: "refuse clearly while requests are in flight rather than killing
/// them." A held claim (as a real in-flight request leaves one) makes the
/// test refuse before it stops or starts anything.
#[tokio::test]
async fn local_model_test_refuses_while_busy_and_touches_nothing() {
    let f = ladder_fixture(64 * GIB, 60).await;
    let _hold = ladder_hold(&f).await; // claims (and cold-starts) the base.
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf"]);

    let err = lmgw_core::modelinfo::local_model_test(&f.state, LADDER, None)
        .await
        .unwrap_err();
    assert!(err.contains("claim(s) open"), "{err}");

    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf"],
        "no climb was attempted"
    );
    assert!(f.stops().is_empty(), "busy — nothing was stopped");
}

/// A rung that will not load ends the run there: nothing above a broken rung
/// is reachable through it, so rung 3 is never attempted once rung 2 fails.
#[tokio::test]
async fn local_model_test_stops_at_the_first_rung_that_will_not_load() {
    let f = ladder_fixture(64 * GIB, 60).await;
    f.world().fail_run_file = Some("ladder-mid.gguf".into());

    let out = crate::common::model_test_wire(
        lmgw_core::modelinfo::local_model_test(&f.state, LADDER, None)
            .await
            .unwrap(),
    );
    assert_eq!(out["ok"], false, "{out}");

    let rungs = out["rungs"].as_array().unwrap_or_else(|| panic!("{out}"));
    assert_eq!(rungs.len(), 2, "rung 3 is never reached: {out}");
    assert_eq!(rungs[0]["ok"], true, "{rungs:?}");
    assert_eq!(rungs[0]["rung"], 1);
    assert_eq!(rungs[1]["ok"], false, "{rungs:?}");
    assert_eq!(rungs[1]["rung"], 2);
    assert!(
        rungs[1]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("rung"),
        "{rungs:?}"
    );

    assert!(
        f.state
            .runtime()
            .list()
            .iter()
            .all(|e| e.model_id != LADDER),
        "a rung that would not start leaves the model down, same as a failed \
         cold start"
    );
}

/// Review third pass, T1: `climb`'s `Done` does not always mean "reached the
/// rung asked for" — a live request's own climb can join first and settle on
/// a *higher* rung while the test's own mark (to a lower one) is still
/// unclaimed, raising it. Crediting the resulting probe to the rung the test
/// asked for, rather than the one `hold.sync()` actually lands on, would let
/// a broken lower rung pass as tested.
#[tokio::test]
async fn local_model_test_does_not_credit_a_rung_another_climb_actually_reached() {
    let f = ladder_fixture(64 * GIB, 60).await;
    set_queue_timeout(&f, 30).await;
    // The base is this fixture's first container (nothing warms it first),
    // so its port is deterministic — delaying its answer buys this test a
    // window to acquire two claims before the test's own climb marks,
    // instead of racing the scheduler for it.
    f.world()
        .chat_delay
        .insert(f.first.address().port(), Duration::from_secs(1));

    let test_task = tokio::spawn({
        let state = f.state.clone();
        async move { lmgw_core::modelinfo::local_model_test(&state, LADDER, None).await }
    });

    // Wait for the entry to exist, at the base, before any climb —
    // `ladder_view`/`until_ladder` assume it is already resident, which it
    // is not yet in the first instants of this test.
    let mut tries = 0;
    loop {
        let ready = f
            .state
            .runtime()
            .list()
            .into_iter()
            .find(|v| v.model_id == LADDER)
            .is_some_and(|v| v.state.as_str() == "ready" && v.climbing.is_none());
        if ready {
            break;
        }
        tries += 1;
        assert!(
            tries < 5_000,
            "the base never came up idle before any climb"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let joiner = ladder_hold(&f).await;
    let other = ladder_hold(&f).await;
    let in_flight = other.begin_send().unwrap();

    until_ladder(&f, "the test's own climb to rung 2 marks", |v| {
        v.climbing.as_ref().is_some_and(|c| c.to == 2)
    })
    .await;
    let raiser = spawn_climb(&f, joiner, 2); // rung 3 (0-indexed 2)
    until_ladder(&f, "the joiner raised the mark to rung 3", |v| {
        v.climbing.as_ref().is_some_and(|c| c.to == 3)
    })
    .await;
    drop(in_flight);

    let (joined, _joiner) = raiser.await.unwrap();
    assert!(
        matches!(joined, Ok(lmgw_core::vram::Climbed::Done)),
        "{joined:?}"
    );

    let out = crate::common::model_test_wire(test_task.await.unwrap().unwrap());
    assert_eq!(out["ok"], false, "{out}");
    let rungs = out["rungs"].as_array().unwrap_or_else(|| panic!("{out}"));
    assert_eq!(
        rungs.len(),
        2,
        "the test's own loop stops at rung 2, never attempting rung 3 itself: {out}"
    );
    assert_eq!(rungs[0]["ok"], true, "{rungs:?}");
    let rung2 = &rungs[1];
    assert_eq!(rung2["ok"], false, "{rung2}");
    assert!(
        rung2["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not reached"),
        "{rung2}"
    );
    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf", "ladder-top.gguf"],
        "one reload, straight to the rung the joiner raised — rung 2's own file never loads"
    );
}

/// Review third pass, T2: neither stop is forced, so an idle claim (a tool
/// loop between turns counts as one, same as a real request) can leave the
/// model up on the top rung at the end — named (`reset_to_base: false`), not
/// silently claimed as done.
#[tokio::test]
async fn local_model_test_names_a_closing_stop_it_could_not_make() {
    let f = ladder_fixture(64 * GIB, 60).await;
    // The top rung's own probe is what this test wants in flight when the
    // extra claim is taken — the third container this fixture hands out,
    // deterministically, since nothing else starts one first.
    f.world()
        .chat_delay
        .insert(f._third.address().port(), Duration::from_secs(1));

    let test_task = tokio::spawn({
        let state = f.state.clone();
        async move { lmgw_core::modelinfo::local_model_test(&state, LADDER, None).await }
    });

    let mut tries = 0;
    loop {
        let on_top_rung_and_sending = f
            .state
            .runtime()
            .list()
            .into_iter()
            .find(|v| v.model_id == LADDER)
            .is_some_and(|v| v.rung.as_ref().is_some_and(|r| r.rung == 3) && v.sends >= 1);
        if on_top_rung_and_sending {
            break;
        }
        tries += 1;
        assert!(tries < 10_000, "the test never reached a sending top rung");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // An idle claim — no send of its own — taken while the top rung's probe
    // is still in flight, and kept past the test's own closing stop.
    let extra = ladder_hold(&f).await;

    let out = crate::common::model_test_wire(test_task.await.unwrap().unwrap());
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(
        out["reset_to_base"], false,
        "the extra claim is still open: {out}"
    );
    assert!(
        f.state
            .runtime()
            .list()
            .iter()
            .any(|e| e.model_id == LADDER),
        "left up on the top rung, not stopped, because nothing was killed for it"
    );
    drop(extra);
}

/// Review S1 (§12 entry 71): the gate is yielded to a climb only while that
/// climb is queued for it, not for the whole of its drain. B's climb drains a
/// send on its running rung; C holds the gate waiting for room that only the
/// busy ladder model could give; D, small, queued behind C. D must not
/// overtake C at the gate for as long as B drains — B cannot use the gate yet.
#[tokio::test]
async fn a_draining_climb_does_not_make_a_waiting_admission_give_up_its_place() {
    let f = ladder_fixture(8 * GIB, 0).await;
    set_queue_timeout(&f, 0).await;
    let trigger = ladder_hold(&f).await;
    let other = ladder_hold(&f).await;
    let in_flight = other.begin_send().unwrap();
    let climbing = spawn_climb(&f, trigger, 1);
    until_ladder(&f, "B's drain", |v| v.climbing.is_some()).await;

    // C: chat needs 6.5 with headroom; 8 − 2 (the base) leaves 6.
    let c = tokio::spawn({
        let base = f.gateway.clone();
        async move { chat(&base).await.status() }
    });
    until_vram(&f, "C waiting for the busy ladder model", |v| {
        v["queue"].as_array().is_some_and(|q| {
            q.iter().any(|w| {
                w["model"] == "chat-model" && w["stage"] == "waiting for a busy model to finish"
            })
        })
    })
    .await;
    // D: the embedder (3.5) would fit — behind C.
    let d = tokio::spawn({
        let base = f.gateway.clone();
        async move { embed(&base).await.status() }
    });
    until_vram(&f, "D queued behind C", |v| {
        v["queue"]
            .as_array()
            .is_some_and(|q| q.iter().any(|w| w["model"] == "embed-model"))
    })
    .await;

    // Several of C's waits, all while B drains.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!(
        !f.runs().contains(&"embed-model".to_string()),
        "D overtook C at the gate while B was only draining: {:?}",
        f.runs()
    );

    // Once B's drain is over it queues for the gate, C yields, and B climbs.
    drop(in_flight);
    let (climbed, _trigger) = tokio::time::timeout(Duration::from_secs(5), climbing)
        .await
        .expect("B got the gate once it queued for it")
        .unwrap();
    assert!(
        matches!(climbed, Ok(lmgw_core::vram::Climbed::Done)),
        "{climbed:?}"
    );
    c.abort();
    d.abort();
}

/// Review S7 (§12 entry 74): an admission that yielded the gate to a climb and
/// runs out of time back in the queue was still waiting for busy models — its
/// refusal names them, as any wait for room does, not "another request was
/// being admitted".
#[tokio::test]
async fn a_timeout_after_yielding_to_a_climb_names_what_holds_the_memory() {
    let f = ladder_fixture(7 * GIB, 0).await;
    set_queue_timeout(&f, 1).await;
    assert_eq!(embed(&f.gateway).await.status(), 200);
    f.world().busy.insert("embed-model".into());
    let trigger = ladder_hold(&f).await;

    // C: chat needs 6.5; 7 − 3 (embedder, busy) − 2 (the base, claimed) = 2.
    let c = tokio::spawn({
        let base = f.gateway.clone();
        async move {
            let resp = chat(&base).await;
            (resp.status(), resp.text().await.unwrap_or_default())
        }
    });
    until_vram(&f, "C waiting for busy models", |v| {
        v["queue"].as_array().is_some_and(|q| {
            q.iter().any(|w| {
                w["model"] == "chat-model" && w["stage"] == "waiting for a busy model to finish"
            })
        })
    })
    .await;
    // B: rung 2 needs 4.5; with the base freed 4 — B takes the gate from C
    // and holds it, waiting for the busy embedder, past C's budget.
    let climbing = spawn_climb(&f, trigger, 1);

    let (status, body) = c.await.unwrap();
    assert_eq!(status, 503, "{body}");
    assert!(
        body.contains("held by"),
        "names what holds the memory: {body}"
    );
    assert!(body.contains("aux/embed-model"), "{body}");
    assert!(
        !body.contains("another request was still being admitted"),
        "{body}"
    );
    let (climbed, _trigger) = climbing.await.unwrap();
    assert_eq!(climbed.unwrap_err().kind(), "vram_queue_timeout");
}

/// Review S6 (§12 entry 73): an edit during the drain leaves the rung the
/// request was climbing to too small for its need. The rung is picked again
/// by that need on the row as it is now — one reload, to the rung that holds
/// it — instead of loading the smaller slot the old index names now.
#[tokio::test]
async fn an_edit_during_the_drain_re_picks_the_rung_by_the_requests_need() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let holder = ladder_hold(&f).await;
    let other = ladder_hold(&f).await;
    let in_flight = other.begin_send().unwrap();

    // Needs 100 per slot: rung 2 (128) holds it on the row as judged.
    let climbing = tokio::spawn({
        let state = f.state.clone();
        async move {
            let climbed = lmgw_core::vram::climb_for(&state, &holder, 1, 100, "needs 100").await;
            (climbed, holder)
        }
    });
    until_ladder(&f, "the mark", |v| v.climbing.is_some()).await;
    edit_ladder(&f, |ladder| ladder[0].ctx_size = 96).await;
    drop(in_flight);

    let (climbed, _holder) = climbing.await.unwrap();
    assert!(
        matches!(climbed, Ok(lmgw_core::vram::Climbed::Done)),
        "{climbed:?}"
    );
    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf", "ladder-top.gguf"],
        "rung 2 now holds 96: straight to rung 3"
    );
    assert_eq!(
        f.world().run_files.last().unwrap().2.as_deref(),
        Some("512")
    );
}

/// Review S6 (§12 entry 73), the explicit rung: an edit during the drain
/// leaves the rung asked for no bigger than the one that runs. Nothing is
/// reloaded — the caller judges again.
#[tokio::test]
async fn an_edit_that_leaves_no_bigger_slot_ends_the_climb_without_a_reload() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let holder = ladder_hold(&f).await;
    lmgw_core::vram::climb(&f.state, &holder, 1, "to rung 2")
        .await
        .unwrap();
    holder.sync().await.unwrap();
    let other = ladder_hold(&f).await;
    let in_flight = other.begin_send().unwrap();

    let climbing = spawn_climb(&f, holder, 2);
    until_ladder(&f, "the mark", |v| v.climbing.is_some()).await;
    // Still valid (64 < 96 < 120), but rung 3 is now smaller than the running
    // rung 2's 128.
    edit_ladder(&f, |ladder| {
        ladder[0].ctx_size = 96;
        ladder[1].ctx_size = 120;
    })
    .await;
    drop(in_flight);

    let (climbed, _holder) = climbing.await.unwrap();
    assert!(
        matches!(climbed, Ok(lmgw_core::vram::Climbed::Done)),
        "{climbed:?}"
    );
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf", "ladder-mid.gguf"]);
    let view = ladder_view(&f);
    assert!(view.climbing.is_none(), "the mark is cleared");
    assert_eq!(view.rung.unwrap().rung, 2, "rung 2 serves on");
}
