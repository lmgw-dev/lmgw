//! The conversation language (chat-voice design §2.1, changed 2026-10-04
//! after the owner's test): a thread's language — its own, else Settings →
//! Chat → Voice's — is a request to its speech models, not a hint. Supertonic
//! is sent it on read-aloud and in a bound session (without it, it reads
//! every clause as English); a TTS whose set lacks the language, and Kokoro,
//! are sent none, the first with a note beside the resolved voice; a stock
//! realtime session's own language stays a hint; dictation goes up in
//! Nemotron ASR's own spelling, and Parakeet TDT, which detects the language
//! itself, is said to. Local rows run on the fake GPU world with synthetic
//! packages; nothing real is started.

use std::path::Path;

use lmgw_core::config::Settings;
use lmgw_core::gguf::synth;
use lmgw_core::store;
use serde_json::{json, Value};

use crate::chat_attach_kinds::new_thread;
use crate::chat_voice_dictation::{recording, store_voice};
use crate::chat_voice_settings::get_json;
use crate::chat_voice_speak::{settings, stored_reply, thread, Reader};
use crate::chat_voice_speak_style::get_thread;
use crate::common::{serve, Gw};
use crate::realtime_expressive::{answer, create};
use crate::support::audio_world::{tts_row, wav_bytes};
use crate::support::audiocpp_gguf;
use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::{add_chat_aliases, chat_fake, ChatFake, Turn};
use crate::support::realtime_tts::spoken_session;

/// A TTS row: its model id, family and package.
pub(crate) type Row = (&'static str, &'static str, fn(&Path));

/// A GPU world with the TTS `rows`, whose Chat speaks with `speaks`, with a
/// voice-library clip `alba`, then `tweak`.
pub(crate) struct Local {
    pub g: Gpu,
    pub chat: ChatFake,
    pub gw: Gw,
}

pub(crate) async fn local(rows: &[Row], speaks: &str, tweak: impl FnOnce(&mut Settings)) -> Local {
    let g = Gpu::new(24 * GIB, rows.len(), 5).await;
    let chat = chat_fake().await;
    add_chat_aliases(&g.state, &chat).await;
    for (id, family, package) in rows {
        let root = g.models_dir().join(id);
        std::fs::create_dir_all(&root).unwrap();
        package(&root);
        store::insert_audio_model(&g.state.db, &tts_row(id, family))
            .await
            .unwrap();
    }
    let voices = g.models_dir().join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("alba.wav"), wav_bytes(24_000, 480)).unwrap();
    let models = g.models_dir().display().to_string();
    let speaks = speaks.to_string();
    settings(&g.state, |s| {
        s.audio.models_dir = models;
        s.chat_tts_alias = speaks;
        s.realtime.default_voice = "alba".into();
        s.realtime.warm_on_connect = false;
        tweak(s);
    })
    .await;
    let gw = serve(g.state.clone()).await;
    Local { g, chat, gw }
}

fn kokoro(r: &Path) {
    audiocpp_gguf::with_options(r, "kokoro_tts", &["language", "speed"], &["offline"])
}

/// Read reply `mid` of `tid` aloud to its end; the speech bodies it sent.
pub(crate) async fn speak(l: &Local, tid: i64, mid: i64) -> Vec<Value> {
    let before = l.g.world().speech_bodies.len();
    let r =
        l.gw.client()
            .post(format!(
                "{}/chat/api/threads/{tid}/messages/{mid}/speak",
                l.gw
            ))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
    let events = Reader::new(r).rest().await;
    assert_eq!(events.last().unwrap().0, "speech_done", "{events:?}");
    let bodies = l.g.world().speech_bodies[before..].to_vec();
    assert!(!bodies.is_empty());
    bodies
}

