//! What each kind of speech row is sent of the conversation language
//! (chat-voice design §2.1; review of 2026-10-04): a family that reads
//! only `options.language` (Sopro) gets it there; a stock realtime
//! session's own language reaches FireRedTTS3 as before the conversation
//! language existed, through its `language` option; a family that takes
//! `auto` alone (VieNeu-TTS) is sent none and said to; and a dictation
//! answered by a fallback goes up in the fallback's spelling, not the
//! primary row's. Local rows run on the fake GPU world with synthetic
//! packages; nothing real is started.

use std::path::Path;

use lmgw_core::config::HoldFallbackMode;
use lmgw_core::gguf::synth;
use lmgw_core::store;
use serde_json::{json, Value};

use crate::chat_attach_kinds::new_thread;
use crate::chat_voice_dictation::{cloud_asr, store_voice, tweak};
use crate::chat_voice_language::{dictate, local, nemotron, speak};
use crate::chat_voice_speak::{stored_reply, thread};
use crate::chat_voice_speak_style::get_thread;
use crate::realtime_expressive::{answer, create};
use crate::support::audio_world::tts_row;
use crate::support::gpu_world::GIB;
use crate::support::realtime_fakes::Turn;
use crate::support::realtime_tts::spoken_session;

/// A package of `family` whose spec lists `languages` and declares a
/// `language` request option.
fn with_language(root: &Path, family: &str, languages: &[&str]) {
    let spec = json!({
        "family": family, "tasks": ["tts"], "modes": ["offline"], "languages": languages,
        "options": {"request": [{"name": "language", "type": "string"}]}
    });
    synth::audiocpp(family, &spec.to_string(), &[])
        .write_to(&root.join(format!("{family}-q8_0.gguf")));
}

fn sopro(root: &Path) {
    with_language(root, "sopro_tts", &["en", "pt", "fr", "de"]);
}

fn fireredtts3(root: &Path) {
    with_language(root, "fireredtts3", &["Chinese", "English", "German"]);
}

fn vieneu(root: &Path) {
    let spec = json!({
        "family": "vieneu_v3_turbo", "tasks": ["tts"], "modes": ["offline"],
        "languages": ["vi", "en"]
    });
    synth::audiocpp("vieneu_v3_turbo", &spec.to_string(), &[])
        .write_to(&root.join("vieneu-q8_0.gguf"));
}

#[tokio::test]
async fn sopro_is_sent_options_language_and_no_top_level_one() {
    let l = local(&[("sopro", "sopro_tts", sopro)], "audio/sopro", |_| {}).await;
    let tid = thread(&l.gw, "chatty").await;
    l.chat.push(Turn::text(&["Guten Tag."]));
    let mid = stored_reply(&l.gw, tid, "hallo").await;
    let t = get_thread(&l.gw, tid).await;
    assert_eq!(
        t["thread"]["voice_resolved"]["language_notes"],
        json!([]),
        "{t}"
    );
    for b in speak(&l, tid, mid).await {
        assert_eq!(b["options"]["language"], "de", "{b}");
        assert!(b.get("language").is_none(), "it reads only options: {b}");
    }
}

#[tokio::test]
async fn vieneu_is_sent_no_language_and_the_page_is_told() {
    let l = local(&[("vn", "vieneu_v3_turbo", vieneu)], "audio/vn", |_| {}).await;
    let tid = thread(&l.gw, "chatty").await;
    let t = get_thread(&l.gw, tid).await;
    let note = &t["thread"]["voice_resolved"]["language_notes"][0];
    assert_eq!(note["stage"], "tts", "{t}");
    assert!(
        note["message"]
            .as_str()
            .unwrap()
            .contains("takes no language but `auto`"),
        "{note}"
    );
    l.chat.push(Turn::text(&["Xin chào."]));
    let mid = stored_reply(&l.gw, tid, "hallo").await;
    for b in speak(&l, tid, mid).await {
        assert!(b.get("language").is_none(), "{b}");
        assert!(b["options"].get("language").is_none(), "{b}");
    }
}

#[tokio::test]
async fn a_stock_session_s_language_reaches_fireredtts3_as_before() {
    let (g, addr, chat) = crate::realtime_expressive::gateway(
        &[("fr3", "fireredtts3", "tts", json!({}), fireredtts3)],
        1,
        |_| {},
    )
    .await;
    let (mut ws, _) = spoken_session(
        &addr,
        &[],
        60_000,
        json!({"audio": {"input": {"transcription": {"language": "de"}}}}),
    )
    .await;
    answer(&mut ws, &chat, &["Hallo du."], create()).await;
    let bodies = g.world().speech_bodies.clone();
    assert!(!bodies.is_empty());
    for b in &bodies {
        assert_eq!(
            b["language"], "German",
            "its `language` option's name, as before 2026-10-04: {b}"
        );
    }
}

/// The `language` field of every upload `mock` received, in order.
async fn languages_at(mock: &wiremock::MockServer) -> Vec<Option<String>> {
    let mut out = Vec::new();
    for r in mock.received_requests().await.unwrap_or_default() {
        let text = String::from_utf8_lossy(&r.body).to_string();
        let found = text.find("name=\"language\"").and_then(|at| {
            let rest = &text[at..];
            let value = &rest[rest.find("\r\n\r\n")? + 4..];
            Some(value[..value.find("\r\n")?].to_string())
        });
        out.push(found);
    }
    out
}

#[tokio::test]
async fn a_dictation_a_fallback_answers_goes_up_in_the_fallback_s_spelling() {
    let (g, gw) = crate::chat_voice_dictation::world(24 * GIB, 2).await;
    g.model("talk", GIB).await;
    let cloud = cloud_asr(&g.state, "cloud-asr", "from the provider").await;
    let root = g.models_dir().join("nemo");
    std::fs::create_dir_all(&root).unwrap();
    nemotron(&root);
    let mut row = tts_row("nemo", "nemotron_asr");
    row.task = "asr".into();
    row.hold_fallback_mode = HoldFallbackMode::Alias;
    row.hold_fallback = Some("cloud-asr".into());
    store::insert_audio_model(&g.state.db, &row).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
    let tid = new_thread(&gw, "talk", false).await;
    store_voice(
        &g.state,
        tid,
        json!({ "asr_alias": "audio/nemo", "language": "id" }),
    )
    .await;

    // Nemotron itself takes `id` as its `id-ID` prompt.
    dictate(&gw, tid).await;
    let sent = g.world().transcription_languages.last().cloned().flatten();
    assert_eq!(sent.as_deref(), Some("id-ID"));

    // Under the hold its fallback answers, and is sent the code as it came:
    // `id-ID` is Nemotron's spelling, not the provider's (OpenAI takes
    // ISO 639-1).
    tweak(&g.state, |s| s.hold.active = true).await;
    let v: Value = dictate(&gw, tid).await;
    assert_eq!(v["asr_answered_by"], "cloud-asr", "{v}");
    assert_eq!(languages_at(&cloud).await, [Some("id".to_string())]);
}
