//! A voice-design package against its row's task (audio-class gap 2):
//! Qwen3-TTS VoiceDesign runs only `vdes`. Saving it as `tts` is refused with
//! the fix spelled out; a row saved before that check is refused per request
//! before anything is started, and says why in its `/v1/models` notes. lmgw
//! never changes the task itself. The packages are synthetic.

use lmgw_core::store;
use serde_json::{json, Value};

use crate::support::audio_world::{tts_row, wav_bytes, world};
use crate::support::audiocpp_gguf;
use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_fakes::{
    add_chat_aliases, chat_fake, events_until, gpu_gateway, next_event, send, user_text, Turn,
};
use crate::support::realtime_tts::spoken_session;

async fn set(state: &lmgw_core::state::SharedState, args: Value) -> Result<Value, String> {
    let patch = lmgw_core::ops::patch_from_args(args.as_object().cloned())?;
    lmgw_core::ops::audio_model_set(state, patch).await
}

#[tokio::test]
async fn saving_a_package_under_a_task_its_variant_does_not_run_is_refused_with_the_fix() {
    let w = world().await;
    let root = w.models.path().join("design");
    std::fs::create_dir_all(&root).unwrap();
    audiocpp_gguf::qwen3(&root, "voice_design");

    let create = |task: &str| {
        json!({"action": "create", "model_id": "design", "family": "qwen3_tts",
               "path": "design", "task": task})
    };
    let err = set(&w.state, create("tts")).await.unwrap_err();
    assert!(
        err.contains("VoiceDesign") && err.contains("set the row's task to vdes"),
        "{err}"
    );
    assert!(w.state.snapshot().audio_models.is_empty(), "nothing saved");

    let made = set(&w.state, create("vdes")).await.unwrap();
    let id = made["id"].as_i64().unwrap();
    // Changing it back is refused the same way; enabling is never refused.
    let err = set(
        &w.state,
        json!({"action": "update", "id": id, "task": "tts"}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("set the row's task to vdes"), "{err}");
    set(&w.state, json!({"action": "disable", "id": id}))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_saved_mismatch_is_refused_before_anything_starts_and_noted() {
    let w = world().await;
    // Saved before the check existed: straight into the store.
    w.row("design", "qwen3_tts", |r| {
        audiocpp_gguf::qwen3(r, "voice_design")
    })
    .await;
    w.answer_wav().await;
    let resp = w
        .speak(json!({"model": "audio/design", "input": "Hello.",
                      "instructions": "a warm voice"}))
        .await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "task_mismatch", "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("set the row's task to vdes"));
    assert_eq!(w.runs(), 0, "nothing was started for it");
    assert!(w.sent().await.is_empty());

    let models: Value =
        w.gw.client()
            .get(format!("{}/v1/models", w.gw))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    let entry = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "audio/design")
        .unwrap();
    assert!(
        entry["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("400 task_mismatch")),
        "{entry}"
    );
}

#[tokio::test]
async fn a_vdes_row_publishes_the_speech_route_and_needs_its_description() {
    let w = world().await;
    w.row_with(
        "design",
        "qwen3_tts",
        |r| audiocpp_gguf::qwen3(r, "voice_design"),
        |row| {
            row.task = "vdes".into();
            row.default_request_options = json!({"instruct": "a calm, low voice"})
                .as_object()
                .cloned()
                .unwrap();
        },
    )
    .await;
    w.answer_wav().await;
    // The row's default description stands in for a request without one.
    let resp = w
        .speak(json!({"model": "audio/design", "input": "Hello."}))
        .await;
    assert_eq!(resp.status(), 200);

    let models: Value =
        w.gw.client()
            .get(format!("{}/v1/models", w.gw))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    let caps = &models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "audio/design")
        .unwrap()["capabilities"];
    assert_eq!(caps["endpoints"][0], "/v1/audio/speech");
    assert_eq!(caps["speech"]["instructions"], "voice_design");
    assert_eq!(caps["speech"]["instructions_required"], true);
}

/// A voice-design row without a description from any source — the
/// session's speech instructions, the owner's setting, the row's own default
/// — cannot speak a single clause. An audio response is refused
/// `instructions_required` before it is created (WP10 D5), naming
/// `session.lmgw.speech_instructions` like `voice_not_configured` names the
/// voice: nothing is started, nothing evicted for it, and the session goes
/// on. With the session's description, or the row's own, the same voice
/// speaks. MOSS-VoiceGen designs its voice from `instruction`
/// (`audio::families::undeclared_instructions`); its rows run `vdes`, which a
/// session takes as its TTS like a `tts` row.
#[tokio::test]
async fn a_realtime_voice_with_no_description_is_refused_before_admission() {
    let g = Gpu::new(24 * GIB, 2, 5).await;
    let chat = chat_fake().await;
    add_chat_aliases(&g.state, &chat).await;
    for (id, defaults) in [
        ("moss", json!({})),
        ("moss-described", json!({"instruct": "a calm, low voice"})),
    ] {
        let root = g.models_dir().join(id);
        std::fs::create_dir_all(&root).unwrap();
        audiocpp_gguf::moss_voicegen(&root);
        let mut row = tts_row(id, "moss_voicegen");
        row.task = "vdes".into();
        row.default_request_options = defaults.as_object().cloned().unwrap();
        store::insert_audio_model(&g.state.db, &row).await.unwrap();
    }
    // The session's voice, a clip of the voice library.
    let voices = g.models_dir().join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("alba.wav"), wav_bytes(24_000, 480)).unwrap();
    let models = g.models_dir().display().to_string();
    let addr = gpu_gateway(&g, |s| {
        s.audio.models_dir = models;
        s.realtime.tts_alias = "audio/moss".into();
        s.realtime.default_voice = "alba".into();
        s.realtime.warm_on_connect = false;
    })
    .await;

    let (mut ws, updated) = spoken_session(&addr, &[], 200, json!({})).await;
    let speech = &updated["session"]["lmgw"]["resolved"]["speech"];
    assert_eq!(speech["instructions"], "voice_design", "{speech}");
    assert_eq!(speech["text"], Value::Null);
    send(&mut ws, user_text("Hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let error = next_event(&mut ws).await;
    assert_eq!(error["type"], "error", "refused before response.created");
    assert_eq!(error["error"]["code"], "instructions_required", "{error}");
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert_eq!(error["error"]["param"], "session.lmgw.speech_instructions");
    // The session goes on, and a description of its own lets it speak.
    chat.push(Turn::text(&["Hello there."]));
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "lmgw": {"speech_instructions": "an old sailor, hoarse and slow"}}}),
    )
    .await;
    assert_eq!(next_event(&mut ws).await["type"], "session.updated");
    assert!(g.runs().is_empty(), "started: {:?}", g.runs());
    assert!(g.stops().is_empty(), "stopped: {:?}", g.stops());
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{events:?}"
    );
    assert_eq!(g.runs(), ["moss"]);
    assert_eq!(
        g.world().speech_bodies[0]["instructions"],
        "an old sailor, hoarse and slow"
    );

    // The row's own description: the voice speaks, and audio.cpp merges the
    // default into the request itself.
    chat.push(Turn::text(&["Hello again."]));
    let (mut ws, _) = spoken_session(
        &addr,
        &[],
        200,
        json!({"lmgw": {"output_lead_ms": 200, "tts_model": "audio/moss-described"}}),
    )
    .await;
    send(&mut ws, user_text("Hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{events:?}"
    );
    assert_eq!(g.runs(), ["moss", "moss-described"]);
    let w = g.world();
    assert_eq!(w.speeches, ["moss", "moss-described"]);
    assert!(w.speech_bodies[1].get("instructions").is_none());
}

