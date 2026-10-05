//! Chat voice WP3 review fixes (chat-voice design §4.2): an Admit group
//! keeps every claim until each stage is up, yet never waits on its own
//! claims while it holds the admission gate — not when the card cannot hold
//! the group beside what runs outside lmgw (the group check), and not when
//! that changes after the check (a stage crowded out gives up). A press
//! that ends while its start is in flight lets the start finish and loads
//! nothing. And the `tts` stage. Mock upstreams and the GPU-world fake
//! only.

use std::time::Duration;

use lmgw_core::runtime::registry::RuntimeState;
use serde_json::{json, Value};

use crate::chat_attach_kinds::new_thread;
use crate::chat_voice_dictation::{
    asr_chat_rows, asr_row, resident, states, store_voice, tweak, until, warm, world,
};
use crate::common::Gw;
use crate::support::gpu_world::{Gpu, GIB};

/// Whether a waiter for `ears` or `talk` is in the admission queue.
async fn queued(g: &Gpu) -> bool {
    g.state
        .vram
        .view(&g.state)
        .await
        .queue
        .iter()
        .any(|w| w.model == "ears" || w.model == "talk")
}

/// Until one of `ears`/`talk` is up and claimed by the press while the
/// other waits in the queue.
async fn one_up_one_queued(g: &Gpu) {
    for _ in 0..500 {
        let up_and_claimed = g.state.runtime().list().iter().any(|v| {
            (v.model_id == "ears" || v.model_id == "talk")
                && v.state == RuntimeState::Ready
                && v.in_flight == 1
        });
        if up_and_claimed && queued(g).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never one stage up and claimed with the other queued");
}

fn second(gw: &Gw) -> Gw {
    Gw {
        base: gw.base.clone(),
        key: gw.key.clone(),
    }
}

/// §4.2's group rule: a stage that is up keeps its claim while its sibling
/// still waits for room, so the sibling's admission cannot evict it. `other`
/// is generating for a client lmgw does not see, so the waiting stage has no
/// victim until it finishes; then `other`, not a group member, is evicted.
/// Either admission order works: whichever stage starts first is the one the
/// other would evict if its claim had been dropped.
#[tokio::test]
async fn a_group_keeps_each_claim_until_every_stage_is_up() {
    let (g, gw) = world(10 * GIB, 3).await;
    g.model("other", 4 * GIB).await;
    g.model("ears", 3 * GIB).await;
    g.model("talk", 6 * GIB).await;
    asr_chat_rows(&g, &["ears"]).await;
    tweak(&g.state, |s| s.chat_stt_alias = "ears".into()).await;
    resident(&g, "other").await;
    g.world().busy.insert("other".into());
    let tid = new_thread(&gw, "talk", false).await;
    let gw2 = second(&gw);
    let pressed = tokio::spawn(async move { warm(&gw2, tid, &["asr", "chat"]).await });

    // One stage is up and claimed by the press; the other waits in the queue.
    one_up_one_queued(&g).await;
    tokio::time::sleep(Duration::from_millis(600)).await; // several POLLs of the wait
    assert!(queued(&g).await, "the second stage still waits for room");
    assert!(
        g.stops().is_empty(),
        "a sibling was evicted: {:?}",
        g.stops()
    );

    g.world().busy.clear();
    let events = pressed.await.unwrap();
    for stage in ["asr", "chat"] {
        let walk: Vec<Value> = states(&events, stage)
            .iter()
            .map(|f| f["state"].clone())
            .collect();
        assert_eq!(
            walk,
            [json!("loading"), json!("ready")],
            "{stage}: {events:?}"
        );
    }
    assert_eq!(g.stops(), ["other"]);
    assert!(g.state.runtime().list().iter().all(|v| v.in_flight == 0));
}

/// B1: a group that fits the card's capacity but not beside VRAM used
/// outside lmgw. Its second stage must not wait on the first one's kept
/// claim: the admission gate is global, so every cold start in lmgw would
/// wait with it until `vram.queue_timeout_seconds` (30 s in this world).
#[tokio::test]
async fn a_group_never_waits_on_its_own_claims() {
    let (g, gw) = world(10 * GIB, 4).await;
    g.model("ears", 3 * GIB).await;
    g.model("talk", 6 * GIB).await;
    g.model("other", GIB).await;
    asr_chat_rows(&g, &["ears"]).await;
    tweak(&g.state, |s| s.chat_stt_alias = "ears".into()).await;
    g.world().outside = 2 * GIB; // 3 + 6 fits 10, not the 8 that are free
    let tid = new_thread(&gw, "talk", false).await;

    let events = tokio::time::timeout(Duration::from_secs(5), warm(&gw, tid, &["asr", "chat"]))
        .await
        .expect("a stage waited on its own group's claim, holding the admission gate");
    let frames: Vec<Value> = ["asr", "chat"]
        .iter()
        .flat_map(|s| states(&events, s))
        .collect();
    let skipped = frames
        .iter()
        .find(|f| f["reason"] == "does_not_fit")
        .unwrap_or_else(|| panic!("{events:?}"));
    // The group check names the memory held outside lmgw.
    assert_eq!(skipped["needed_bytes"], 9 * GIB, "{skipped}");
    assert_eq!(skipped["capacity_bytes"], 8 * GIB, "{skipped}");
    assert!(
        skipped["message"]
            .as_str()
            .unwrap()
            .contains("held outside lmgw"),
        "{skipped}"
    );
    assert!(
        g.stops().is_empty(),
        "the group evicted itself: {:?}",
        g.stops()
    );
    assert!(g.state.runtime().list().iter().all(|v| v.in_flight == 0));
    // The gate is free: an unrelated cold start that fits goes straight in.
    let other = tokio::time::timeout(
        Duration::from_secs(2),
        lmgw_core::vram::admit(&g.state, &g.route("other"), "other"),
    )
    .await;
    assert!(
        matches!(other, Ok(Ok(Some(_)))),
        "the admission gate is still held"
    );
}

/// B1 at run time: the group fit at its check, then a program outside lmgw
/// takes memory while the second stage waits (`other` is busy, its only
/// victim). Room for it could now only come from its sibling's kept claim:
/// it gives up as `does_not_fit` within a few polls — the old wait held the
/// admission gate until the queue timeout — every claim is let go, nothing
/// is evicted, and the gate is free.
#[tokio::test]
async fn a_stage_crowded_out_by_its_own_group_gives_up_its_wait() {
    let (g, gw) = world(10 * GIB, 4).await;
    g.model("other", 2 * GIB).await;
    g.model("ears", 3 * GIB).await;
    g.model("talk", 6 * GIB).await;
    g.model("probe", GIB).await;
    asr_chat_rows(&g, &["ears"]).await;
    tweak(&g.state, |s| s.chat_stt_alias = "ears".into()).await;
    resident(&g, "other").await;
    g.world().busy.insert("other".into());
    let tid = new_thread(&gw, "talk", false).await;
    let gw2 = second(&gw);
    let pressed = tokio::spawn(async move { warm(&gw2, tid, &["asr", "chat"]).await });

    one_up_one_queued(&g).await;
    // Still room once `other` finishes: the stage keeps waiting.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        queued(&g).await,
        "it gave up while `other` could still make room"
    );

    // A game takes 2 GiB.
    g.world().outside = 2 * GIB;
    let events = tokio::time::timeout(Duration::from_secs(5), pressed)
        .await
        .expect("the crowded-out stage kept the admission gate")
        .unwrap();
    let walks: Vec<Vec<Value>> = ["asr", "chat"].iter().map(|s| states(&events, s)).collect();
    let (up, out): (Vec<_>, Vec<_>) = walks
        .iter()
        .partition(|w| w.last().is_some_and(|f| f["state"] == "ready"));
    assert_eq!((up.len(), out.len()), (1, 1), "{events:?}");
    let last = out[0].last().unwrap();
    assert_eq!(
        (&last["state"], &last["reason"]),
        (&json!("skipped"), &json!("does_not_fit")),
        "{events:?}"
    );
    assert!(
        last["message"].as_str().unwrap().contains("beside"),
        "{last}"
    );
    assert!(
        last["needed_bytes"].as_u64().unwrap() > last["capacity_bytes"].as_u64().unwrap(),
        "{last}"
    );
    assert!(g.stops().is_empty(), "evicted: {:?}", g.stops());
    until("every claim let go", || {
        g.state.runtime().list().iter().all(|v| v.in_flight == 0)
    })
    .await;
    assert!(!queued(&g).await, "the wait is gone");
    // The gate is free: a Background start takes it and answers at once.
    let snap = g.state.snapshot();
    let fit = tokio::time::timeout(
        Duration::from_secs(2),
        g.state.vram.check_background_start(
            &g.state,
            &snap,
            lmgw_core::runtime::Class::Chat,
            "probe",
        ),
    )
    .await;
    assert!(fit.is_ok(), "the admission gate is still held");
    g.world().busy.clear();
}

