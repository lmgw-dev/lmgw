//! The TTS alias and the voice (realtime design §5.3, §5.4, §10.2, §10.3):
//! the voice chain and its echoes, what an audio response says when there
//! is nothing to speak with, and the key's say over the TTS alias.

use lmgw_core::config::{KeyPolicy, ScopeMode};
use lmgw_core::store::{self, NewAlias, NewAudioModel};
use serde_json::{json, Value};

use crate::support::realtime_fakes::{
    events_until, next_event, open, send, types, user_text, Step, Turn, Ws, KEY,
};
use crate::support::realtime_tts::{speech_gateway, spoken_session, tts_rows, TTS_ALIAS};

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

#[tokio::test]
async fn the_voice_chain_resolves_and_echoes_every_step() {
    let (_s, addr, chat, tts) = speech_gateway(false, None, |s| {
        s.realtime.voice_map.insert("boss".into(), "cosette".into());
    })
    .await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    let created = next_event(&mut ws).await;
    // No voice named: `marin`, which the model lacks → realtime.default_voice.
    assert_eq!(created["session"]["audio"]["output"]["voice"], "marin");
    assert_eq!(created["session"]["lmgw"]["resolved"]["voice"], "alba");
    assert_eq!(created["session"]["lmgw"]["resolved"]["tts"], TTS_ALIAS);

    for (asked, speaks) in [
        // 1. a voice of the model's own list;
        ("cosette", "cosette"),
        // 2. realtime.voice_map;
        ("boss", "cosette"),
        // 3. an OpenAI name → the setting.
        ("alloy", "alba"),
    ] {
        let u = update(&mut ws, voice(json!(asked)), asked).await;
        assert_eq!(u["type"], "session.updated", "{u}");
        assert_eq!(u["session"]["audio"]["output"]["voice"], asked);
        assert_eq!(u["session"]["lmgw"]["resolved"]["voice"], speaks, "{asked}");
    }
    // 5. A name nothing known without asking the model shows: accepted
    // provisionally — resolving a voice never asks the model (WP3 review
    // M1). An {id} with no voice library is refused at once.
    let u = update(&mut ws, voice(json!("zork")), "z").await;
    assert_eq!(u["type"], "session.updated", "{u}");
    assert_eq!(u["session"]["lmgw"]["resolved"]["voice"], "zork");
    let e = update(&mut ws, voice(json!({"id": "a-clip"})), "id").await;
    assert_eq!(code(&e), "voice_not_found", "{e}");
    assert_eq!(tts.seen.voice_lists(), 0, "nothing was asked of the model");

    // The first spoken clause checks it on the route the response holds: a
    // voice the model does not have fails that response, never reaching the
    // engine, and the session goes on.
    update(
        &mut ws,
        json!({"output_modalities": ["audio"], "lmgw": {"output_lead_ms": 60_000}}),
        "audio",
    )
    .await;
    chat.push(Turn::text(&["Hi."]));
    let events = turn(
        &mut ws,
        json!({"type": "response.create", "event_id": "r1"}),
    )
    .await;
    let error = events
        .iter()
        .find(|e| e["type"] == "error")
        .unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!(code(error), "voice_not_found", "{error}");
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert_eq!(error["error"]["param"], "session.audio.output.voice");
    assert_eq!(error["error"]["event_id"], "r1");
    assert_eq!(events.last().unwrap()["response"]["status"], "failed");
    assert_eq!(tts.seen.count(), 0, "the engine never got the clause");
    assert_eq!(tts.seen.voice_lists(), 1);

    // The list read there is the session's now: a name it lacks is refused
    // by the update itself, which changes nothing.
    let e = update(&mut ws, voice(json!("zork2")), "z2").await;
    assert_eq!(code(&e), "voice_not_found", "{e}");
    assert_eq!(e["error"]["param"], "session.audio.output.voice");
    assert_eq!(e["error"]["event_id"], "z2");
    let u = update(&mut ws, json!({"instructions": "Be brief."}), "same").await;
    assert_eq!(u["session"]["audio"]["output"]["voice"], "zork");
    // The session's own voice, which that list does not show, is no longer
    // provisional (package B review 5) — but not for good either (B2 review
    // 5): the next audio response reads the model's list again at its first
    // clause, and fails there while the model still lacks it.
    let events = turn(
        &mut ws,
        json!({"type": "response.create", "event_id": "r2"}),
    )
    .await;
    let error = events
        .iter()
        .find(|e| e["type"] == "error")
        .unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!(code(error), "voice_not_found", "{error}");
    assert_eq!(error["error"]["param"], "session.audio.output.voice");
    assert_eq!(error["error"]["event_id"], "r2");
    assert_eq!(events.last().unwrap()["response"]["status"], "failed");
    assert_eq!(tts.seen.voice_lists(), 2, "read again");
    assert_eq!(tts.seen.count(), 0);

    // Speaking with a mapped name: the engine gets the voice it maps to,
    // the response echoes the name asked for — known from the list already
    // read, so it is not read again.
    update(&mut ws, voice(json!("boss")), "boss2").await;
    chat.push(Turn::text(&["Hi."]));
    let events = turn(&mut ws, json!({"type": "response.create"})).await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    assert_eq!(
        events.last().unwrap()["response"]["audio"]["output"]["voice"],
        "boss"
    );
    assert_eq!(tts.seen.body(0)["voice"], "cosette");
    assert_eq!(tts.seen.voice_lists(), 2, "not read again");
}

