//! Where a voice is checked (realtime design §5.3, §9.1, §9.2; WP3 review
//! M1, M2, m6, m9): never by asking the model from the handshake or an
//! update — not under the GPU hold, not of a container that does not answer
//! — but at a response's first clause on the route it holds; a fallback
//! speaking its own voice; a response's own voice; and facts read afresh.

use std::time::Duration;

use lmgw_core::config::HoldFallbackMode;
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAudioModel};
use serde_json::{json, Value};

use crate::support::realtime_fakes::{events_until, next_event, open, send, user_text, Turn, Ws};
use crate::support::realtime_tts::{
    add_cloud_tts_alias, speech_gateway, spoken_session, tts_fake, tts_rows,
};

/// A long lead: nothing waits.
const NO_WAIT: u32 = 60_000;

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

async fn update(ws: &mut Ws, session: Value, event_id: &str) -> Value {
    let mut s = json!({"type": "realtime"});
    for (k, v) in session.as_object().unwrap() {
        s[k] = v.clone();
    }
    send(
        ws,
        json!({"type": "session.update", "event_id": event_id, "session": s}),
    )
    .await;
    next_event(ws).await
}

fn voice(v: Value) -> Value {
    json!({"audio": {"output": {"voice": v}}})
}

/// A user turn and a response with `create`'s parameters, to `response.done`.
async fn turn(ws: &mut Ws, create: Value) -> Vec<Value> {
    send(ws, user_text("hi")).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, create).await;
    events_until(ws, "response.done").await
}

fn error_of(events: &[Value]) -> &Value {
    events
        .iter()
        .find(|e| e["type"] == "error")
        .unwrap_or_else(|| panic!("no error in {events:?}"))
}

