//! How a read-aloud ends, and what it says on the way (chat-voice design
//! §6.3, §6.4; WP4 review m3, m8, n3): a TTS that fails on a later clause
//! ends the speech, not the text; `speech/stop` from another window stops a
//! stored reply's read-aloud, and counts only those still speaking; under
//! the GPU hold a TTS fallback answers and is named, and a row with none is
//! `gpu_hold`; and a cold TTS warmed beside the prefill says `loading` and
//! its end once. The chat upstream and the TTS are fakes; the cold TTS is a
//! row on the fake GPU world.

use std::sync::Arc;

use lmgw_core::config::HoldFallbackMode;
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAudioModel};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::chat_voice_speak::{
    messages, names, position, post, spoken, stored_reply, thread, tts_rows, until, world, Reader,
};
use crate::chat_voice_speak_style::design_world;
use crate::support::realtime_fakes::{Step, Turn};
use crate::support::realtime_tts::{add_cloud_tts_alias, speech, tts_fake, wav, Tts, TTS_ALIAS};

#[tokio::test]
async fn a_tts_that_fails_on_a_later_clause_ends_the_speech_not_the_text() {
    let w = world(|_| {}).await;
    let tid = thread(&w.gw, "chatty").await;
    w.tts.push(Tts::Wav(wav(&speech(300), 24_000)));
    w.tts.push(Tts::Status(
        500,
        json!({"error": {"message": "the engine fell over", "type": "server_error"}}),
    ));
    w.chat.push(Turn::text(&[
        "Erster Satz. ",
        "Zweiter Satz. ",
        "Dritter Satz.",
    ]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi", "speak": true}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    assert_eq!(spoken(&events), ["Erster Satz."]);
    let error = position(&events, |(e, _)| e == "speech_error");
    assert!(
        events[error].1["message"]
            .as_str()
            .unwrap()
            .contains("fell over"),
        "{events:?}"
    );
    // It ends the speech: nothing after it is the speech's.
    assert!(!names(&events).contains(&"speech_done"), "{events:?}");
    assert!(
        events[error + 1..]
            .iter()
            .all(|(e, _)| !e.starts_with("speech")),
        "{events:?}"
    );
    // The text is whole, and saved.
    let done = &events[position(&events, |(e, _)| e == "done")].1;
    assert_eq!(done["saved"], true);
    let m = messages(&w.gw, tid).await;
    assert_eq!(
        m.last().unwrap().1,
        "Erster Satz. Zweiter Satz. Dritter Satz."
    );
    // One row for the read-aloud, the failure's.
    let state = w.state.clone();
    until("the speech's row is written", async || {
        !tts_rows(&state, TTS_ALIAS).await.is_empty()
    })
    .await;
    let rows = tts_rows(&w.state, TTS_ALIAS).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_ne!(rows[0].1, 200, "{rows:?}");
}

#[tokio::test]
async fn speech_stop_from_another_window_stops_a_stored_reply() {
    let w = world(|_| {}).await;
    let tid = thread(&w.gw, "chatty").await;
    w.chat.push(Turn::text(&["Eins. ", "Zwei. ", "Drei."]));
    let mid = stored_reply(&w.gw, tid, "zähl").await;
    // The second clause is never answered: only the stop ends it.
    w.tts.push(Tts::Wav(wav(&speech(300), 24_000)));
    w.tts.push(Tts::Held(
        Arc::new(Notify::new()),
        wav(&speech(300), 24_000),
    ));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
        json!({}),
    )
    .await;
    let mut window_a = Reader::new(r);
    window_a.until("speech", |_| true).await;
    let seen = w.tts.seen.clone();
    until("the second clause is with the TTS", async || {
        seen.count() == 2
    })
    .await;
    // Another window stops the thread's speech.
    let stopped: Value = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/speech/stop"),
        json!({}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(stopped, json!({"ok": true, "stopped": 1}));
    let rest = window_a.rest().await;
    assert_eq!(names(&rest), ["speech_done"], "{rest:?}");
    assert_eq!(rest[0].1["stopped"], true);
    let state = w.state.clone();
    until("the speak's row is written", async || {
        !tts_rows(&state, TTS_ALIAS).await.is_empty()
    })
    .await;
    assert_eq!(
        tts_rows(&w.state, TTS_ALIAS).await,
        [("chat".to_string(), 200, Some("canceled".to_string()))]
    );
    let again: Value = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/speech/stop"),
        json!({}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(again["stopped"], 0);
}

#[tokio::test]
async fn speech_stop_counts_only_the_read_alouds_still_speaking() {
    // No TTS anywhere: the read-aloud is refused at once, while the text
    // still streams (review n3: it was counted while its stream lasted).
    let w = world(|s| s.chat_tts_alias.clear()).await;
    let tid = thread(&w.gw, "chatty").await;
    let hold = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Halb. "),
        Step::Wait(hold.clone()),
        Step::Text("Ganz."),
        Step::Finish("stop"),
    ]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi", "speak": true}),
    )
    .await;
    let mut sse = Reader::new(r);
    let events = sse.until("speech_error", |_| true).await;
    assert_eq!(events[0].0, "turn");
    let (gw, id) = (w.gw.clone(), tid);
    until("the refused read-aloud is let go", async || {
        let v: Value = post(
            &gw,
            &format!("/chat/api/threads/{id}/speech/stop"),
            json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
        v["stopped"] == 0
    })
    .await;
    hold.notify_one();
    let rest = sse.rest().await;
    assert_eq!(rest.last().unwrap().0, "done", "{rest:?}");
}

#[tokio::test]
async fn a_voice_that_failed_is_not_counted_while_its_text_goes_on() {
    let w = world(|_| {}).await;
    let tid = thread(&w.gw, "chatty").await;
    w.tts.push(Tts::Status(
        500,
        json!({"error": {"message": "the engine fell over", "type": "server_error"}}),
    ));
    let hold = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Erster Satz. "),
        Step::Wait(hold.clone()),
        Step::Text("Zweiter Satz."),
        Step::Finish("stop"),
    ]));
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hi", "speak": true}),
    )
    .await;
    let mut sse = Reader::new(r);
    sse.until("speech_error", |_| true).await;
    // The text is still held, its speech over: nothing to stop.
    let (gw, id) = (w.gw.clone(), tid);
    until("the failed read-aloud is let go", async || {
        let v: Value = post(
            &gw,
            &format!("/chat/api/threads/{id}/speech/stop"),
            json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
        v["stopped"] == 0
    })
    .await;
    hold.notify_one();
    let rest = sse.rest().await;
    assert!(
        rest.iter()
            .any(|(e, d)| e == "delta" && d["text"] == "Zweiter Satz."),
        "{rest:?}"
    );
    assert_eq!(rest.last().unwrap().0, "done", "{rest:?}");
    assert_eq!(w.tts.seen.count(), 1, "nothing more was synthesized");
}