/// An lmgw audio row speaking `narrator` by default, with the voice library
/// in a models dir holding one clip, `clip`.
async fn local_row(state: &lmgw_core::state::SharedState) {
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
            default_voice_preset: Some(Value::String("narrator".into())),
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

#[tokio::test]
async fn a_library_clip_by_id_and_the_rows_default_preset() {
    let models = tempfile::tempdir().unwrap();
    std::fs::create_dir(models.path().join("voices")).unwrap();
    std::fs::write(models.path().join("voices/clip.wav"), b"RIFF").unwrap();
    let dir = models.path().to_str().unwrap().to_string();
    let (state, addr, chat, _tts) = speech_gateway(false, None, move |s| {
        s.audio.models_dir = dir;
        s.realtime.default_voice = String::new();
        // Nothing of the local row may start in this test.
        s.realtime.warm_on_connect = false;
    })
    .await;
    local_row(&state).await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    next_event(&mut ws).await;

    // The session names its own TTS model; `marin` → the row's default
    // preset, the setting being empty.
    let u = update(
        &mut ws,
        json!({"lmgw": {"tts_model": "audio/narrator-tts"}}),
        "tts",
    )
    .await;
    assert_eq!(u["type"], "session.updated", "{u}");
    assert_eq!(u["session"]["lmgw"]["tts_model"], "audio/narrator-tts");
    assert_eq!(
        u["session"]["lmgw"]["resolved"]["tts"],
        "audio/narrator-tts"
    );
    assert_eq!(u["session"]["lmgw"]["resolved"]["voice"], "narrator");

    // 4. A clip of the voice library, by id: the session keeps the object.
    let u = update(&mut ws, voice(json!({"id": "clip"})), "clip").await;
    assert_eq!(
        u["session"]["audio"]["output"]["voice"],
        json!({"id": "clip"})
    );
    assert_eq!(u["session"]["lmgw"]["resolved"]["voice"], "clip");
    let e = update(&mut ws, voice(json!({"id": "nope"})), "nope").await;
    assert_eq!(code(&e), "voice_not_found", "{e}");

    // The response object says the voice as a string — `@openai/agents`'
    // schema wants one. (A call only: nothing is spoken, nothing started.)
    chat.push(Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id: Some("call_1"),
            name: "f",
        },
        Step::CallArgs {
            index: 0,
            args: "{}",
        },
        Step::Finish("tool_calls"),
    ]));
    let events = turn(
        &mut ws,
        json!({"type": "response.create", "response": {"output_modalities": ["audio"]}}),
    )
    .await;
    for r in [&events[0]["response"], &events.last().unwrap()["response"]] {
        assert_eq!(r["audio"]["output"]["voice"], "clip", "{r}");
    }
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
}

#[tokio::test]
async fn an_audio_response_without_a_voice_or_a_tts_alias_never_starts() {
    // Nothing to speak with: no default voice, and the remote model has no
    // default preset.
    let (_s, addr, chat, tts) = speech_gateway(false, None, |s| {
        s.realtime.default_voice = String::new();
    })
    .await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    let created = next_event(&mut ws).await;
    assert_eq!(created["session"]["lmgw"]["resolved"]["voice"], Value::Null);
    let refused = |want: &'static str, param: &'static str| {
        move |events: Vec<Value>| {
            assert_eq!(types(&events)[..1], ["error"], "{events:?}");
            assert_eq!(code(&events[0]), want);
            assert_eq!(events[0]["error"]["param"], param);
            assert_eq!(events[0]["error"]["event_id"], "a");
        }
    };
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create", "event_id": "a"})).await;
    refused("voice_not_configured", "session.audio.output.voice")(vec![next_event(&mut ws).await]);
    // Text still works.
    chat.push(Turn::text(&["Text."]));
    send(
        &mut ws,
        json!({"type": "response.create", "response": {"output_modalities": ["text"]}}),
    )
    .await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(events[0]["type"], "response.created", "{events:?}");
    assert_eq!(tts.seen.count(), 0, "the engine was never called voiceless");

    // No TTS alias at all.
    let (_s, addr, chat, _tts) = speech_gateway(false, None, |s| {
        s.realtime.tts_alias = String::new();
    })
    .await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    let created = next_event(&mut ws).await;
    assert_eq!(created["session"]["lmgw"]["resolved"]["tts"], Value::Null);
    send(&mut ws, user_text("hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create", "event_id": "a"})).await;
    refused("tts_not_configured", "session.lmgw.tts_model")(vec![next_event(&mut ws).await]);
    chat.push(Turn::text(&["Text."]));
    send(
        &mut ws,
        json!({"type": "response.create", "response": {"output_modalities": ["text"]}}),
    )
    .await;
    let events = events_until(&mut ws, "response.done").await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    // A name that is no TTS alias, in the session's own knob.
    let e = update(&mut ws, json!({"lmgw": {"tts_model": "chatty"}}), "c").await;
    assert_eq!(e["type"], "error", "{e}");
    assert_eq!(e["error"]["param"], "session.lmgw.tts_model");
}

