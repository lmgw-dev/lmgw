//! Chat voice WP1 (chat-voice design §2, §3): the Voice settings group on
//! both save paths and the self-admin schema, a thread's voice overrides and
//! what they resolve to, a folder's voice defaults, temporary threads and
//! Keep, the audio-attachment transcript following the Chat's ASR chain, and
//! spoken turns in storage, export and search. Mock upstreams only.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::ops::settings_set;
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::MockServer;

use crate::chat_attach_kinds::{
    chat_mock, new_thread, patch, setup, stt_mock, transcriptions, upload_ok, wav,
};
use crate::common::Gw;

/// The attachment harness (`plain`, `hearing`, … and `my-asr`), plus a
/// second speech-to-text alias `other-asr` and a text-to-speech alias
/// `my-tts` on a second speech upstream.
pub(crate) async fn gateway(
    chat: &MockServer,
    stt: &MockServer,
    stt2: &MockServer,
) -> (SharedState, Gw) {
    let (state, gw) = setup(chat, stt).await;
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "speech-up".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::AudioCpp,
            base_url: format!("{}/v1", stt2.uri()),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    for (alias, task, endpoint) in [
        ("other-asr", "asr", "/v1/audio/transcriptions"),
        ("my-tts", "tts", "/v1/audio/speech"),
    ] {
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: alias.into(),
                upstream_id: up,
                upstream_model_id: format!("{alias}-model"),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: Some(json!({ "capabilities": {
                    "task": task, "endpoints": [endpoint], "source": "owner"
                } })),
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    (state, gw)
}

pub(crate) async fn mocks() -> (MockServer, MockServer, MockServer) {
    (
        chat_mock().await,
        stt_mock("from the settings alias").await,
        stt_mock("from the thread's alias").await,
    )
}

pub(crate) async fn get_json(gw: &Gw, path: &str) -> Value {
    gw.client()
        .get(format!("{gw}{path}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

pub(crate) async fn post(gw: &Gw, path: &str, body: Value) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

pub(crate) async fn thread(gw: &Gw, tid: i64) -> Value {
    get_json(gw, &format!("/chat/api/threads/{tid}")).await["thread"].clone()
}

pub(crate) async fn set_voice(gw: &Gw, tid: i64, voice: Value) -> (u16, Value) {
    post(
        gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "voice": voice }),
    )
    .await
}

// -- the Voice settings group ------------------------------------------------

#[tokio::test]
async fn the_voice_keys_have_defaults_and_save_on_both_paths() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;

    let s = get_json(&gw, "/api/settings-full").await;
    assert_eq!(s["chat_tts_alias"], "");
    assert_eq!(s["chat_voice"], "");
    assert_eq!(s["chat_speech_style"], "");
    assert_eq!(s["chat_voice_language"], "");
    assert_eq!(s["chat_read_aloud"], false);
    assert_eq!(s["chat_turn_detection"], "semantic_vad");

    // The self-admin path, read back through both reads.
    let res = settings_set(
        &state,
        patch(json!({
            "chat_tts_alias": " my-tts ",
            "chat_voice": " alba ",
            "chat_speech_style": "calm",
            "chat_voice_language": "DE",
            "chat_read_aloud": true,
            "chat_turn_detection": "push_to_talk",
        })),
    )
    .await
    .unwrap();
    for key in ["chat_tts_alias", "chat_voice", "chat_turn_detection"] {
        assert!(
            res["changed"].as_array().unwrap().contains(&json!(key)),
            "{res}"
        );
    }
    let cur = state.snapshot().settings.clone();
    assert_eq!(
        (
            cur.chat_tts_alias.as_str(),
            cur.chat_voice.as_str(),
            cur.chat_speech_style.as_str(),
            cur.chat_voice_language.as_str(),
            cur.chat_read_aloud,
            cur.chat_turn_detection.as_str(),
        ),
        ("my-tts", "alba", "calm", "de", true, "push_to_talk")
    );
    let read = lmgw_core::ops::settings(&state).await.unwrap();
    assert_eq!(read["chat_tts_alias"], "my-tts");
    assert_eq!(read["chat_turn_detection"], "push_to_talk");
    let full = get_json(&gw, "/api/settings-full").await;
    assert_eq!(full["chat_voice_language"], "de");
    assert_eq!(full["chat_read_aloud"], true);

    // Refusals name the reason; nothing changes.
    for (bad, word) in [
        (json!({ "chat_tts_alias": "my-asr" }), "text-to-speech"),
        (json!({ "chat_tts_alias": "nope" }), "does not resolve"),
        (json!({ "chat_voice_language": "deu" }), "ISO 639-1"),
        (json!({ "chat_turn_detection": "auto" }), "semantic_vad"),
    ] {
        let e = settings_set(&state, patch(bad.clone())).await.unwrap_err();
        assert!(e.contains(word), "{bad}: {e}");
    }
    assert_eq!(state.snapshot().settings.chat_tts_alias, "my-tts");
    // Empty is never refused: it falls through to realtime's setting.
    settings_set(
        &state,
        patch(json!({ "chat_tts_alias": "", "chat_voice_language": "" })),
    )
    .await
    .unwrap();
    assert_eq!(state.snapshot().settings.chat_tts_alias, "");

    // The dashboard path takes and refuses the same.
    let (status, res) = post(
        &gw,
        "/api/op/settings_set_full",
        json!({ "chat_tts_alias": "my-tts", "chat_turn_detection": "server_vad",
                "chat_read_aloud": false }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let cur = state.snapshot().settings.clone();
    assert_eq!(
        (
            cur.chat_tts_alias.as_str(),
            cur.chat_turn_detection.as_str(),
            cur.chat_read_aloud
        ),
        ("my-tts", "server_vad", false)
    );
    for bad in [
        json!({ "chat_tts_alias": "my-chat-nope" }),
        json!({ "chat_tts_alias": "other-asr" }),
        json!({ "chat_voice_language": "english" }),
        json!({ "chat_turn_detection": "vad" }),
    ] {
        let (status, res) = post(&gw, "/api/op/settings_set_full", bad.clone()).await;
        assert_eq!(status, 400, "{bad} -> {res}");
    }
}

#[test]
fn the_settings_tool_lists_the_voice_keys() {
    let (tool, _) = lmgw_core::mcp::selfadmin::full_catalog()
        .into_iter()
        .find(|(t, _)| t["name"] == "lmgw__settings_set")
        .expect("lmgw__settings_set");
    let props = &tool["inputSchema"]["properties"];
    for (key, ty) in [
        ("chat_tts_alias", "string"),
        ("chat_voice", "string"),
        ("chat_speech_style", "string"),
        ("chat_voice_language", "string"),
        ("chat_voice_reply_language", "string"),
        ("chat_read_aloud", "boolean"),
        ("chat_turn_detection", "string"),
    ] {
        assert_eq!(props[key]["type"], ty, "{key}: {}", props[key]);
    }
    assert_eq!(
        props["chat_turn_detection"]["enum"],
        json!(["semantic_vad", "server_vad", "push_to_talk"])
    );
    // The changed chain is described: the thread's override, realtime's
    // fallback, and no longer "Send is then blocked" for every thread.
    let stt = props["chat_stt_alias"]["description"].as_str().unwrap();
    assert!(stt.contains("realtime.asr_alias"), "{stt}");
    assert!(stt.contains("thread"), "{stt}");
}

// -- a thread's overrides ----------------------------------------------------

#[tokio::test]
async fn a_thread_override_wins_and_the_resolution_names_each_source() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    settings_set(
        &state,
        patch(json!({
            "chat_stt_alias": "my-asr",
            "chat_voice": "alba",
            "realtime": { "tts_alias": "my-tts", "speech_instructions": "warm" },
        })),
    )
    .await
    .unwrap();
    let tid = new_thread(&gw, "plain", false).await;

    let t = thread(&gw, tid).await;
    assert_eq!(t["voice"], json!({}));
    let r = &t["voice_resolved"];
    assert_eq!(r["asr"]["alias"], "my-asr");
    assert_eq!(r["asr"]["source"], "chat");
    assert_eq!(r["asr"]["local"], true, "an audio.cpp upstream: {r}");
    assert_eq!(r["tts"]["alias"], "my-tts");
    assert_eq!(r["tts"]["source"], "realtime");
    // `chat_voice` was chosen for the Chat's model, which is realtime's here.
    assert_eq!(
        r["voice"],
        json!({ "name": "alba", "source": "chat", "inherits": null, "note": null })
    );
    assert_eq!(
        r["speech_style"],
        json!({ "text": "warm", "source": "realtime" })
    );
    assert_eq!(
        r["turn_detection"],
        json!({ "value": "semantic_vad", "source": "chat" })
    );
    assert_eq!(r["read_aloud"], json!({ "value": false, "source": "chat" }));
    assert_eq!(r["language"], json!({ "value": null, "source": null }));
    assert_eq!(
        r["reply_language"],
        json!({ "value": null, "source": null })
    );
    assert_eq!(r["problems"], json!([]));
    assert_eq!(r["realtime"]["ok"], true);

    // The override, as a whole object; the answer carries the resolution.
    let (status, res) = set_voice(
        &gw,
        tid,
        json!({
            "asr_alias": "other-asr", "voice": "own", "speech_style": "",
            "language": "EN", "read_aloud": true, "turn_detection": "push_to_talk",
            "seed": 1234567,
        }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["voice_resolved"]["asr"]["alias"], "other-asr");
    let t = thread(&gw, tid).await;
    assert_eq!(t["voice"]["language"], "en", "case folded: {t}");
    let r = &t["voice_resolved"];
    assert_eq!(r["asr"]["source"], "thread");
    assert_eq!(r["tts"]["source"], "realtime", "not overridden: inherited");
    assert_eq!(r["voice"]["name"], "own");
    assert_eq!(r["voice"]["source"], "thread");
    assert_eq!(r["speech_style"], json!({ "text": "", "source": "thread" }));
    assert_eq!(r["language"], json!({ "value": "en", "source": "thread" }));
    assert_eq!(
        r["read_aloud"],
        json!({ "value": true, "source": "thread" })
    );
    assert_eq!(
        r["turn_detection"],
        json!({ "value": "push_to_talk", "source": "thread" })
    );
    assert_eq!(r["seed"], 1234567);

    // Strict input: each refusal names the field, and nothing is written.
    for (bad, word) in [
        (json!({ "tts": "my-tts" }), "tts"),
        (json!({ "language": "english" }), "ISO 639-1"),
        (json!({ "turn_detection": "auto" }), "auto"),
        (json!({ "asr_alias": "my-tts" }), "asr"),
        (json!({ "tts_alias": "my-asr" }), "text-to-speech"),
        (json!({ "tts_alias": "gone" }), "does not resolve"),
        (json!("loud"), "object"),
    ] {
        let (status, res) = set_voice(&gw, tid, bad.clone()).await;
        assert_eq!(status, 400, "{bad} -> {res}");
        assert_eq!(res["code"], "bad_request");
        assert!(
            res["message"].as_str().unwrap().contains(word),
            "{bad}: {res}"
        );
    }
    assert_eq!(thread(&gw, tid).await["voice"]["asr_alias"], "other-asr");

    // A form that never showed the seed does not erase it; null does.
    let (status, _) = set_voice(&gw, tid, json!({ "tts_alias": "my-tts" })).await;
    assert_eq!(status, 200);
    let t = thread(&gw, tid).await;
    assert_eq!(
        t["voice"],
        json!({ "tts_alias": "my-tts", "seed": 1234567 }),
        "the whole object replaced the overrides; the seed stayed"
    );
    assert_eq!(t["voice_resolved"]["tts"]["source"], "thread");
    let (status, _) = set_voice(&gw, tid, Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(thread(&gw, tid).await["voice"], json!({ "seed": 1234567 }));
    let (status, _) = set_voice(&gw, tid, json!({ "seed": null })).await;
    assert_eq!(status, 200);
    assert_eq!(thread(&gw, tid).await["voice"], json!({}));

    // A settings patch without `voice` leaves it alone.
    set_voice(&gw, tid, json!({ "voice": "kept" })).await;
    let (status, _) = post(
        &gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "temperature": 0.5 }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(thread(&gw, tid).await["voice"]["voice"], "kept");
}

#[tokio::test]
async fn the_resolution_names_missing_and_vanished_models_and_admin_chat() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    let tid = new_thread(&gw, "plain", false).await;
    let r = thread(&gw, tid).await["voice_resolved"].clone();
    let codes: Vec<(&str, &str)> = r["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["stage"].as_str().unwrap(), p["code"].as_str().unwrap()))
        .collect();
    assert_eq!(
        codes,
        vec![("asr", "not_configured"), ("tts", "not_configured")]
    );

    // A thread override whose alias went away since.
    set_voice(&gw, tid, json!({ "tts_alias": "my-tts" })).await;
    sqlx::query("UPDATE models SET enabled = 0 WHERE alias = 'my-tts'")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let r = thread(&gw, tid).await["voice_resolved"].clone();
    let tts = r["problems"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["stage"] == "tts")
        .unwrap()
        .clone();
    assert_eq!(tts["code"], "unresolved", "{r}");
    assert!(tts["message"].as_str().unwrap().contains("this thread"));
    // Saving the rest is not blocked by the vanished alias it already had.
    let (status, res) = set_voice(
        &gw,
        tid,
        json!({ "tts_alias": "my-tts", "read_aloud": true }),
    )
    .await;
    assert_eq!(status, 200, "{res}");

    // Admin Chat: realtime mode refused, with its reason.
    let admin: Value = gw
        .client()
        .post(format!("{gw}/chat/api/threads"))
        .json(&json!({ "model_alias": "plain", "kind": "admin" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rt = &admin["voice_resolved"]["realtime"];
    assert_eq!(rt["ok"], false, "{admin}");
    assert_eq!(rt["code"], "chat_thread_admin");
    assert_eq!(rt["reason"], "Voice mode is not available in Admin Chat");
}

// -- folders -----------------------------------------------------------------

#[tokio::test]
async fn a_folders_voice_defaults_are_copied_into_a_new_thread() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    let (status, folder) = post(
        &gw,
        "/chat/api/folders",
        json!({ "name": "Voice", "defaults": {
            "temperature": 0.2,
            "voice": { "tts_alias": "my-tts", "read_aloud": true, "language": "DE" }
        } }),
    )
    .await;
    assert_eq!(status, 200, "{folder}");
    assert_eq!(folder["defaults"]["voice"]["language"], "de");
    let fid = folder["id"].as_i64().unwrap();

    // Strict, and the aliases checked, as a thread's own.
    for bad in [
        json!({ "voice": { "tts": "x" } }),
        json!({ "voice": { "tts_alias": "my-asr" } }),
        json!({ "voice": { "language": "deu" } }),
    ] {
        let (status, res) = post(
            &gw,
            &format!("/chat/api/folders/{fid}"),
            json!({ "defaults": bad.clone() }),
        )
        .await;
        assert_eq!(status, 400, "{bad} -> {res}");
    }

    let (status, t) = post(
        &gw,
        "/chat/api/threads",
        json!({ "model_alias": "plain", "folder_id": fid }),
    )
    .await;
    assert_eq!(status, 200, "{t}");
    assert_eq!(
        t["voice"],
        json!({ "tts_alias": "my-tts", "read_aloud": true, "language": "de" })
    );
    assert_eq!(t["voice_resolved"]["tts"]["source"], "thread");
    assert_eq!(t["temperature"], 0.2);

    // A defaults row a newer build wrote, with a key inside `voice` this
    // build does not know: the folder keeps every default.
    sqlx::query("UPDATE chat_folders SET defaults = ?1 WHERE id = ?2")
        .bind(
            json!({ "temperature": 0.2, "kb_mode": "tool",
                    "voice": { "tts_alias": "my-tts", "added_later": true } })
            .to_string(),
        )
        .bind(fid)
        .execute(&state.db)
        .await
        .unwrap();
    let folders = get_json(&gw, "/chat/api/folders").await;
    let d = &folders["folders"][0]["defaults"];
    assert_eq!(d["temperature"], 0.2, "{d}");
    assert_eq!(d["kb_mode"], "tool");
    assert_eq!(d["voice"], json!({ "tts_alias": "my-tts" }));
    // An empty voice is no default at all.
    let (status, f) = post(
        &gw,
        &format!("/chat/api/folders/{fid}"),
        json!({ "defaults": { "voice": { "voice": " " } } }),
    )
    .await;
    assert_eq!(status, 200, "{f}");
    assert_eq!(f["defaults"]["voice"], Value::Null);
}

// -- temporary threads and Keep ----------------------------------------------

#[tokio::test]
async fn a_temporary_thread_holds_its_voice_and_keep_copies_it() {
    let (chat, stt, stt2) = mocks().await;
    let (_state, gw) = gateway(&chat, &stt, &stt2).await;
    let tid = new_thread(&gw, "plain", true).await;
    assert!(tid < 0);
    let (status, res) = set_voice(&gw, tid, json!({ "voice": "alba", "seed": 9 })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(thread(&gw, tid).await["voice"]["voice"], "alba");

    // A dictated send: the user message carries how it was spoken.
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/send"))
        .json(&json!({ "content": "spoken question", "voice": {
            "via": "dictation", "asr": "my-asr", "asr_ms": 98, "audio_ms": 2800
        } }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _ = r.text().await.unwrap();
    let msgs = get_json(&gw, &format!("/chat/api/threads/{tid}")).await["messages"].clone();
    assert_eq!(msgs[0]["voice"]["via"], "dictation", "{msgs}");
    assert_eq!(msgs[0]["voice"]["asr_ms"], 98);
    assert_eq!(msgs[1]["voice"], Value::Null, "a typed reply");

    // A send may not claim to be a realtime turn (400), nor carry reply
    // fields (the request type refuses them: 422, as any malformed body).
    for (bad, want) in [
        (json!({ "via": "realtime" }), 400),
        (json!({ "via": "dictation", "unheard": "x" }), 422),
    ] {
        let (status, res) = post(
            &gw,
            &format!("/chat/api/threads/{tid}/send"),
            json!({ "content": "x", "voice": bad.clone() }),
        )
        .await;
        assert_eq!(status, want, "{bad} -> {res}");
    }

    let (status, kept) = post(&gw, &format!("/chat/api/threads/{tid}/persist"), json!({})).await;
    assert_eq!(status, 200, "{kept}");
    let id = kept["id"].as_i64().unwrap();
    assert_eq!(
        kept["thread"]["voice"],
        json!({ "voice": "alba", "seed": 9 })
    );
    let msgs = get_json(&gw, &format!("/chat/api/threads/{id}")).await["messages"].clone();
    assert_eq!(msgs[0]["voice"]["asr"], "my-asr", "{msgs}");
    assert_eq!(msgs[0]["voice"]["audio_ms"], 2800);
}

// -- audio attachments follow the Chat's ASR chain ---------------------------

#[tokio::test]
async fn an_attachment_is_transcribed_by_realtimes_alias_then_by_the_threads_own() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    // No `chat_stt_alias`: the Chat falls back to realtime's.
    settings_set(
        &state,
        patch(json!({ "realtime": { "asr_alias": "my-asr" } })),
    )
    .await
    .unwrap();
    let tid = new_thread(&gw, "plain", false).await;
    let a = upload_ok(&gw, tid, "memo.wav", wav()).await;
    assert_eq!(a["meta"]["transcript_alias"], "my-asr", "{a}");
    assert_eq!(a["blockers"], json!([]));
    assert_eq!(transcriptions(&stt).await, 1);
    assert_eq!(transcriptions(&stt2).await, 0);

    // The thread's own override wins.
    set_voice(&gw, tid, json!({ "asr_alias": "other-asr" })).await;
    let b = upload_ok(&gw, tid, "memo2.wav", wav()).await;
    assert_eq!(b["meta"]["transcript_alias"], "other-asr", "{b}");
    assert_eq!(transcriptions(&stt2).await, 1);

    // With no alias anywhere, the draft is blocked and the reason points at
    // the voice settings.
    settings_set(&state, patch(json!({ "realtime": { "asr_alias": "" } })))
        .await
        .unwrap();
    let bare = new_thread(&gw, "plain", false).await;
    let c = upload_ok(&gw, bare, "memo3.wav", wav()).await;
    let blockers = c["blockers"].to_string();
    assert!(blockers.contains("Settings → Chat → Voice"), "{c}");
}

// -- spoken turns in storage, export and search -------------------------------

/// A thread with a dictated question and a spoken reply that was cut: its
/// heard part in `content`, the rest in `voice.unheard`.
async fn spoken_thread(state: &SharedState) -> (i64, i64) {
    let tid = store::create_chat_thread(&state.db, "plain", "chat")
        .await
        .unwrap();
    let dictated = store::MessageVoice {
        via: store::VIA_DICTATION.into(),
        asr: Some("my-asr".into()),
        ..Default::default()
    };
    store::append_user_message_with_voice(
        &state.db,
        tid,
        "what about alpaca wool",
        &[],
        &[],
        Some(&dictated),
    )
    .await
    .unwrap();
    let rid = store::append_chat_reply(
        &state.db,
        tid,
        &store::ChatReply {
            content: "Alpaca wool is warm.".into(),
            model: Some("plain".into()),
            voice: Some(store::MessageVoice {
                via: store::VIA_REALTIME.into(),
                tts: Some("my-tts".into()),
                voice: Some("alba".into()),
                unheard: Some("It is also zebrastripe soft.\nAnd light.".into()),
                timing: Some(store::VoiceTiming {
                    asr_ms: Some(31),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    (tid, rid)
}

#[tokio::test]
async fn unheard_text_is_not_searchable_and_the_exports_mark_spoken_turns() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    let (tid, _) = spoken_thread(&state).await;

    let hits = |q: &'static str| {
        let gw = &gw;
        async move {
            get_json(gw, &format!("/chat/api/search?q={q}")).await["threads"]
                .as_array()
                .unwrap()
                .len()
        }
    };
    assert_eq!(hits("alpaca").await, 1, "the heard text is found");
    assert_eq!(hits("zebrastripe").await, 0, "the unheard rest is not");

    let md = gw
        .client()
        .get(format!("{gw}/chat/api/threads/{tid}/export?format=md"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(md.contains("## You · spoken"), "{md}");
    assert!(md.contains("## Assistant · plain · spoken"), "{md}");
    assert!(
        md.contains("Alpaca wool is warm.\n\n> *(not heard)*\n>\n> It is also zebrastripe soft.\n> And light."),
        "{md}"
    );

    let v: Value = gw
        .client()
        .get(format!("{gw}/chat/api/threads/{tid}/export?format=json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["messages"][0]["voice"]["via"], "dictation");
    assert_eq!(v["messages"][1]["voice"]["tts"], "my-tts");
    assert_eq!(v["messages"][1]["voice"]["timing"]["asr_ms"], 31);
    assert_eq!(v["thread"]["voice"], json!({}));
}

#[tokio::test]
async fn editing_a_spoken_reply_clears_unheard_and_timing_continuing_clears_unheard() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    let (tid, rid) = spoken_thread(&state).await;

    let (status, res) = post(
        &gw,
        &format!("/chat/api/threads/{tid}/messages/{rid}/edit"),
        json!({ "content": "Alpaca wool is warm, edited." }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let v = &res["message"]["voice"];
    assert_eq!(v["tts"], "my-tts", "still a spoken reply: {v}");
    assert!(
        v.get("unheard").is_none() && v.get("timing").is_none(),
        "{v}"
    );

    // A continue (here at the store, as the turn's save makes it): the
    // continuation follows the heard text, so the unheard rest goes, the
    // timing stays.
    let (tid, rid) = spoken_thread(&state).await;
    let saved = store::continue_chat_reply(
        &state.db,
        tid,
        rid,
        "Alpaca wool is warm.",
        &store::ChatReply {
            content: "Alpaca wool is warm. And soft.".into(),
            model: Some("plain".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(saved, store::ContinueSave::Saved);
    let row = store::get_chat_message(&state.db, tid, rid)
        .await
        .unwrap()
        .unwrap();
    let v = row.voice.unwrap();
    assert_eq!((v.unheard, v.tts.as_deref()), (None, Some("my-tts")));
    assert!(v.timing.is_some());
}

// -- the migration ------------------------------------------------------------

/// Migration 0057: every thread an install already has overrides nothing
/// (`{}`), and every message is a typed turn (NULL).
#[tokio::test]
async fn migration_0057_leaves_every_thread_and_message_unspoken() {
    let pool = crate::migrations::db_at_version(56).await;
    sqlx::query("INSERT INTO chat_threads (model_alias, kind) VALUES ('m', 'chat')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO chat_messages (thread_id, role, content, reasoning) \
         VALUES (1, 'user', 'hi', '')",
    )
    .execute(&pool)
    .await
    .unwrap();

    store::run_migrations(&pool).await.unwrap();

    let t = store::get_chat_thread(&pool, 1).await.unwrap().unwrap();
    assert!(t.voice.is_empty());
    let m = store::list_chat_messages(&pool, 1).await.unwrap();
    assert_eq!(m[0].voice, None);
}