/// An lmgw audio row `held-tts` (alias `audio/held-tts`), with `fallback`
/// as its hold fallback when given.
async fn local_tts(state: &SharedState, fallback: Option<&str>) {
    store::insert_audio_model(
        &state.db,
        &NewAudioModel {
            model_id: "held-tts".into(),
            family: "pocket_tts".into(),
            path: "pocket".into(),
            task: "tts".into(),
            mode: "offline".into(),
            lazy: None,
            busy_timeout_ms: None,
            backend: None,
            threads: None,
            load_options: Default::default(),
            session_options: Default::default(),
            default_request_options: Default::default(),
            model_spec_override: None,
            config_id: None,
            weight_id: None,
            voice_presets: Default::default(),
            default_voice_preset: None,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: if fallback.is_some() {
                HoldFallbackMode::Alias
            } else {
                HoldFallbackMode::None
            },
            hold_fallback: fallback.map(str::to_string),
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

#[tokio::test]
async fn under_the_hold_a_tts_fallback_answers_and_is_named() {
    let cloud = tts_fake(&[]).await;
    let w = world(|s| {
        s.hold.active = true;
        s.chat_tts_alias = "audio/held-tts".into();
    })
    .await;
    add_cloud_tts_alias(&w.state, &cloud, "cloud-tts").await;
    local_tts(&w.state, Some("cloud-tts")).await;
    let tid = thread(&w.gw, "chatty").await;
    w.chat.push(Turn::text(&["Guten Tag."]));
    let mid = stored_reply(&w.gw, tid, "hallo").await;
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
        json!({}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    // Held: nothing loads, so no `state`; the fallback is named at once.
    assert_eq!(
        names(&events),
        ["voice", "speech", "speech_done"],
        "{events:?}"
    );
    assert_eq!(events[0].1["tts"], "audio/held-tts");
    assert_eq!(events[0].1["tts_answered_by"], "cloud-tts");
    assert_eq!(events[2].1["tts_answered_by"], "cloud-tts");
    assert_eq!(cloud.seen.count(), 1);
    assert_eq!(w.tts.seen.count(), 0);
    let state = w.state.clone();
    until("the speak's row is written", async || {
        !tts_rows(&state, "audio/held-tts").await.is_empty()
    })
    .await;
    assert_eq!(
        tts_rows(&w.state, "audio/held-tts").await,
        [("chat".to_string(), 200, None)]
    );
}

#[tokio::test]
async fn under_the_hold_a_tts_with_no_fallback_is_gpu_hold() {
    let w = world(|s| {
        s.hold.active = true;
        s.chat_tts_alias = "audio/held-tts".into();
    })
    .await;
    local_tts(&w.state, None).await;
    let tid = thread(&w.gw, "chatty").await;
    w.chat.push(Turn::text(&["Guten Tag."]));
    let mid = stored_reply(&w.gw, tid, "hallo").await;
    let r = post(
        &w.gw,
        &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
        json!({}),
    )
    .await;
    let events = Reader::new(r).rest().await;
    assert_eq!(names(&events), ["speech_error"], "{events:?}");
    assert_eq!(events[0].1["code"], "gpu_hold");
}

/// Review m3: the chat model is up (a provider), the TTS a cold row whose
/// start takes a while. The Background warm beside the prefill and the
/// route's opening at the first clause both find it loading; the page is
/// told `loading` once, and its end once.
#[tokio::test]
async fn a_cold_tts_warmed_beside_the_prefill_says_loading_and_ready_once() {
    let d = design_world(Some("a deep, slow narrator"), |_| {}).await;
    let gate = d.g.gate_runs();
    let tid = thread(&d.gw, "chatty").await;
    d.chat
        .push(Turn::text(&["Guten ", "Tag. ", "Wie geht es?"]));
    let r = post(
        &d.gw,
        &format!("/chat/api/threads/{tid}/send"),
        json!({"content": "hallo", "speak": true}),
    )
    .await;
    let mut sse = Reader::new(r);
    // The text is done and the TTS loading, in either order: the first
    // clause waits on the start in flight, as the warm does.
    let mut events = Vec::new();
    let seen =
        |events: &[(String, Value)], f: &dyn Fn(&(String, Value)) -> bool| events.iter().any(f);
    while !seen(&events, &|(e, _)| e == "done")
        || !seen(&events, &|(e, d)| e == "state" && d["state"] == "loading")
    {
        events.push(sse.next().await.expect("the stream goes on"));
    }
    let g = &d.g;
    until("the start is in flight", async || !g.runs().is_empty()).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    gate.send(true).unwrap();
    events.extend(sse.rest().await);
    let tts: Vec<&str> = events
        .iter()
        .filter(|(e, d)| e == "state" && d["stage"] == "tts")
        .map(|(_, d)| d["state"].as_str().unwrap())
        .collect();
    assert_eq!(tts, ["loading", "ready"], "{events:?}");
    let ready = events
        .iter()
        .find(|(e, d)| e == "state" && d["state"] == "ready")
        .unwrap();
    assert!(ready.1["ms"].as_u64().unwrap() >= 200, "{ready:?}");
    assert_eq!(
        spoken(&events),
        ["Guten Tag.", "Wie geht es?"],
        "{events:?}"
    );
    assert_eq!(events.last().unwrap().0, "speech_done");
    assert_eq!(d.g.runs(), ["design-row"], "one start");
}

/// A stored reply's read-aloud on a cold TTS, abandoned while the model is
/// still loading (review m8): the speech stops, and at most one row is
/// written for it, never two.
#[tokio::test]
async fn a_read_aloud_abandoned_while_its_tts_loads_writes_one_row_at_most() {
    let d = design_world(Some("a deep, slow narrator"), |_| {}).await;
    let tid = thread(&d.gw, "chatty").await;
    d.chat.push(Turn::text(&["Guten Tag."]));
    let mid = stored_reply(&d.gw, tid, "hallo").await;
    let gate = d.g.gate_runs();
    let r = post(
        &d.gw,
        &format!("/chat/api/threads/{tid}/messages/{mid}/speak"),
        json!({}),
    )
    .await;
    let mut sse = Reader::new(r);
    sse.until("state", |s| s["state"] == "loading").await;
    let g = &d.g;
    until("the start is in flight", async || !g.runs().is_empty()).await;
    // The page goes; then the start finishes.
    drop(sse);
    gate.send(true).unwrap();
    let (gw, id) = (d.gw.clone(), tid);
    until("the read-aloud is over", async || {
        let v: Value = post(
            &gw,
            &format!("/chat/api/threads/{id}/speech/stop"),
            json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
        v["stopped"] == 0
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let rows = tts_rows(&d.g.state, "audio/design-row").await;
    assert!(rows.len() <= 1, "{rows:?}");
    assert!(
        d.g.world()
            .speech_bodies
            .iter()
            .all(|b| b["input"] != "Guten Tag."),
        "nothing was said for it: {:?}",
        d.g.world().speech_bodies
    );
}