/// R1's open-time refusal is the backstop for facts a response read before
/// the row changed (WP10 made D5's check before `response.created` the
/// first line): a voice-design row that describes itself passes the
/// response's snapshot, then loses its description — the owner clears its
/// default request options — while the chat model is still writing. The
/// response's TTS route opens at the first clause, judges the row as it is
/// now, and refuses it `instructions_required` before admission: nothing
/// is started, the response fails, its TTS row is written, and the session
/// goes on. No other realtime path reaches it: a fallback is never a local
/// row, so it never has a description to lack.
#[tokio::test]
async fn a_row_that_loses_its_description_mid_response_is_refused_when_its_route_opens() {
    let g = Gpu::new(24 * GIB, 1, 5).await;
    let chat = chat_fake().await;
    add_chat_aliases(&g.state, &chat).await;
    let root = g.models_dir().join("moss-described");
    std::fs::create_dir_all(&root).unwrap();
    audiocpp_gguf::moss_voicegen(&root);
    let mut row = tts_row("moss-described", "moss_voicegen");
    row.task = "vdes".into();
    row.default_request_options = json!({"instruct": "a calm, low voice"})
        .as_object()
        .cloned()
        .unwrap();
    let id = store::insert_audio_model(&g.state.db, &row).await.unwrap();
    let voices = g.models_dir().join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("alba.wav"), wav_bytes(24_000, 480)).unwrap();
    let models = g.models_dir().display().to_string();
    let addr = gpu_gateway(&g, |s| {
        s.audio.models_dir = models;
        s.realtime.tts_alias = "audio/moss-described".into();
        s.realtime.default_voice = "alba".into();
        s.realtime.warm_on_connect = false;
    })
    .await;

    let (mut ws, updated) = spoken_session(&addr, &[], 200, json!({})).await;
    let speech = &updated["session"]["lmgw"]["resolved"]["speech"];
    assert_eq!(speech["source"], "row", "{speech}");
    send(&mut ws, user_text("Hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    chat.push(Turn::Held(
        release.clone(),
        Box::new(Turn::text(&["Hello there."])),
    ));
    send(&mut ws, json!({"type": "response.create"})).await;
    assert_eq!(
        next_event(&mut ws).await["type"],
        "response.created",
        "the snapshot saw the row's description"
    );
    set(
        &g.state,
        json!({"action": "update", "id": id, "default_request_options": {}}),
    )
    .await
    .unwrap();
    assert!(g.state.snapshot().audio_models[0]
        .default_request_options
        .is_empty());
    release.notify_one();

    let events = events_until(&mut ws, "response.done").await;
    let error = events
        .iter()
        .find(|e| e["type"] == "error")
        .unwrap_or_else(|| panic!("an error event: {events:?}"));
    assert_eq!(error["error"]["code"], "instructions_required", "{error}");
    let done = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "failed", "{done}");
    assert_eq!(
        done["status_details"]["error"]["code"],
        "instructions_required"
    );
    assert!(g.runs().is_empty(), "started: {:?}", g.runs());
    assert!(g.stops().is_empty(), "stopped: {:?}", g.stops());
    let rows: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT status, error_kind FROM request_logs WHERE requested_alias = \
         'audio/moss-described' AND class = 'audio'",
    )
    .fetch_all(&g.state.db)
    .await
    .unwrap();
    assert_eq!(rows, [(400, Some("instructions_required".into()))]);

    // The session goes on: the next response is refused before it is
    // created, by the facts read again.
    send(
        &mut ws,
        json!({"type": "response.create", "event_id": "r2"}),
    )
    .await;
    let e = next_event(&mut ws).await;
    assert_eq!(e["type"], "error", "{e}");
    assert_eq!(e["error"]["code"], "instructions_required");
    assert_eq!(e["error"]["event_id"], "r2");
}