#[tokio::test]
async fn the_tts_alias_is_the_key_s_to_use_before_the_101_and_before_each_voice() {
    let bearer = format!("Bearer {KEY}");
    let auth = [("authorization", bearer.as_str())];
    // A key that may not use the TTS alias cannot open a session (§10.2).
    let narrow = KeyPolicy {
        scope_mode: ScopeMode::Allow,
        scope_patterns: "chatty".into(),
        ..Default::default()
    };
    let (_s, addr, _c, _t) = speech_gateway(true, Some(narrow), |_| {}).await;
    let refused = tokio_tungstenite::connect_async({
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = format!("ws://{addr}/v1/realtime?model=chatty")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", bearer.parse().unwrap());
        req
    })
    .await;
    match refused {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), 403)
        }
        other => panic!("expected a 403 before the upgrade, got {other:?}"),
    }

    // One that may: its rows carry the key; then it is narrowed mid-session.
    let wide = KeyPolicy {
        scope_mode: ScopeMode::Allow,
        scope_patterns: format!("chatty\n{TTS_ALIAS}"),
        ..Default::default()
    };
    let (state, addr, chat, tts) = speech_gateway(true, Some(wide), |_| {}).await;
    let (mut ws, _) = spoken_session(&addr, &auth, 60_000, json!({})).await;
    chat.push(Turn::text(&["Hi."]));
    let events = turn(&mut ws, json!({"type": "response.create"})).await;
    assert_eq!(events.last().unwrap()["response"]["status"], "completed");
    assert_eq!(tts_rows(&state).await, [(200, Some("voice".into()))]);
    // Every row of the session — chat and TTS — is labelled realtime (§11).
    let labels: Vec<(String,)> = sqlx::query_as("SELECT DISTINCT ingress_proto FROM request_logs")
        .fetch_all(&state.db)
        .await
        .unwrap();
    assert_eq!(labels, [("realtime".to_string(),)]);

    // A TTS alias the key may not use, named by the session: refused (§10.3).
    let up: (i64,) = sqlx::query_as("SELECT upstream_id FROM models WHERE alias = ?1")
        .bind(TTS_ALIAS)
        .fetch_one(&state.db)
        .await
        .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "speak-too".into(),
            upstream_id: up.0,
            upstream_model_id: "other-voice".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({ "capabilities": {
                "task": "tts", "endpoints": ["/v1/audio/speech"], "source": "owner"
            } })),
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let e = update(&mut ws, json!({"lmgw": {"tts_model": "speak-too"}}), "t").await;
    assert_eq!(code(&e), "key_scope", "{e}");
    assert_eq!(e["error"]["param"], "session.lmgw.tts_model");

    // Narrowed while the session is open: the next answer's first clause
    // is refused, and nothing is synthesized.
    sqlx::query("UPDATE api_keys SET scope_patterns = 'chatty' WHERE name = 'voice'")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    chat.push(Turn::text(&["Again."]));
    let events = turn(
        &mut ws,
        json!({"type": "response.create", "event_id": "r2"}),
    )
    .await;
    let error = events
        .iter()
        .find(|e| e["type"] == "error")
        .unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!(code(error), "key_scope");
    assert_eq!(error["error"]["event_id"], "r2");
    assert_eq!(events.last().unwrap()["response"]["status"], "failed");
    assert_eq!(tts.seen.count(), 1, "the refused clause was never sent");
    let (refusals,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM request_logs WHERE error_kind = 'key_scope' AND requested_alias = ?1",
    )
    .bind(TTS_ALIAS)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(refusals, 1, "the refusal is traffic: it has its row");
}

