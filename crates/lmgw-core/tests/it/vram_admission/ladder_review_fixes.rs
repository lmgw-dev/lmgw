//! Ladder climbs: review fixes (phase 3 review; ladder design §12 entries 47+)

use super::*;

/// Edit the ladder row's higher rungs the way a save does: the row, then the
/// snapshot. No apply: a climb in progress keeps the model busy.
pub(super) async fn edit_ladder(f: &Fixture, edit: impl FnOnce(&mut Vec<lmgw_core::ladder::Rung>)) {
    let row = f
        .state
        .snapshot()
        .local_models
        .iter()
        .find(|m| m.model_id == LADDER)
        .cloned()
        .unwrap();
    let mut ladder = row.ladder.clone();
    edit(&mut ladder);
    store::update_local_model(
        &f.state.db,
        row.id,
        &NewLocalModel {
            model_id: row.model_id,
            gguf_path: row.gguf_path,
            params: row.params,
            args: row.args,
            idle_seconds: row.idle_seconds,
            enabled: row.enabled,
            public: row.public,
            image: row.image,
            extra_run_args: row.extra_run_args,
            warm_start: row.warm_start,
            hold_fallback_mode: row.hold_fallback_mode,
            hold_fallback: row.hold_fallback,
            capabilities_override: row.capabilities_override,
            ladder,
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// Spawn `vram::climb(hold, to)`, handing the hold back with the answer.
pub(super) fn spawn_climb(
    f: &Fixture,
    hold: lmgw_core::vram::LocalHold,
    to: usize,
) -> tokio::task::JoinHandle<(
    Result<lmgw_core::vram::Climbed, lmgw_core::error::GatewayError>,
    lmgw_core::vram::LocalHold,
)> {
    let state = f.state.clone();
    tokio::spawn(async move {
        let climbed = lmgw_core::vram::climb(&state, &hold, to, "a test's need").await;
        (climbed, hold)
    })
}

/// Poll the ladder model's view until `ready` holds.
pub(super) async fn until_ladder(
    f: &Fixture,
    what: &str,
    ready: impl Fn(&lmgw_core::runtime::registry::RuntimeView) -> bool,
) {
    for _ in 0..3_000 {
        if ready(&ladder_view(f)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("{what}: {:?}", ladder_view(f));
}

/// Review finding 3 (§12 entry 49): a joiner raises the climb to a rung that
/// does not fit right now. The holder still climbs to its own rung, which
/// does — it is never failed for the joiner's need — and the joiner, judging
/// again on the rung that runs, gets its own verdict.
#[tokio::test]
async fn a_joiners_rung_that_does_not_fit_now_never_fails_the_holder() {
    let f = ladder_fixture(12 * GIB, 0).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    set_slot_busy(&f, true);
    set_queue_timeout(&f, 1).await;
    let holder = ladder_hold(&f).await;
    let joiner = ladder_hold(&f).await;
    let other = ladder_hold(&f).await;
    let in_flight = other.begin_send().unwrap();

    let first = spawn_climb(&f, holder, 1);
    until_ladder(&f, "the holder's mark", |v| v.climbing.is_some()).await;
    let second = spawn_climb(&f, joiner, 2);
    until_ladder(&f, "the joiner's raise", |v| {
        v.climbing.as_ref().is_some_and(|c| c.to == 3)
    })
    .await;
    drop(in_flight);

    // 12 − 6 (chat, busy) − 2 (the base) = 4 free, 6 with the base counted
    // as freed: rung 2 (4.5) fits, rung 3 (8.5) does not.
    let (climbed, _holder) = first.await.unwrap();
    assert!(
        matches!(climbed, Ok(lmgw_core::vram::Climbed::Done)),
        "{climbed:?}"
    );
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf", "ladder-mid.gguf"]);
    let (joined, joiner) = second.await.unwrap();
    assert!(matches!(joined, Ok(lmgw_core::vram::Climbed::Done)));

    joiner.sync().await.unwrap();
    assert_eq!(joiner.rung().map(|r| r.index), Some(1));
    let own = lmgw_core::vram::climb(&f.state, &joiner, 2, "its own need").await;
    match own {
        Err(lmgw_core::error::GatewayError::VramQueueTimeout { model, .. }) => {
            assert_eq!(model, "ladder-model rung 3/3 (ladder-top.gguf)")
        }
        other => panic!("the joiner's own verdict: {other:?}"),
    }
    assert_eq!(ladder_view(&f).rung.unwrap().rung, 2, "rung 2 serves on");
}

/// Review finding 3's privacy half (§12 entry 49): the joiner's raised rung
/// is short of VRAM outside lmgw's control, the holder's own is not. The
/// holder climbs to its own rung — it is never answered by its cloud
/// fallback for somebody else's need.
#[tokio::test]
async fn a_holder_is_never_sent_to_its_fallback_for_a_joiners_rung() {
    let f = ladder_fixture(12 * GIB, 0).await;
    f.attribute(4 * GIB);
    let _cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    let holder = ladder_hold(&f).await;
    assert!(holder.policy().is_some());
    // A joiner that cannot fall back, so it joins instead of answering from
    // its own verdict before the mark.
    let route = f.state.snapshot().resolve(LADDER).unwrap();
    let joiner = lmgw_core::vram::admit(&f.state, &route, LADDER)
        .await
        .unwrap()
        .unwrap();
    let other = ladder_hold(&f).await;
    let in_flight = other.begin_send().unwrap();

    let first = spawn_climb(&f, holder, 1);
    until_ladder(&f, "the holder's mark", |v| v.climbing.is_some()).await;
    let second = spawn_climb(&f, joiner, 2);
    until_ladder(&f, "the joiner's raise", |v| {
        v.climbing.as_ref().is_some_and(|c| c.to == 3)
    })
    .await;
    drop(in_flight);

    // 12 − 4 outside − 2 (the base) = 6 free, 8 with the base freed: rung 2
    // (4.5) fits; rung 3 (8.5) would be short of outside VRAM.
    let (climbed, _holder) = first.await.unwrap();
    assert!(
        matches!(climbed, Ok(lmgw_core::vram::Climbed::Done)),
        "answered locally, not by the fallback: {climbed:?}"
    );
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf", "ladder-mid.gguf"]);
    let _ = second.await.unwrap();
}

/// Review finding 4 (§12 entry 50): the owner edits the rung while the climb
/// drains. The climb starts the row as it is when it starts, not as it was
/// when it was marked.
#[tokio::test]
async fn an_edit_during_the_drain_takes_effect_in_the_climb() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let holder = ladder_hold(&f).await;
    let other = ladder_hold(&f).await;
    let in_flight = other.begin_send().unwrap();

    let climbing = spawn_climb(&f, holder, 1);
    until_ladder(&f, "the mark", |v| v.climbing.is_some()).await;
    edit_ladder(&f, |ladder| ladder[0].ctx_size = 256).await;
    drop(in_flight);

    let (climbed, holder) = climbing.await.unwrap();
    assert!(matches!(climbed, Ok(lmgw_core::vram::Climbed::Done)));
    let (_, file, ctx) = f.world().run_files.last().cloned().unwrap();
    assert_eq!(
        (file.as_str(), ctx.as_deref()),
        ("ladder-mid.gguf", Some("256"))
    );
    holder.sync().await.unwrap();
    assert_eq!(holder.gate_facts().unwrap().params.ctx_size, Some(256));
}

/// Review finding 5 (§12 entry 51): a request parked on a climb whose rung
/// fails to start goes back to admission — which reads the hold again. The
/// owner switched it on while the climb loaded: the request is refused with
/// `gpu_hold`, and the base is not cold-started under the hold.
#[tokio::test]
async fn a_request_sent_back_by_a_failed_climb_honours_a_hold_engaged_meanwhile() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let holder = ladder_hold(&f).await;
    f.world().fail_run_file = Some("ladder-top.gguf".into());
    let (open, gate) = tokio::sync::watch::channel(false);
    *f.podman.gate.lock().unwrap() = Some(gate);

    let climbing = spawn_climb(&f, holder, 2);
    until_ladder(&f, "the new rung loading", |v| {
        v.state.as_str() == "starting"
    })
    .await;
    let waiting = tokio::spawn({
        let state = f.state.clone();
        async move {
            lmgw_core::gate::open(
                &state,
                LADDER,
                lmgw_core::gate::RouteCheck::Text("/v1/chat/completions"),
            )
            .await
            .map(|o| o.hold.is_some())
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished(), "parked on the climb");
    engage_hold(&f).await;
    open.send_replace(true);

    let (climbed, _holder) = climbing.await.unwrap();
    assert_eq!(climbed.unwrap_err().http_status(), 502);
    let refused = waiting.await.unwrap().expect_err("refused under the hold");
    assert_eq!(refused.error.kind(), "gpu_hold", "{}", refused.error);
    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf", "ladder-top.gguf"],
        "no base start under the hold"
    );
}

/// Review finding 7 (§12 entry 52), the raise: the joiner raised the climb to
/// its rung and that rung will not start. The joiner — which needed exactly
/// that rung — is told so, instead of re-admitting the base only to try the
/// same broken rung again. The holder never needed it: it judges again, and
/// climbs to its own rung from the base. The broken rung is tried once.
#[tokio::test]
async fn a_joiner_of_a_failed_rung_gets_the_failure_and_the_holder_climbs_its_own() {
    let f = ladder_fixture(16 * GIB, 0).await;
    f.world().fail_run_file = Some("ladder-top.gguf".into());
    let holder = ladder_hold(&f).await;
    let joiner = ladder_hold(&f).await;
    let other = ladder_hold(&f).await;
    let in_flight = other.begin_send().unwrap();

    let first = spawn_climb(&f, holder, 1);
    until_ladder(&f, "the holder's mark", |v| v.climbing.is_some()).await;
    let second = spawn_climb(&f, joiner, 2);
    until_ladder(&f, "the joiner's raise", |v| {
        v.climbing.as_ref().is_some_and(|c| c.to == 3)
    })
    .await;
    drop(in_flight);

    let (joined, _joiner) = second.await.unwrap();
    let err = joined.unwrap_err();
    assert_eq!(err.http_status(), 502);
    assert!(
        err.to_string()
            .contains("climbing 'ladder-model' to rung 3/3 (ladder-top.gguf) failed"),
        "{err}"
    );
    let (climbed, holder) = first.await.unwrap();
    assert!(
        matches!(climbed, Ok(lmgw_core::vram::Climbed::Done)),
        "not failed for the joiner's rung: {climbed:?}"
    );

    holder.sync().await.unwrap();
    assert_eq!(holder.rung().map(|r| r.index), Some(0), "back at the base");
    lmgw_core::vram::climb(&f.state, &holder, 1, "its own need")
        .await
        .expect("its own rung starts");
    assert_eq!(
        ladder_runs(&f),
        vec![
            "ladder-base.gguf",
            "ladder-top.gguf",
            "ladder-base.gguf",
            "ladder-mid.gguf"
        ]
    );
}

/// Review finding 7 (§12 entry 52), the same rung: two requests need rung 3,
/// the second joins the first's climb, and rung 3 will not start. Both get the
/// 502 naming it; rung 3 was tried once.
#[tokio::test]
async fn triggers_that_need_the_same_broken_rung_share_one_failure() {
    let f = ladder_fixture(16 * GIB, 0).await;
    f.world().fail_run_file = Some("ladder-top.gguf".into());
    let first_hold = ladder_hold(&f).await;
    let second_hold = ladder_hold(&f).await;
    let other = ladder_hold(&f).await;
    let in_flight = other.begin_send().unwrap();

    let first = spawn_climb(&f, first_hold, 2);
    until_ladder(&f, "the first mark", |v| v.climbing.is_some()).await;
    let second = spawn_climb(&f, second_hold, 2);
    // Nothing on the view says a trigger joined: give its (immediate) pre-checks
    // time to reach the mark before the drain ends.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(in_flight);

    for task in [first, second] {
        let (climbed, _hold) = task.await.unwrap();
        let err = climbed.unwrap_err();
        assert!(
            err.to_string()
                .contains("climbing 'ladder-model' to rung 3/3 (ladder-top.gguf) failed"),
            "{err}"
        );
    }
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf", "ladder-top.gguf"]);
}

/// Review finding 8 (§12 entry 53): another model's admission holds the gate
/// and waits for room that only the ladder model's memory could give — and
/// the ladder model is busy exactly because its trigger waits on a climb
/// that waits on that gate. With no queue timeout that was a deadlock. The
/// waiting admission yields the gate to the pending climb, which decides,
/// climbs, and lets it measure again.
#[tokio::test]
async fn a_climb_is_not_stuck_behind_an_admission_waiting_for_its_model() {
    let f = ladder_fixture(8 * GIB, 0).await;
    set_queue_timeout(&f, 0).await;
    let holder = ladder_hold(&f).await;

    // chat needs 6.5 with headroom; 8 − 2 (the base, claimed) leaves 6.
    let chatting = tokio::spawn({
        let base = f.gateway.clone();
        async move { chat(&base).await.status() }
    });
    until_vram(&f, "chat waiting for the busy ladder model", |v| {
        v["queue"].as_array().is_some_and(|q| {
            q.iter().any(|w| {
                w["model"] == "chat-model" && w["stage"] == "waiting for a busy model to finish"
            })
        })
    })
    .await;

    // Rung 2 (4.5 with headroom) fits once the base counts as freed.
    let climbed = tokio::time::timeout(
        Duration::from_secs(5),
        lmgw_core::vram::climb(&f.state, &holder, 1, "why"),
    )
    .await
    .expect("the climb got the gate");
    assert!(
        matches!(climbed, Ok(lmgw_core::vram::Climbed::Done)),
        "{climbed:?}"
    );
    assert_eq!(ladder_runs(&f), vec!["ladder-base.gguf", "ladder-mid.gguf"]);
    assert!(
        !chatting.is_finished(),
        "chat still waits for room, as it should"
    );
    chatting.abort();
}

/// Review finding 1 over HTTP: a ladder saved before §4.3's trained-context
/// rule, whose base is configured at 64 per slot on weights trained at 48.
/// llama-server caps that slot at 48, so lmgw judges the base on 48 — and
/// says so — and a request that fits the configured 64 but not the real 48
/// climbs instead of coming back truncated.
#[tokio::test]
async fn a_rung_is_judged_on_the_slot_its_trained_context_leaves() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let dir = std::path::PathBuf::from(f.state.snapshot().settings.router.models_dir.clone());
    lmgw_core::gguf::synth::chat("qwen3", 48).write_to(&dir.join("capped-base.gguf"));
    lmgw_core::gguf::synth::chat("qwen3", 512).write_to(&dir.join("capped-top.gguf"));
    store::insert_local_model(
        &f.state.db,
        &NewLocalModel {
            model_id: "capped".into(),
            gguf_path: "capped-base.gguf".into(),
            params: lmgw_core::config::LlamaParams {
                ctx_size: Some(64),
                parallel: Some(1),
                n_predict: Some(16),
                ..Default::default()
            },
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![lmgw_core::ladder::Rung {
                gguf_path: "capped-top.gguf".into(),
                ctx_size: 512,
            }],
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    let ask = |n: usize| {
        chat_body(
            &f.gateway,
            json!({"model": "capped", "max_tokens": 16,
                   "messages": [{"role": "user", "content": words("c", n)}]}),
        )
    };

    // 21 prompt tokens + 16 = 37 ≤ 48.
    let resp = ask(20).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-lmgw-rung"),
        Some("1/2; ctx=48; gguf=capped-base.gguf")
    );
    // 41 + 16 = 57: inside the configured 64, past the real 48.
    let resp = ask(40).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-lmgw-rung"),
        Some("2/2; ctx=512; gguf=capped-top.gguf")
    );
}

/// Review finding 6: the container a ladder send goes to and the facts it is
/// judged by come from one read of the claim — on the base, and after the
/// claim followed a climb to rung 3. (The race it closes, a shared hold moved
/// by another task between two reads, cannot be forced from here; this pins
/// that the one read agrees with the registry at both ends.)
#[tokio::test]
async fn a_holds_attempt_and_its_facts_are_read_together() {
    let f = ladder_fixture(16 * GIB, 0).await;
    let hold = ladder_hold(&f).await;
    let read = |hold: &lmgw_core::vram::LocalHold| {
        let ((port, generation), facts) = hold.attempt_with_facts();
        let facts = facts.expect("a chat container has facts");
        let view = ladder_view(&f);
        assert_eq!((port, generation), (view.port, view.generation));
        assert_eq!((port, generation), hold.attempt());
        facts.rung.map(|r| r.index)
    };
    assert_eq!(read(&hold), Some(0));
    climb_to_top(&f, &hold).await.unwrap();
    hold.sync().await.unwrap();
    assert_eq!(read(&hold), Some(2));
}

/// Review finding 13: a `/v1/responses` turn that needs a climb, while VRAM
/// outside lmgw's control leaves no room for it, is answered by the fallback.
/// The unary answer leaves after its last turn, so it says so —
/// `x-lmgw-fallback` with its reason, and no rung. A stream's headers left
/// before its first turn ran: they keep the rung stamped at open.
#[tokio::test]
async fn a_responses_turn_the_fallback_answered_is_named_on_the_unary_answer() {
    let f = ladder_fixture(12 * GIB, 0).await;
    f.attribute(6 * GIB);
    let _cloud = cloud_chat(&f, "cloud-chat").await;
    set_global_fallback(&f, "cloud-chat").await;
    // A client max output above the row's `n_predict` (16): the clamp is
    // planned at open, and the fallback's turn is sent the client's own value.
    let ask = |stream: bool| {
        f.gateway
            .client()
            .post(format!("{}/v1/responses", f.gateway))
            .json(
                &json!({"model": LADDER, "input": words("r", 200), "stream": stream,
                          "max_output_tokens": 64}),
            )
            .send()
    };

    let resp = ask(false).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-fallback"), Some("cloud-chat"));
    assert_eq!(fallback_reason(&resp), Some("external_vram"));
    assert_eq!(header(&resp, "x-lmgw-rung"), None);
    assert_eq!(
        header(&resp, "x-lmgw-max-tokens-clamped"),
        None,
        "the fallback was not clamped (review S2)"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("from the cloud"), "{body}");

    let resp = ask(true).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-lmgw-rung"),
        Some("1/3; ctx=64; gguf=ladder-base.gguf"),
        "a stream's headers left before the turn"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("from the cloud"), "{body}");
    assert!(f.stops().is_empty(), "no climb: {:?}", f.stops());
}

/// Review finding 14, the gate's side of "two triggers at once": two
/// admitted requests whose counts run beside their sends on rung 1 both find
/// they do not fit. A (needs rung 2) marks first; its drain waits for a third
/// send still answering on rung 1. B (needs rung 3) then joins A's mark and
/// raises it before the claim, so one reload to rung 3 serves both — the
/// in-flight send finishes on rung 1, and nothing climbs twice.
#[tokio::test]
async fn a_second_trigger_joins_and_raises_the_first_ones_climb() {
    let f = ladder_fixture(16 * GIB, 0).await;
    assert_eq!(
        ladder_chat(&f, Dialect::OpenAi, false, 3, 16)
            .await
            .status(),
        200
    );
    let base = ladder_view(&f).port;
    {
        let mut w = f.world();
        w.chat_delay.insert(base, Duration::from_millis(1_500));
        w.template_delay_by_tag = vec![
            ("alpha".into(), Duration::from_millis(150)),
            ("bravo".into(), Duration::from_millis(600)),
        ];
    }
    let send = |tag: &'static str, n: usize| {
        let gw = f.gateway.clone();
        tokio::spawn(async move {
            chat_body(
                &gw,
                json!({"model": LADDER, "max_tokens": 16,
                       "messages": [{"role": "user", "content": words(tag, n)}]}),
            )
            .await
        })
    };
    let bodies_on_base = || f.world().chat_bodies.get(&base).map_or(0, Vec::len);

    // C fits rung 1 and is answering there for 1.5 s.
    let c = send("charlie", 3);
    while bodies_on_base() < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // A: 56 tokens + 16 > 64, rung 2. B: 121 + 16 > 128, rung 3 — B's
    // prompt is past rung 1 too, but its refusal is held with the send, so
    // its (slower) count is what decides.
    let a = send("alpha", 55);
    let b = send("bravo", 120);
    let mut raised = false;
    for _ in 0..1_000 {
        let climbing = ladder_view(&f).climbing;
        if climbing.as_ref().is_some_and(|m| m.to == 3) {
            raised = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(raised, "B raised A's mark to rung 3 before its claim");

    let c = c.await.unwrap();
    assert_eq!(c.status(), 200);
    assert_eq!(
        header(&c, "x-lmgw-rung"),
        Some("1/3; ctx=64; gguf=ladder-base.gguf"),
        "the send in flight finished on rung 1"
    );
    for (who, resp) in [("A", a.await.unwrap()), ("B", b.await.unwrap())] {
        assert_eq!(resp.status(), 200, "{who}");
        assert_eq!(
            header(&resp, "x-lmgw-rung"),
            Some("3/3; ctx=512; gguf=ladder-top.gguf"),
            "{who}"
        );
    }
    assert_eq!(
        ladder_runs(&f),
        vec!["ladder-base.gguf", "ladder-top.gguf"],
        "one reload served both"
    );
    assert_eq!(f.stops(), vec![LADDER.to_string()]);
}
