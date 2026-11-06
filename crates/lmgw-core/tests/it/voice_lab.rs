//! The Voices lab (`lmgw_core::voice_lab`): the sentence table and the
//! owner's sentences, trimming a candidate's silence, rounds rendered on a
//! voice-design row through its fallback too, saving a sample with its
//! transcript and provenance (and the overwrite confirmation), the cloning
//! check, and the ops' and tools' arguments. The audio.cpp container is the
//! audio suites' stand-in ([`crate::support::audio_world`]); every clip is
//! synthetic.

use lmgw_core::voice_lab::wav::pcm16_mono;
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, Request, ResponseTemplate};

use crate::support::audio_world::{world, World};
use crate::support::audiocpp_gguf;

/// Rounds, saves, provenance, overwrite and the cloning check, over HTTP.
mod flow;
/// Saves and the Audio lab's writes at once, the overwrite, the transcript
/// as sent, a records file lmgw cannot read.
mod library;
/// The ops' and the tools' arguments.
mod ops;
/// A round stopped, discarded while it renders, and Clear.
mod stops;
/// The sentence table, the owner's sentences, trimming and the name check.
mod units;

/// 24 kHz: 0.5 s of silence, 1 s of a tone, 0.3 s of silence.
pub(crate) fn padded_tone() -> Vec<u8> {
    let rate = 24_000usize;
    let mut s = vec![0i16; rate / 2];
    s.extend((0..rate).map(|i| {
        let t = i as f64 / rate as f64;
        ((t * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16
    }));
    s.extend(std::iter::repeat_n(0i16, rate * 3 / 10));
    pcm16_mono(rate as u32, &s)
}

/// A world with a Qwen3-TTS VoiceDesign row `design` (task vdes) and an
/// OmniVoice row `clone`, whose container answers every speech request with
/// [`padded_tone`]; the bodies it was sent are in [`World::sent`].
pub(crate) async fn lab_world() -> World {
    lab_world_answering_after(std::time::Duration::ZERO).await
}

/// [`lab_world`], its container answering each speech request after
/// `delay`.
pub(crate) async fn lab_world_answering_after(delay: std::time::Duration) -> World {
    let w = world().await;
    w.row_with(
        "design",
        "qwen3_tts",
        |r| audiocpp_gguf::qwen3(r, "voice_design"),
        |row| row.task = "vdes".into(),
    )
    .await;
    w.row("clone", "omnivoice", |_| {}).await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_bytes(padded_tone())
                .set_delay(delay)
        })
        .mount(&w.container)
        .await;
    w
}

pub(crate) async fn post(w: &World, p: &str, body: Value) -> (u16, Value) {
    let resp =
        w.gw.client()
            .post(format!("{}{p}", w.gw))
            .json(&body)
            .send()
            .await
            .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

pub(crate) async fn get(w: &World, p: &str) -> (u16, Value) {
    let resp =
        w.gw.client()
            .get(format!("{}{p}", w.gw))
            .send()
            .await
            .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

pub(crate) async fn bytes(w: &World, p: &str) -> (u16, Option<String>, Vec<u8>) {
    let resp =
        w.gw.client()
            .get(format!("{}{p}", w.gw))
            .send()
            .await
            .unwrap();
    let status = resp.status().as_u16();
    let ty = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    (status, ty, resp.bytes().await.unwrap().to_vec())
}