/// An lmgw audio row `narrator-tts` (alias `audio/narrator-tts`) with one
/// preset, `narrator`, and — when given — a hold fallback alias.
async fn local_row(state: &SharedState, fallback: Option<&str>) {
    store::insert_audio_model(
        &state.db,
        &NewAudioModel {
            model_id: "narrator-tts".into(),
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
            voice_presets: serde_json::Map::from_iter([(
                "narrator".into(),
                json!({"voice_id": "alba"}),
            )]),
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

async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Wait until the TTS alias has `n` rows.
async fn rows(state: &SharedState, n: usize) {
    for _ in 0..500 {
        if tts_rows(state).await.len() == n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {n} TTS rows");
}

#[tokio::test]
async fn under_the_gpu_hold_a_voice_is_taken_at_its_word_and_nothing_starts() {
    let (state, addr, chat, _tts) = speech_gateway(false, None, |s| {
        s.hold.active = true;
        s.realtime.warm_on_connect = false;
    })
    .await;
    local_row(&state, None).await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    next_event(&mut ws).await;
    // The local model, held, and a voice only its list could confirm: once
    // refused as `voice_not_found` after an admission the hold refused
    // (M1). Now it is taken provisionally, and nothing is started for it.
    let u = update(
        &mut ws,
        json!({"lmgw": {"tts_model": "audio/narrator-tts", "output_lead_ms": NO_WAIT},
               "output_modalities": ["audio"], "audio": {"output": {"voice": "javert"}}}),
        "u",
    )
    .await;
    assert_eq!(u["type"], "session.updated", "{u}");
    assert_eq!(u["session"]["lmgw"]["resolved"]["voice"], "javert");
    assert!(state.runtime().list().is_empty(), "nothing was started");
    // The response then says what really stands in its way: the hold.
    chat.push(Turn::text(&["Hi."]));
    let events = turn(&mut ws, json!({"type": "response.create"})).await;
    assert_eq!(code(error_of(&events)), "gpu_hold");
    assert_eq!(events.last().unwrap()["response"]["status"], "failed");
    assert!(state.runtime().list().is_empty());
}

#[tokio::test]
async fn a_voice_list_that_never_comes_holds_only_its_response_until_it_ends() {
    let (state, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    let _held = tts.hold_voices();
    // The handshake and the update ask the model nothing: a container that
    // is up and does not answer delays neither.
    let (mut ws, updated) = spoken_session(
        &addr,
        &[],
        NO_WAIT,
        json!({"audio": {"output": {"voice": "cosette"}}}),
    )
    .await;
    assert_eq!(updated["session"]["lmgw"]["resolved"]["voice"], "cosette");
    assert_eq!(tts.seen.voice_lists(), 0);

    // The first clause checks the provisional voice, and waits on the list.
    chat.push(Turn::text(&["Hi."]));
    send(&mut ws, user_text("hi")).await;
    let item = events_until(&mut ws, "conversation.item.done").await[0]["item"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    send(&mut ws, json!({"type": "response.create"})).await;
    assert_eq!(next_event(&mut ws).await["type"], "response.created");
    until("the list is asked for", || tts.seen.voice_lists() == 1).await;
    // The session is not frozen meanwhile.
    send(
        &mut ws,
        json!({"type": "conversation.item.retrieve", "item_id": item}),
    )
    .await;
    assert_eq!(
        next_event(&mut ws).await["type"],
        "conversation.item.retrieved"
    );
    // A cancel ends the wait at once, and the response's one row says so.
    send(&mut ws, json!({"type": "response.cancel"})).await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "cancelled");
    rows(&state, 1).await;
    assert_eq!(tts.seen.count(), 0, "nothing was synthesized");

    // The end of the session ends it too.
    chat.push(Turn::text(&["Hi."]));
    send(&mut ws, json!({"type": "response.create"})).await;
    until("asked again", || tts.seen.voice_lists() == 2).await;
    drop(ws);
    rows(&state, 2).await;
    let kinds: Vec<(Option<String>,)> =
        sqlx::query_as("SELECT error_kind FROM request_logs WHERE class = 'audio' ORDER BY id")
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert_eq!(
        kinds,
        [(Some("canceled".into()),), (Some("canceled".into()),)]
    );
}

#[tokio::test]
async fn a_fallback_speaks_a_voice_of_its_own() {
    let cloud = tts_fake(&[]).await;
    let (state, addr, chat, local) = speech_gateway(false, None, |s| {
        s.hold.active = true;
        s.realtime.warm_on_connect = false;
        s.realtime.voice_map.insert("boss".into(), "ash".into());
    })
    .await;
    add_cloud_tts_alias(&state, &cloud, "cloud-tts").await;
    local_row(&state, Some("cloud-tts")).await;
    let (mut ws, updated) = spoken_session(
        &addr,
        &[],
        NO_WAIT,
        json!({"lmgw": {"output_lead_ms": NO_WAIT, "tts_model": "audio/narrator-tts"}}),
    )
    .await;
    // For the local model, `marin` is realtime.default_voice's `alba`…
    assert_eq!(updated["session"]["lmgw"]["resolved"]["voice"], "alba");
    // …which a cloud fallback would refuse (M2): under the hold it speaks
    // the OpenAI voice the session asked for.
    chat.push(Turn::text(&["Hi."]));
    let events = turn(&mut ws, json!({"type": "response.create"})).await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    assert_eq!(cloud.seen.body(0)["voice"], "marin");
    assert_eq!(cloud.seen.body(0)["model"], "gpt-4o-mini-tts");
    assert_eq!(local.seen.count(), 0);
    // A name realtime.voice_map maps: the name it maps to.
    update(&mut ws, voice(json!("boss")), "boss").await;
    chat.push(Turn::text(&["Hi."]));
    let events = turn(&mut ws, json!({"type": "response.create"})).await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    assert_eq!(cloud.seen.body(1)["voice"], "ash");
    // A voice only the local model has: the fallback has none, and says so.
    update(&mut ws, voice(json!("cosette")), "c").await;
    chat.push(Turn::text(&["Hi."]));
    let events = turn(
        &mut ws,
        json!({"type": "response.create", "event_id": "r3"}),
    )
    .await;
    let e = error_of(&events);
    assert_eq!(code(e), "voice_not_configured", "{e}");
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'cloud-tts'"),
        "{e}"
    );
    assert_eq!(e["error"]["event_id"], "r3");
    assert_eq!(events.last().unwrap()["response"]["status"], "failed");
    assert_eq!(cloud.seen.count(), 2, "nothing sent it");
}

#[tokio::test]
async fn a_response_speaks_with_its_own_voice_in_the_session_s_format() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |s| {
        s.realtime.voice_map.insert("boss".into(), "cosette".into());
    })
    .await;
    let (mut ws, _) = spoken_session(&addr, &[], NO_WAIT, json!({})).await;
    // Its own voice, through the same chain, for that response only (m6).
    chat.push(Turn::text(&["Hi."]));
    let events = turn(
        &mut ws,
        json!({"type": "response.create",
               "response": {"audio": {"output": {"voice": "boss"}}}}),
    )
    .await;
    let done = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "completed");
    assert_eq!(events[0]["response"]["audio"]["output"]["voice"], "boss");
    assert_eq!(done["audio"]["output"]["voice"], "boss");
    assert_eq!(tts.seen.body(0)["voice"], "cosette");
    chat.push(Turn::text(&["Hi."]));
    let events = turn(&mut ws, json!({"type": "response.create"})).await;
    assert_eq!(
        events.last().unwrap()["response"]["audio"]["output"]["voice"],
        "marin"
    );
    assert_eq!(tts.seen.body(1)["voice"], "alba");

    // Refused before it starts: another format, a clip there is no library
    // for, a shape that is no voice.
    for (audio, want, param) in [
        (
            json!({"output": {"format": {"type": "audio/pcmu"}}}),
            "unsupported",
            "response.audio.output.format",
        ),
        (
            json!({"output": {"format": {"type": "audio/pcm", "rate": 16000}}}),
            "unsupported",
            "response.audio.output.format",
        ),
        (
            json!({"output": {"voice": {"id": "nope"}}}),
            "voice_not_found",
            "response.audio.output.voice",
        ),
        (
            json!({"output": {"voice": 5}}),
            "invalid_value",
            "response.audio",
        ),
    ] {
        send(
            &mut ws,
            json!({"type": "response.create", "event_id": "x", "response": {"audio": audio}}),
        )
        .await;
        let e = next_event(&mut ws).await;
        assert_eq!(e["type"], "error", "{e}");
        assert_eq!(code(&e), want, "{e}");
        assert_eq!(e["error"]["param"], param, "{e}");
        assert_eq!(e["error"]["event_id"], "x");
    }
    // The session's own format is fine to name.
    chat.push(Turn::text(&["Hi."]));
    let events = turn(
        &mut ws,
        json!({"type": "response.create", "response": {"audio": {"output": {
            "format": {"type": "audio/pcm", "rate": 24000}, "voice": "alloy"}}}}),
    )
    .await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    assert_eq!(tts.seen.body(2)["voice"], "alba");
}

#[tokio::test]
async fn a_response_s_own_voice_that_the_model_lacks_is_named_as_its_own() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |_| {}).await;
    let (mut ws, _) = spoken_session(&addr, &[], NO_WAIT, json!({})).await;
    // Provisional at the create — the list is unread — and refused at the
    // first clause, as the response's voice, not the session's (package B
    // review 6).
    chat.push(Turn::text(&["Hi."]));
    let events = turn(
        &mut ws,
        json!({"type": "response.create", "event_id": "own",
               "response": {"audio": {"output": {"voice": "zork"}}}}),
    )
    .await;
    let e = error_of(&events);
    assert_eq!(code(e), "voice_not_found", "{e}");
    assert_eq!(e["error"]["param"], "response.audio.output.voice", "{e}");
    assert_eq!(e["error"]["event_id"], "own");
    assert_eq!(events.last().unwrap()["response"]["status"], "failed");
    // The session's voice is not touched by it, and the list read there
    // verifies it: the next response speaks without reading it again.
    chat.push(Turn::text(&["Hi."]));
    let events = turn(&mut ws, json!({"type": "response.create"})).await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    assert_eq!(tts.seen.body(0)["voice"], "alba");
    assert_eq!(tts.seen.voice_lists(), 1);
}

#[tokio::test]
async fn a_clip_added_mid_session_is_seen_by_the_next_update() {
    let models = tempfile::tempdir().unwrap();
    std::fs::create_dir(models.path().join("voices")).unwrap();
    let dir = models.path().to_str().unwrap().to_string();
    let (state, addr, _chat, _tts) = speech_gateway(false, None, move |s| {
        s.audio.models_dir = dir;
        s.realtime.warm_on_connect = false;
    })
    .await;
    local_row(&state, None).await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    next_event(&mut ws).await;
    let u = update(
        &mut ws,
        json!({"lmgw": {"tts_model": "audio/narrator-tts"}}),
        "tts",
    )
    .await;
    assert_eq!(u["type"], "session.updated", "{u}");
    let e = update(&mut ws, voice(json!({"id": "late"})), "early").await;
    assert_eq!(code(&e), "voice_not_found", "{e}");
    // The owner records a clip while the session is open (review m9).
    std::fs::write(models.path().join("voices/late.wav"), b"RIFF").unwrap();
    let u = update(&mut ws, voice(json!({"id": "late"})), "late").await;
    assert_eq!(u["type"], "session.updated", "{u}");
    assert_eq!(u["session"]["lmgw"]["resolved"]["voice"], "late");
}
