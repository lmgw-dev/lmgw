//! A thread whose voice is a voice-library clip without a transcript
//! (chat-voice design §2.3, §6.1): for OmniVoice, which cannot clone a clip
//! without one, the thread JSON's `problems` say so (the page warns, and
//! refuses read-aloud and voice mode before the press), read-aloud is
//! refused `voice_needs_transcript` naming the clip and the fix, and the
//! press's warm starts nothing for it. A clip with its transcript has no
//! problem. For a family the table does not know refuses (upstream drift),
//! audio.cpp's own 500 reaches read-aloud as the same code. The container
//! is the audio world's wiremock stand-in; the clip is synthetic bytes.

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::chat_voice_dictation::{states, warm};
use crate::chat_voice_settings::set_voice;
use crate::chat_voice_speak::{names, settings, stored_reply, thread, Reader};
use crate::chat_voice_speak_style::get_thread;
use crate::support::audio_world::{wav_bytes, world, World};
use crate::support::realtime_fakes::{add_chat_aliases, chat_fake, ChatFake, Turn};

/// The audio world with the chat fake (`chatty`), the TTS row `omni` of
/// `family` the Chat speaks with, one clip `anna.wav` in the voice library
/// without a transcript, and a thread whose voice is that clip.
async fn clip_thread(family: &str) -> (World, ChatFake, i64) {
    let w = world().await;
    let chat = chat_fake().await;
    add_chat_aliases(&w.state, &chat).await;
    w.row("omni", family, |_| {}).await;
    let voices = w.models.path().join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("anna.wav"), wav_bytes(24_000, 480)).unwrap();
    settings(&w.state, |s| s.chat_tts_alias = "audio/omni".into()).await;
    let tid = thread(&w.gw, "chatty").await;
    let (status, body) = set_voice(&w.gw, tid, json!({ "language": "de", "voice": "anna" })).await;
    assert_eq!(status, 200, "{body}");
    (w, chat, tid)
}

/// The thread's `tts` problems.
async fn tts_problems(w: &World, tid: i64) -> Vec<Value> {
    let t = get_thread(&w.gw, tid).await;
    t["thread"]["voice_resolved"]["problems"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["stage"] == "tts")
        .cloned()
        .collect()
}

async fn speak(w: &World, tid: i64, mid: i64) -> Vec<(String, Value)> {
    let r =
        w.gw.client()
            .post(format!(
                "{}/chat/api/threads/{tid}/messages/{mid}/speak",
                w.gw
            ))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
    Reader::new(r).rest().await
}

#[tokio::test]
async fn omnivoice_flags_an_untranscribed_clip_and_refuses_to_speak_it() {
    let (w, chat, tid) = clip_thread("omnivoice").await;

    // The voice settings say it before anything is pressed.
    let problems = tts_problems(&w, tid).await;
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0]["code"], "voice_needs_transcript");
    let m = problems[0]["message"].as_str().unwrap();
    assert!(m.contains("'anna'") && m.contains("'audio/omni'"), "{m}");
    assert!(m.contains("transcribe it in the Audio lab"), "{m}");

    // Read-aloud is refused with the same words, and nothing is started.
    chat.push(Turn::text(&["Hallo."]));
    let mid = stored_reply(&w.gw, tid, "hi").await;
    let events = speak(&w, tid, mid).await;
    assert_eq!(names(&events), ["speech_error"], "{events:?}");
    assert_eq!(events[0].1["code"], "voice_needs_transcript");
    assert!(
        events[0].1["message"].as_str().unwrap().contains("'anna'"),
        "{events:?}"
    );

    // The press's warm starts nothing for a voice it cannot speak.
    let s = states(&warm(&w.gw, tid, &["tts"]).await, "tts");
    assert_eq!(
        s.last().map(|f| (&f["state"], &f["reason"])),
        Some((&json!("skipped"), &json!("cannot_speak"))),
        "{s:?}"
    );
    assert_eq!(w.runs(), 0, "nothing was started");
    assert!(w.sent().await.is_empty(), "nothing reached the engine");

    // Transcribed, the clip is no problem any more.
    std::fs::write(
        w.models.path().join("voices/prompt_text"),
        "anna|Hallo zusammen.\n",
    )
    .unwrap();
    assert!(tts_problems(&w, tid).await.is_empty());
}

#[tokio::test]
async fn audio_cpp_s_own_refusal_reaches_read_aloud_as_voice_needs_transcript() {
    // A family the table says clones without — as if upstream changed it.
    let (w, chat, tid) = clip_thread("voxcpm2").await;
    assert!(tts_problems(&w, tid).await.is_empty());
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": {
            "message": "VoxCPM2 voice clone requires reference_text",
            "type": "server_error",
        }})))
        .mount(&w.container)
        .await;
    chat.push(Turn::text(&["Hallo."]));
    let mid = stored_reply(&w.gw, tid, "hi").await;
    let events = speak(&w, tid, mid).await;
    let error = events
        .iter()
        .find(|(e, _)| e == "speech_error")
        .unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!(error.1["code"], "voice_needs_transcript", "{events:?}");
    let m = error.1["message"].as_str().unwrap();
    assert!(m.contains("'anna'") && m.contains("Audio lab"), "{m}");
}
