//! Personality profiles' voice resolution (personality-profiles design
//! §2.3's voice half; filled by WP3): the tiers thread → profile → `chat_*`
//! → `realtime.*`, `source: "profile"`, M1 for a profile's voice, and the
//! notes `voice_resolved` carries.

use lmgw_core::config::chat_profile::{create_kind, ProfileCreate, ProfilePatch, ProfileVoice};
use lmgw_core::state::SharedState;
use lmgw_core::store::chat_profiles as profiles;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::chat_attach_kinds::{new_thread, patch};
use crate::chat_voice_settings::{gateway, mocks, thread};

fn voice(tts: Option<&str>, v: Option<&str>, style: Option<&str>) -> ProfileVoice {
    ProfileVoice {
        tts_alias: tts.map(Into::into),
        voice: v.map(Into::into),
        speech_style: style.map(Into::into),
    }
}

async fn profile(state: &SharedState, name: &str, voice: ProfileVoice) -> i64 {
    let kind = create_kind(&ProfileCreate {
        name: Some(name.into()),
        voice,
        ..Default::default()
    })
    .unwrap();
    let id = profiles::create_chat_profile(&state.db, &kind)
        .await
        .unwrap()
        .unwrap()
        .id;
    state.reload_snapshot().await.unwrap();
    id
}

/// Bind the thread to the profile (the routes are WP4's), or unbind it.
async fn bind(state: &SharedState, tid: i64, id: Option<i64>) {
    sqlx::query("UPDATE chat_threads SET profile_id = ?1 WHERE id = ?2")
        .bind(id)
        .bind(tid)
        .execute(&state.db)
        .await
        .unwrap();
}

async fn store_voice(state: &SharedState, tid: i64, v: Value) {
    sqlx::query("UPDATE chat_threads SET voice = ?1 WHERE id = ?2")
        .bind(v.to_string())
        .bind(tid)
        .execute(&state.db)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_profile_is_a_tier_between_the_thread_and_the_chat() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    lmgw_core::ops::settings_set(
        &state,
        patch(json!({
            "chat_tts_alias": "my-tts",
            "chat_voice": "alba",
            "chat_speech_style": "warm",
            "realtime": { "default_voice": "M5" },
        })),
    )
    .await
    .unwrap();
    let tid = new_thread(&gw, "plain", false).await;
    let plain = thread(&gw, tid).await["voice_resolved"].clone();
    assert_eq!(plain["voice"]["source"], "chat", "{plain}");

    // A profile that sets no voice field changes nothing.
    let empty = profile(&state, "Leer", ProfileVoice::default()).await;
    bind(&state, tid, Some(empty)).await;
    assert_eq!(thread(&gw, tid).await["voice_resolved"], plain);

    // Its own style, voice and model, each named `profile`.
    let p = profile(
        &state,
        "Ruhig",
        voice(Some("cloud-tts"), Some("pv"), Some("slow")),
    )
    .await;
    bind(&state, tid, Some(p)).await;
    let t = thread(&gw, tid).await;
    assert_eq!(t["profile_id"], p, "{t}");
    let r = &t["voice_resolved"];
    assert_eq!(r["tts"]["alias"], "cloud-tts", "{r}");
    assert_eq!(r["tts"]["source"], "profile");
    assert_eq!(r["tts"]["inherited"], "my-tts");
    assert_eq!(r["voice"]["name"], "pv");
    assert_eq!(r["voice"]["source"], "profile");
    assert_eq!(r["speech_style"]["text"], "slow");
    assert_eq!(r["speech_style"]["source"], "profile");

    // The thread's own fields win, one at a time; the rest stays the profile's.
    store_voice(&state, tid, json!({ "speech_style": "fast" })).await;
    let r = thread(&gw, tid).await["voice_resolved"].clone();
    assert_eq!(r["speech_style"]["source"], "thread", "{r}");
    assert_eq!(r["voice"]["source"], "profile");

    // M1: the thread speaks with a model the profile's voice was not chosen
    // for. The voice stands back, nothing named goes out, and the note says
    // whose voice it was.
    store_voice(&state, tid, json!({ "tts_alias": "other-tts" })).await;
    let r = thread(&gw, tid).await["voice_resolved"].clone();
    assert_eq!(r["tts"]["source"], "thread", "{r}");
    assert_eq!(r["voice"]["name"], Value::Null);
    assert_eq!(r["voice"]["inherits"], "M5");
    let note = r["voice"]["note"].as_str().unwrap();
    assert!(
        note.contains("'pv'") && note.contains("'Ruhig'") && note.contains("'cloud-tts'"),
        "{note}"
    );

    // Unbinding is back to exactly the plain thread.
    store_voice(&state, tid, json!({})).await;
    bind(&state, tid, None).await;
    assert_eq!(thread(&gw, tid).await["voice_resolved"], plain);
}

#[tokio::test]
async fn a_profile_edit_reaches_the_next_resolution_through_a_new_snapshot() {
    let (chat, stt, stt2) = mocks().await;
    let (state, gw) = gateway(&chat, &stt, &stt2).await;
    let p = profile(
        &state,
        "Ruhig",
        voice(Some("cloud-tts"), None, Some("slow")),
    )
    .await;
    let tid = new_thread(&gw, "plain", false).await;
    bind(&state, tid, Some(p)).await;
    let before = state.snapshot();
    assert_eq!(
        thread(&gw, tid).await["voice_resolved"]["speech_style"]["text"],
        "slow"
    );

    // What the bound session's stage cache keys on (`Arc::ptr_eq` of the
    // snapshot) moves with the edit, and the new snapshot resolves anew.
    let edit = ProfilePatch {
        voice: Some(Some(voice(Some("cloud-tts"), None, Some("brisk")))),
        ..Default::default()
    };
    profiles::update_chat_profile(&state.db, p, &edit)
        .await
        .unwrap()
        .unwrap();
    state.reload_snapshot().await.unwrap();
    assert!(!Arc::ptr_eq(&before, &state.snapshot()));
    assert_eq!(
        thread(&gw, tid).await["voice_resolved"]["speech_style"]["text"],
        "brisk"
    );
}

/// A bound session starts with the profile's voice and style, as with a
/// thread's own (`realtime::thread::shape_session`).
#[tokio::test]
async fn a_bound_session_starts_with_the_profiles_voice_and_style() {
    use crate::realtime_chat_thread::world;

    let w = world(|_| {}).await;
    let tid = w.thread("other", json!({})).await;
    let (_ws, created) = w.bind(tid).await;
    let s = &created["session"];
    // Realtime's own rule names `alba`; the thread names nothing.
    assert_eq!(s["lmgw"]["resolved"]["voice"], "alba", "{s}");
    assert_eq!(s["lmgw"]["resolved"]["speech"]["text"], Value::Null, "{s}");

    let p = profile(
        &w.state,
        "Ruhig",
        voice(None, Some("cosette"), Some("slowly")),
    )
    .await;
    bind(&w.state, tid, Some(p)).await;
    let (_ws, created) = w.bind(tid).await;
    let s = &created["session"];
    assert_eq!(s["audio"]["output"]["voice"], "cosette", "{s}");
    assert_eq!(s["lmgw"]["resolved"]["voice"], "cosette", "{s}");
    assert_eq!(s["lmgw"]["resolved"]["speech"]["text"], "slowly", "{s}");
    assert_eq!(s["lmgw"]["tts_model"], "speak");
}