/// WP11 server review m2: the three GPU stages of a bound session's
/// connect warm (asr, chat, tts) as one Admit group, where two stages can
/// wait at once — one holding the admission gate, one queued behind it.
/// The group fits at its check; `ears` and `talk` do not fit beside `other`
/// (busy, the only victim) together, so one comes up and the other waits,
/// with the TTS queued behind it; a program outside lmgw then takes memory
/// and the waiting stage is crowded out, in either admission order: two
/// `ready`, one `skipped: does_not_fit`, nothing evicted, every claim let
/// go, and the gate free within a few polls.
#[tokio::test]
async fn a_three_stage_group_crowded_out_after_two_are_up_lets_everything_go() {
    let (g, gw) = world(10 * GIB, 5).await;
    g.model("other", 2 * GIB).await;
    g.model("ears", 3 * GIB).await;
    g.model("talk", 6 * GIB).await;
    g.model("probe", GIB).await;
    asr_chat_rows(&g, &["ears"]).await;
    design_row(&g, "designer", "vdes").await;
    tweak(&g.state, |s| {
        s.chat_stt_alias = "ears".into();
        s.chat_tts_alias = "audio/designer".into();
        s.chat_speech_style = "a calm, low voice".into();
    })
    .await;
    resident(&g, "other").await;
    g.world().busy.insert("other".into());
    let tid = new_thread(&gw, "talk", false).await;
    let gw2 = second(&gw);
    let pressed = tokio::spawn(async move { warm(&gw2, tid, &["asr", "chat", "tts"]).await });

    one_up_one_queued(&g).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        queued(&g).await,
        "it gave up while `other` could still make room"
    );

    // A program outside lmgw takes 1.5 GiB. Whichever of `ears` and
    // `talk` came up, the other cannot get room beside it now, not even
    // once `other` lets go; the TTS (a few hundred bytes) still fits.
    g.world().outside = 3 * GIB / 2;
    let events = tokio::time::timeout(Duration::from_secs(5), pressed)
        .await
        .expect("the crowded-out stage kept the admission gate")
        .unwrap();
    let walks: Vec<Vec<Value>> = ["asr", "chat", "tts"]
        .iter()
        .map(|s| states(&events, s))
        .collect();
    let (up, out): (Vec<_>, Vec<_>) = walks
        .iter()
        .partition(|w| w.last().is_some_and(|f| f["state"] == "ready"));
    assert_eq!((up.len(), out.len()), (2, 1), "{events:?}");
    let last = out[0].last().unwrap();
    assert_eq!(
        (&last["state"], &last["reason"]),
        (&json!("skipped"), &json!("does_not_fit")),
        "{events:?}"
    );
    assert!(g.stops().is_empty(), "evicted: {:?}", g.stops());
    until("every claim let go", || {
        g.state.runtime().list().iter().all(|v| v.in_flight == 0)
    })
    .await;
    assert!(!queued(&g).await, "the wait is gone");
    let snap = g.state.snapshot();
    let fit = tokio::time::timeout(
        Duration::from_secs(2),
        g.state.vram.check_background_start(
            &g.state,
            &snap,
            lmgw_core::runtime::Class::Chat,
            "probe",
        ),
    )
    .await;
    assert!(fit.is_ok(), "the admission gate is still held");
    g.world().busy.clear();
}

