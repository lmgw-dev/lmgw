//! Chat voice writes (chat-voice design §2.1, §2.2, §3; WP1 review m1, m2,
//! m8, n1): a settings write never erases a seed drawn meanwhile, an edited
//! dictated message becomes a typed turn, a folder's voice defaults are not
//! re-checked for an alias they already named and take no seed, and an
//! audio attachment's transcript follows the thread's ASR override on
//! history replay and on `/transcribe`. Mock upstreams only.

use lmgw_core::store::{self, SeedWrite};
use serde_json::{json, Value};

use crate::chat_attach_kinds::{new_thread, send, transcriptions, upload_ok, wav};
use crate::chat_voice_settings::{gateway, get_json, mocks, post, set_voice, thread};
use crate::common::Gw;

// -- the seed ------------------------------------------------------------------

#[tokio::test]
async fn a_settings_write_keeps_a_seed_drawn_since_its_read_unless_it_names_one() {
    let (chat, stt, stt2) = mocks().await;
    let (state, _gw) = gateway(&chat, &stt, &stt2).await;
    let tid = store::create_chat_thread(&state.db, "plain", "chat")
        .await
        .unwrap();
    // The settings handler's read: no seed yet.
    let mut t = store::get_chat_thread(&state.db, tid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(t.voice.seed, None);
    // Meanwhile the first speech draws one (WP4).
    sqlx::query("UPDATE chat_threads SET voice = '{\"seed\":7}' WHERE id = ?1")
        .bind(tid)
        .execute(&state.db)
        .await
        .unwrap();
    // The handler's write, from its stale copy: the drawn seed stays.
    t.voice.tts_alias = Some("my-tts".into());
    let stored = store::update_chat_thread_settings(&state.db, &t, SeedWrite::Keep, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (stored.tts_alias.as_deref(), stored.seed),
        (Some("my-tts"), Some(7))
    );
    let row = store::get_chat_thread(&state.db, tid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.voice, stored);

    // A patch that named the seed writes what it named, `null` included.
    t.voice.seed = None;
    let stored = store::update_chat_thread_settings(&state.db, &t, SeedWrite::AsGiven, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.seed, None);
    // No such row: nothing written, and the caller is told.
    t.id = 9999;
    assert_eq!(
        store::update_chat_thread_settings(&state.db, &t, SeedWrite::Keep, None)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn the_settings_answer_carries_the_seed_as_stored() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    let tid = new_thread(&gw, "plain", false).await;
    sqlx::query("UPDATE chat_threads SET voice = '{\"seed\":11}' WHERE id = ?1")
        .bind(tid)
        .execute(&state.db)
        .await
        .unwrap();
    // A save of something else entirely, and one of the voice without the
    // seed: both answer with the stored seed, so the page sees it.
    let (status, res) = post(
        &gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "temperature": 0.4 }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["voice"]["seed"], 11, "{res}");
    assert_eq!(res["voice_resolved"]["seed"], 11);
    let (status, res) = set_voice(&gw, tid, json!({ "read_aloud": true })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["voice"], json!({ "read_aloud": true, "seed": 11 }));
}

// -- an edited dictated message ------------------------------------------------

async fn dictated_send(gw: &Gw, tid: i64) {
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
}

async fn edit(gw: &Gw, tid: i64, mid: i64, content: &str) {
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/messages/{mid}/edit"))
        .json(&json!({ "content": content }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _ = r.text().await.unwrap();
}

async fn messages(gw: &Gw, tid: i64) -> Vec<Value> {
    get_json(gw, &format!("/chat/api/threads/{tid}")).await["messages"]
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn an_edited_dictated_message_becomes_a_typed_turn_on_both_backings() {
    let (chat, stt, stt2) = mocks().await;
    let (_state, gw) = gateway(&chat, &stt, &stt2).await;
    for temporary in [false, true] {
        let tid = new_thread(&gw, "plain", temporary).await;
        dictated_send(&gw, tid).await;
        let mid = messages(&gw, tid).await[0]["id"].as_i64().unwrap();
        // Resent unchanged: still what was spoken.
        edit(&gw, tid, mid, "spoken question").await;
        let m = &messages(&gw, tid).await[0];
        assert_eq!(m["voice"]["via"], "dictation", "temporary {temporary}: {m}");
        // Rewritten by hand: no longer the transcript, so no mic badge and
        // no ASR timings describing it.
        edit(&gw, tid, mid, "typed question").await;
        let m = &messages(&gw, tid).await[0];
        assert_eq!(m["content"], "typed question");
        assert_eq!(m["voice"], Value::Null, "temporary {temporary}: {m}");
    }
}

// -- folders -------------------------------------------------------------------

#[tokio::test]
async fn a_folder_keeps_a_voice_alias_it_named_and_takes_no_seed() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    let (status, folder) = post(
        &gw,
        "/chat/api/folders",
        json!({ "name": "Voice", "defaults": { "voice": { "tts_alias": "my-tts" } } }),
    )
    .await;
    assert_eq!(status, 200, "{folder}");
    let fid = folder["id"].as_i64().unwrap();
    // The model goes away since: saving the folder's other defaults is not
    // blocked by the alias it already named, but a new bad one is refused.
    sqlx::query("UPDATE models SET enabled = 0 WHERE alias = 'my-tts'")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let (status, res) = post(
        &gw,
        &format!("/chat/api/folders/{fid}"),
        json!({ "defaults": { "voice": { "tts_alias": "my-tts", "read_aloud": true } } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["defaults"]["voice"]["read_aloud"], true);
    let (status, res) = post(
        &gw,
        &format!("/chat/api/folders/{fid}"),
        json!({ "defaults": { "voice": { "tts_alias": "gone" } } }),
    )
    .await;
    assert_eq!(status, 400, "{res}");

    // A seed is each thread's own.
    for body in [
        json!({ "name": "Seeded", "defaults": { "voice": { "seed": 5 } } }),
        json!({ "defaults": { "voice": { "seed": 5 } } }),
    ] {
        let path = if body.get("name").is_some() {
            "/chat/api/folders".to_string()
        } else {
            format!("/chat/api/folders/{fid}")
        };
        let (status, res) = post(&gw, &path, body.clone()).await;
        assert_eq!(status, 400, "{body} -> {res}");
        assert!(
            res["message"].as_str().unwrap().contains("voice.seed"),
            "{res}"
        );
    }
}

// -- transcripts follow the thread's ASR override --------------------------------

#[tokio::test]
async fn history_replay_transcribes_with_the_threads_asr_override() {
    let (chat, stt, stt2) = mocks().await;
    let (_state, gw) = gateway(&chat, &stt, &stt2).await;
    // Sent to a model that hears audio: no transcript is made.
    let tid = new_thread(&gw, "hearing", false).await;
    let a = upload_ok(&gw, tid, "memo.wav", wav()).await;
    assert!(a["meta"].get("transcript_alias").is_none(), "{a}");
    assert_eq!(send(&gw, tid, &[a["id"].as_i64().unwrap()]).await.0, 200);
    // The thread moves to a model that cannot hear, with its own ASR: the
    // next turn replays the audio as a transcript made by that alias.
    let (status, res) = post(
        &gw,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "model_alias": "plain", "voice": { "asr_alias": "other-asr" } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(send(&gw, tid, &[]).await.0, 200);
    assert_eq!(transcriptions(&stt).await, 0, "not the settings' alias");
    assert_eq!(transcriptions(&stt2).await, 1, "the thread's alias");
    let reqs = chat.received_requests().await.unwrap();
    let call = reqs
        .iter()
        .rev()
        .find(|r| r.url.path() == "/chat/completions")
        .expect("a chat call");
    let last: Value = serde_json::from_slice(&call.body).unwrap();
    let all = last["messages"].to_string();
    assert!(all.contains(r#"by=\"other-asr\""#), "{all}");
    assert!(all.contains("from the thread's alias"), "{all}");
}

#[tokio::test]
async fn the_transcribe_route_uses_the_threads_asr_override() {
    let (chat, stt, stt2) = mocks().await;
    let (_state, gw) = gateway(&chat, &stt, &stt2).await;
    // No ASR anywhere at upload: no transcript, no error.
    let tid = new_thread(&gw, "plain", false).await;
    let a = upload_ok(&gw, tid, "memo.wav", wav()).await;
    let id = a["id"].as_i64().unwrap();
    assert!(a["meta"].get("transcript_alias").is_none(), "{a}");
    set_voice(&gw, tid, json!({ "asr_alias": "other-asr" })).await;
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/attachments/{id}/transcribe"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["meta"]["transcript_alias"], "other-asr", "{v}");
    assert_eq!(transcriptions(&stt2).await, 1);
    assert_eq!(transcriptions(&stt).await, 0);
    // The thread still reads as it was set.
    assert_eq!(thread(&gw, tid).await["voice"]["asr_alias"], "other-asr");
}
