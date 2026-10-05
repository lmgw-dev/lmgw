//! `POST /v1/audio/speech` and audio.cpp's refusals: a voice-library clip
//! with no transcript is refused before anything starts when the row's
//! engine is one that cannot clone without (`audio::families::
//! clone_requires_transcript`, OmniVoice here); audio.cpp's own 500 for it
//! — a family the table does not list, as upstream drifts — and for an
//! image without eSpeak NG are said as lmgw's refusals with their codes,
//! never as a bare upstream error. The container is a wiremock stand-in;
//! the clips are a few synthetic bytes.

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::support::audio_world::{wav_bytes, world, World};

/// A world with the TTS row `tts` of `family` and one clip, `anna.wav`, in
/// the voice library — with `transcript` as its `prompt_text` line.
async fn clip_world(family: &str, transcript: Option<&str>) -> World {
    let w = world().await;
    w.row("tts", family, |_| {}).await;
    let voices = w.models.path().join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("anna.wav"), wav_bytes(24_000, 480)).unwrap();
    if let Some(t) = transcript {
        std::fs::write(voices.join("prompt_text"), format!("anna|{t}\n")).unwrap();
    }
    w
}

/// The container answers every speech request with audio.cpp's 500 for
/// `message`.
async fn engine_says(w: &World, message: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": {
            "message": message,
            "type": "server_error",
        }})))
        .mount(&w.container)
        .await;
}

async fn speak(w: &World, voice: Option<&str>) -> (u16, Value) {
    let mut body = json!({"model": "audio/tts", "input": "Guten Tag."});
    if let Some(v) = voice {
        body["voice"] = json!(v);
    }
    let r = w.speak(body).await;
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn omnivoice_with_an_untranscribed_clip_is_refused_before_a_start() {
    let w = clip_world("omnivoice", None).await;
    // The voice list says so beside the clip.
    let v = w.voices("tts").await;
    assert_eq!(v["lmgw"]["needs_transcript"], true, "{v}");
    let anna = v["lmgw"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "anna")
        .cloned()
        .unwrap();
    assert_eq!(anna["transcript"], false, "{anna}");

    let (status, body) = speak(&w, Some("anna")).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "voice_needs_transcript", "{body}");
    let m = body["error"]["message"].as_str().unwrap();
    assert!(m.contains("'anna'") && m.contains("'tts'"), "{m}");
    assert!(m.contains("transcribe it in the Audio lab"), "{m}");
    assert_eq!(w.runs(), 0, "nothing was started for it");
    assert!(w.sent().await.is_empty());
}

#[tokio::test]
async fn a_transcribed_clip_or_a_transcript_of_the_request_s_own_is_sent() {
    let w = clip_world("omnivoice", Some("Hallo zusammen.")).await;
    w.answer_wav().await;
    let (status, body) = speak(&w, Some("anna")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(w.voices("tts").await["lmgw"]["needs_transcript"], true);

    let w = clip_world("omnivoice", None).await;
    w.answer_wav().await;
    let r = w
        .speak(json!({
            "model": "audio/tts", "input": "Guten Tag.", "voice": "anna",
            "reference_text": "Hallo zusammen."
        }))
        .await;
    assert_eq!(r.status(), 200);
    // An engine that speaks without one is not refused (the note says it
    // clones better with one).
    let w = clip_world("cosyvoice3", None).await;
    w.answer_wav().await;
    let (status, body) = speak(&w, Some("anna")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(w.voices("tts").await["lmgw"]["needs_transcript"], false);
}

#[tokio::test]
async fn audio_cpp_s_own_transcript_refusal_is_voice_needs_transcript() {
    // A family the table says clones without — as if upstream changed it.
    let w = clip_world("voxcpm2", None).await;
    engine_says(&w, "VoxCPM2 voice clone requires reference_text").await;
    let (status, body) = speak(&w, Some("anna")).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "voice_needs_transcript", "{body}");
    let m = body["error"]["message"].as_str().unwrap();
    assert!(m.contains("'anna'") && m.contains("'audio/tts'"), "{m}");
    assert!(m.contains("Audio lab"), "{m}");
    assert!(
        m.contains("VoxCPM2 voice clone requires reference_text"),
        "audio.cpp's words stay: {m}"
    );
    assert_eq!(w.sent().await.len(), 1, "it reached the engine");
}

#[tokio::test]
async fn an_image_without_espeak_says_to_rebuild_it() {
    let w = clip_world("sanotts", None).await;
    engine_says(
        &w,
        "Could not load eSpeak-ng; install the shared library or provide its path",
    )
    .await;
    let (status, body) = speak(&w, None).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "audio_image_lacks_espeak", "{body}");
    let m = body["error"]["message"].as_str().unwrap();
    assert!(m.starts_with("'audio/tts' needs eSpeak NG"), "{m}");
    assert!(
        m.contains("rebuild the audio.cpp image on the Backends page"),
        "{m}"
    );
}