/// §4.2 "a container start already in flight finishes": the press ends
/// while its model's start is under way. The start completes (no orphaned
/// `starting` entry), the claim is let go, and no load is sent for a press
/// nobody is waiting on.
#[tokio::test]
async fn a_press_that_ends_mid_start_lets_the_start_finish_and_loads_nothing() {
    let (g, gw) = world(10 * GIB, 2).await;
    g.model("talk", GIB).await;
    asr_row(&g, "ears", false, None).await;
    tweak(&g.state, |s| s.chat_stt_alias = "audio/ears".into()).await;
    let tid = new_thread(&gw, "talk", false).await;
    let release = g.gate_runs();
    let mut r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/voice/warm"))
        .json(&json!({ "stages": ["asr"] }))
        .send()
        .await
        .unwrap();
    let _loading = r.chunk().await.unwrap();
    until("the start is in flight", || g.runs() == ["ears"]).await;
    drop(r);
    tokio::time::sleep(Duration::from_millis(200)).await;
    release.send(true).unwrap();
    until("ears is up and unclaimed", || {
        g.state
            .runtime()
            .list()
            .iter()
            .any(|v| v.model_id == "ears" && v.state == RuntimeState::Ready && v.in_flight == 0)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        g.world().transcriptions.is_empty(),
        "loaded for a press that ended"
    );
    assert!(g.stops().is_empty(), "{:?}", g.stops());
    assert_eq!(g.runs(), ["ears"]);
}

/// A voice-design TTS row `id` on the GPU world (`audio/<id>`), its own
/// description `instruct` unless `task` makes it a row its package cannot
/// run.
async fn design_row(g: &Gpu, id: &str, task: &str) {
    let root = g.models_dir().join(id);
    std::fs::create_dir_all(&root).unwrap();
    crate::realtime_expressive::voice_design(&root);
    let mut row = crate::support::audio_world::tts_row(id, "qwen3_tts");
    row.task = task.into();
    lmgw_core::store::insert_audio_model(&g.state.db, &row)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
}

/// Review m7b, the `tts` stage of a press: refused `422 tts_not_configured`
/// with no TTS at any level; warmed through admission and loaded on the
/// admission's claim with one word in the thread's speech style and its
/// stored seed (no request row, every claim let go after); and a row that
/// can never speak the thread's answers is `skipped: cannot_speak`, with
/// nothing started for it.
#[tokio::test]
async fn a_press_warms_the_tts_in_the_threads_style_and_seed() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("talk", GIB).await;
    let tid = new_thread(&gw, "talk", false).await;
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/voice/warm"))
        .json(&json!({ "stages": ["tts"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "tts_not_configured", "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("Settings → Chat → Voice"),
        "{v}"
    );

    design_row(&g, "designer", "vdes").await;
    design_row(&g, "mismatch", "tts").await;
    tweak(&g.state, |s| {
        s.chat_tts_alias = "audio/designer".into();
        s.chat_speech_style = "a calm, low voice".into();
    })
    .await;
    store_voice(&g.state, tid, json!({ "seed": 4242 })).await;
    let s = states(&warm(&gw, tid, &["tts"]).await, "tts");
    let walk: Vec<&Value> = s.iter().map(|f| &f["state"]).collect();
    assert_eq!(walk, [&json!("loading"), &json!("ready")], "{s:?}");
    assert_eq!(g.runs(), ["designer"]);
    let body = g.world().speech_bodies[0].clone();
    assert_eq!(body["input"], "Hello.", "{body}");
    assert_eq!(body["seed"], 4242, "the stored seed, sent: {body}");
    assert!(
        body.to_string().contains("a calm, low voice"),
        "the thread's style: {body}"
    );
    assert!(g.state.runtime().list().iter().all(|v| v.in_flight == 0));
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_logs")
        .fetch_one(&g.state.db)
        .await
        .unwrap();
    assert_eq!(rows, 0, "a warm-up is no request");

    // The thread's own TTS: a row its package cannot run.
    store_voice(&g.state, tid, json!({ "tts_alias": "audio/mismatch" })).await;
    let s = states(&warm(&gw, tid, &["tts"]).await, "tts");
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(
        (&s[0]["state"], &s[0]["reason"]),
        (&json!("skipped"), &json!("cannot_speak")),
        "{s:?}"
    );
    assert_eq!(g.runs(), ["designer"], "nothing started for it");
}