#[tokio::test]
async fn a_voice_the_owner_fixes_mid_session_speaks_at_the_next_response() {
    // B2 review 4 and 5: `boss` maps to a voice the model lacks. Once a
    // response has read the list that is the owner's misconfiguration —
    // `voice_not_configured`, said by audio responses only, never a refused
    // update — and once the owner fixes the map the next response speaks,
    // with no update from the client.
    let (state, addr, chat, tts) = speech_gateway(false, None, |s| {
        s.realtime.voice_map.insert("boss".into(), "phantom".into());
    })
    .await;
    let mut ws = open(&addr, "/v1/realtime?model=chatty", &[]).await;
    next_event(&mut ws).await;
    let u = update(
        &mut ws,
        json!({"output_modalities": ["audio"], "lmgw": {"output_lead_ms": 60_000},
               "audio": {"output": {"voice": "boss"}}}),
        "boss",
    )
    .await;
    assert_eq!(u["type"], "session.updated", "{u}");
    chat.push(Turn::text(&["Hi."]));
    let events = turn(&mut ws, json!({"type": "response.create"})).await;
    let error = events
        .iter()
        .find(|e| e["type"] == "error")
        .unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!(code(error), "voice_not_configured", "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("fix the setting realtime.voice_map"),
        "{error}"
    );
    // The list is read: the misconfiguration does not refuse the client's
    // updates — a text session never trips over it.
    let u = update(&mut ws, json!({"output_modalities": ["text"]}), "text").await;
    assert_eq!(u["type"], "session.updated", "{u}");
    let u = update(&mut ws, json!({"output_modalities": ["audio"]}), "audio").await;
    assert_eq!(u["type"], "session.updated", "{u}");

    // The owner fixes the map.
    let mut settings = state.snapshot().settings.clone();
    settings
        .realtime
        .voice_map
        .insert("boss".into(), "cosette".into());
    lmgw_core::store::save_settings(&state.db, &settings)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    chat.push(Turn::text(&["Hi again."]));
    let events = turn(&mut ws, json!({"type": "response.create"})).await;
    assert_eq!(
        events.last().unwrap()["response"]["status"],
        "completed",
        "{events:?}"
    );
    assert_eq!(tts.seen.count(), 1);
    assert_eq!(tts.seen.body(0)["voice"], "cosette");
}

/// A voice the local TTS row's package ships (audio-class gap 1): Qwen3's
/// `ryan`, named `Ryan` by the session, is known from the package — not
/// accepted provisionally and then refused at the first clause because the
/// engine's own list (`["alba"]` here, the fake container's) lacks it — and
/// sent in the package's spelling, with the session's language in Qwen3's
/// vocabulary (gap 7).
#[tokio::test]
async fn a_shipped_voice_of_a_local_row_is_known_by_name_in_any_case() {
    use crate::support::gpu_world::{Gpu, GIB};
    use crate::support::realtime_fakes::{add_chat_aliases, chat_fake, gpu_gateway};
    use crate::support::realtime_tts::spoken_session;

    let g = Gpu::new(24 * GIB, 3, 5).await;
    let chat = chat_fake().await;
    add_chat_aliases(&g.state, &chat).await;
    let root = g.models_dir().join("qwen3");
    std::fs::create_dir_all(&root).unwrap();
    crate::support::audiocpp_gguf::qwen3(&root, "custom_voice");
    store::insert_audio_model(
        &g.state.db,
        &NewAudioModel {
            model_id: "qwen3".into(),
            family: "qwen3_tts".into(),
            path: "qwen3".into(),
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
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    let models = g.models_dir().display().to_string();
    let addr = gpu_gateway(&g, |s| {
        s.audio.models_dir = models;
        s.realtime.tts_alias = "audio/qwen3".into();
        s.realtime.default_voice = String::new();
        s.realtime.warm_on_connect = false;
    })
    .await;
    // The session's language (audio-class gap 7) goes along, in the
    // package's vocabulary: Qwen3 refuses `de`.
    let mut extra = voice(json!("Ryan"));
    extra["audio"]["input"] = json!({"transcription": {"language": "de-DE"}});
    let (mut ws, updated) = spoken_session(&addr, &[], 0, extra).await;
    assert_eq!(updated["session"]["lmgw"]["resolved"]["voice"], "Ryan");

    chat.push(Turn::text(&["Hallo du. "]));
    send(&mut ws, user_text("Hi")).await;
    events_until(&mut ws, "conversation.item.done").await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = events_until(&mut ws, "response.done").await;
    let done = &events.last().unwrap()["response"];
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(g.world().speeches, vec!["qwen3".to_string()]);
    let body = g.world().speech_bodies[0].clone();
    assert_eq!(body["voice"], "ryan", "the package's spelling: {body}");
    assert_eq!(body["language"], "german", "{body}");
}