async fn set_voice(gw: &Gw, tid: i64, voice: Value) -> Value {
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/settings"))
        .json(&json!({ "voice": voice }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

#[tokio::test]
async fn supertonic_reads_aloud_in_the_thread_s_language() {
    let l = local(
        &[("st", "supertonic", audiocpp_gguf::supertonic)],
        "audio/st",
        |s| s.realtime.default_voice = "M1".into(),
    )
    .await;
    let tid = thread(&l.gw, "chatty").await;
    l.chat.push(Turn::text(&["Heute ist das Wetter schön."]));
    let mid = stored_reply(&l.gw, tid, "hallo").await;
    let t = get_thread(&l.gw, tid).await;
    assert_eq!(t["thread"]["voice_resolved"]["language"]["value"], "de");
    assert_eq!(
        t["thread"]["voice_resolved"]["language_notes"],
        json!([]),
        "Supertonic speaks German: {t}"
    );
    for b in speak(&l, tid, mid).await {
        assert_eq!(b["language"], "de", "every clause: {b}");
    }

    // A language its set lacks: none sent — a 500 per clause otherwise —
    // and the page is told why.
    let answer = set_voice(&l.gw, tid, json!({ "language": "sw" })).await;
    let notes = answer["voice_resolved"]["language_notes"].clone();
    assert_eq!(notes[0]["stage"], "tts", "{notes}");
    assert_eq!(notes[0]["alias"], "audio/st", "{notes}");
    let message = notes[0]["message"].as_str().unwrap();
    assert!(
        message.starts_with("text-to-speech model 'audio/st' does not speak sw (sw)"),
        "{message}"
    );
    for b in speak(&l, tid, mid).await {
        assert!(b.get("language").is_none(), "{b}");
    }
}

#[tokio::test]
async fn kokoro_is_sent_no_language() {
    let l = local(&[("kokoro", "kokoro_tts", kokoro)], "audio/kokoro", |_| {}).await;
    let tid = thread(&l.gw, "chatty").await;
    l.chat.push(Turn::text(&["Guten Tag."]));
    let mid = stored_reply(&l.gw, tid, "hallo").await;
    for b in speak(&l, tid, mid).await {
        assert!(
            b.get("language").is_none(),
            "its voice decides, and it refuses a mismatch: {b}"
        );
    }
}

#[tokio::test]
async fn a_bound_session_speaks_the_thread_s_language_on_supertonic() {
    let g = Gpu::new(24 * GIB, 1, 5).await;
    let root = g.models_dir().join("st");
    std::fs::create_dir_all(&root).unwrap();
    audiocpp_gguf::supertonic(&root);
    store::insert_audio_model(&g.state.db, &tts_row("st", "supertonic"))
        .await
        .unwrap();
    let models = g.models_dir().display().to_string();
    let w = crate::realtime_chat_thread::world_on(g.state.clone(), |s| {
        s.audio.models_dir = models;
        s.chat_tts_alias = "audio/st".into();
        s.realtime.default_voice = "M1".into();
        s.realtime.warm_on_connect = false;
    })
    .await;
    let tid = w.thread("chatty", json!({})).await;
    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Wie spät ist es?"));
    w.chat.push(Turn::text(&["Es ist drei Uhr."]));
    crate::realtime_chat_thread::say(&mut ws).await;
    crate::realtime_chat_thread::until_type(&mut ws, "lmgw.response.timing").await;
    let bodies = g.world().speech_bodies.clone();
    assert!(!bodies.is_empty());
    for b in &bodies {
        assert_eq!(b["language"], "de", "{b}");
    }
    // And the reply was asked for in it.
    let system = w.chat.seen.chat(0)["messages"][0]["content"].to_string();
    assert!(system.contains("The user speaks German"), "{system}");
}

#[tokio::test]
async fn a_stock_session_s_own_language_stays_a_hint() {
    let (g, addr, chat) = crate::realtime_expressive::gateway(
        &[(
            "st",
            "supertonic",
            "tts",
            json!({}),
            audiocpp_gguf::supertonic,
        )],
        1,
        |s| s.realtime.default_voice = "M1".into(),
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
        assert!(
            b.get("language").is_none(),
            "what the user speaks is no request for the reply: {b}"
        );
    }
}

/// A Nemotron ASR package whose processor config has these prompts.
pub(crate) fn nemotron(root: &Path) {
    let cfg = serde_json::to_vec(&json!({
        "prompt_dictionary": {"auto": 0, "de": 7, "de-DE": 8, "id-ID": 11, "ja-JA": 20, "ja-JP": 21}
    }))
    .unwrap();
    synth::audiocpp(
        "nemotron_asr",
        r#"{"family":"nemotron_asr","tasks":["asr"],"languages":["de-DE","id-ID","ja-JP"]}"#,
        &[("processor_config.json", &cfg)],
    )
    .write_to(&root.join("nemotron-asr-q8_0.gguf"));
}

pub(crate) async fn dictate(gw: &Gw, tid: i64) -> Value {
    let r = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{tid}/transcribe"))
        .header("content-type", "audio/wav")
        .body(recording(1))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

#[tokio::test]
async fn dictation_goes_up_in_the_asr_row_s_spelling_and_a_detecting_row_is_said_to() {
    let (g, gw) = crate::chat_voice_dictation::world(24 * GIB, 2).await;
    g.model("talk", GIB).await;
    for (id, family) in [("nemo", "nemotron_asr"), ("para", "parakeet_tdt")] {
        let root = g.models_dir().join(id);
        std::fs::create_dir_all(&root).unwrap();
        if id == "nemo" {
            nemotron(&root);
        } else {
            std::fs::write(root.join("model.gguf"), vec![0u8; 4096]).unwrap();
        }
        let mut row = tts_row(id, family);
        row.task = "asr".into();
        store::insert_audio_model(&g.state.db, &row).await.unwrap();
    }
    g.state.reload_snapshot().await.unwrap();
    let tid = new_thread(&gw, "talk", false).await;

    // Nemotron's own keys: `de` as it is, `id` as the one prompt it names.
    for (asked, sent) in [("de", "de"), ("id", "id-ID")] {
        store_voice(
            &g.state,
            tid,
            json!({ "asr_alias": "audio/nemo", "language": asked }),
        )
        .await;
        dictate(&gw, tid).await;
        let sent_now = g.world().transcription_languages.last().cloned().flatten();
        assert_eq!(sent_now.as_deref(), Some(sent), "{asked}");
        let t = get_thread(&gw, tid).await;
        assert_eq!(t["thread"]["voice_resolved"]["language_notes"], json!([]));
    }
    // `ja` names two prompts: sent as it came, and the page is told the
    // model would refuse it.
    store_voice(
        &g.state,
        tid,
        json!({ "asr_alias": "audio/nemo", "language": "ja" }),
    )
    .await;
    let t = get_thread(&gw, tid).await;
    let note = &t["thread"]["voice_resolved"]["language_notes"][0];
    assert_eq!(note["stage"], "asr", "{t}");
    assert!(
        note["message"]
            .as_str()
            .unwrap()
            .contains("has no prompt for Japanese (ja)"),
        "{note}"
    );

    // Parakeet TDT detects the language itself.
    store_voice(
        &g.state,
        tid,
        json!({ "asr_alias": "audio/para", "language": "de" }),
    )
    .await;
    let t = get_thread(&gw, tid).await;
    let note = &t["thread"]["voice_resolved"]["language_notes"][0];
    assert_eq!(note["alias"], "audio/para", "{t}");
    assert!(
        note["message"]
            .as_str()
            .unwrap()
            .starts_with("speech-to-text model 'audio/para' detects the language itself"),
        "{note}"
    );

    // Settings → Chat → Voice says the same for its own models, as saved.
    settings(&g.state, |s| {
        s.chat_stt_alias = "audio/para".into();
        s.chat_voice_language = "de".into();
    })
    .await;
    let full = get_json(&gw, "/api/settings-full").await;
    let notes = &full["chat_voice_language_notes"];
    assert_eq!(notes.as_array().map(Vec::len), Some(1), "{notes}");
    assert_eq!(notes[0]["stage"], "asr");
    settings(&g.state, |s| s.chat_voice_language = String::new()).await;
    let full = get_json(&gw, "/api/settings-full").await;
    assert_eq!(full["chat_voice_language_notes"], json!([]), "no language");
}
