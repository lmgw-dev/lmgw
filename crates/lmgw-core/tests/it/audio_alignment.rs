//! `POST /v1/audio/alignments` answers one typed shape whichever family
//! aligned: the answers below are what Qwen3's forced aligner and
//! CrisperWhisper's aligner sent for the same 24 kHz clip on 2026-10-22
//! (trimmed to a few words), and lmgw answers both in the same dialect.

use lmgw_api_types::audio_alignment::AudioAlignment;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::audio_backend::setup_audio_proxy;
use crate::support::audio_world::wav_bytes;

const TRANSCRIPT: &str = "Throughout the Vita Nuova, there is a strain like the first falling \
    murmur which reaches the ear in some remote meadow, and prepares us to look upon the sea.";

/// Qwen3 strips the punctuation off the word and states no sample rate.
fn qwen3() -> Value {
    json!({
        "text": TRANSCRIPT, "language": "en",
        "timing": {"wall_ms": 288.04, "audio_duration_ms": 8335, "rtf": 0.0345579},
        "words": [
            {"word": "Vita", "start": 0.64, "end": 0.8, "start_sample": 15360, "end_sample": 19200, "confidence": 0},
            {"word": "Nuova", "start": 0.8, "end": 1.12, "start_sample": 19200, "end_sample": 26880, "confidence": 0},
            {"word": "there", "start": 1.12, "end": 1.22667, "start_sample": 26880, "end_sample": 29440, "confidence": 0}
        ]
    })
}

/// CrisperWhisper keeps it.
fn crisperwhisper() -> Value {
    json!({
        "text": TRANSCRIPT, "language": "en",
        "timing": {"wall_ms": 412.5, "audio_duration_ms": 8335, "rtf": 0.0494901},
        "words": [
            {"word": "Vita", "start": 0.96, "end": 1.12, "start_sample": 23040, "end_sample": 26880, "confidence": 0},
            {"word": "Nuova,", "start": 1.12, "end": 1.68, "start_sample": 26880, "end_sample": 40320, "confidence": 0},
            {"word": "there", "start": 1.68, "end": 1.82, "start_sample": 40320, "end_sample": 43680, "confidence": 0}
        ]
    })
}

/// `POST /v1/audio/alignments` of `TRANSCRIPT` against a 24 kHz clip, on a
/// gateway whose upstream answers `answer`.
async fn align(answer: ResponseTemplate) -> reqwest::Response {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/alignments"))
        .respond_with(answer)
        .expect(1)
        .mount(&mock)
        .await;
    let (_state, base) = setup_audio_proxy(&mock.uri()).await;
    let form = reqwest::multipart::Form::new()
        .text("model", "my-asr")
        .text("text", TRANSCRIPT)
        .text("language", "en")
        .part(
            "file",
            reqwest::multipart::Part::bytes(wav_bytes(24_000, 480)).file_name("clip.wav"),
        );
    base.client()
        .post(format!("{base}/v1/audio/alignments"))
        .multipart(form)
        .send()
        .await
        .unwrap()
}

fn json_answer(body: &Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .set_body_json(body)
}

/// The words' spans in the transcript, cut out of it.
fn spans(a: &AudioAlignment) -> Vec<String> {
    let chars: Vec<char> = a.text.chars().collect();
    a.words
        .iter()
        .map(|w| {
            chars[w.char_start.unwrap()..w.char_end.unwrap()]
                .iter()
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn qwen3_and_crisperwhisper_answer_the_same_shape() {
    let mut got = Vec::new();
    for answer in [qwen3(), crisperwhisper()] {
        let resp = align(json_answer(&answer)).await;
        assert_eq!(resp.status().as_u16(), 200);
        let ct = resp.headers()["content-type"].to_str().unwrap().to_string();
        assert!(ct.starts_with("application/json"), "{ct}");
        got.push(resp.json::<AudioAlignment>().await.unwrap());
    }
    let (q, c) = (&got[0], &got[1]);
    for a in [q, c] {
        assert_eq!(a.text, TRANSCRIPT, "the client's text, not the echo");
        assert_eq!(a.language.as_deref(), Some("en"));
        assert_eq!(
            a.sample_rate,
            Some(24_000),
            "from the words' seconds and samples"
        );
        assert_eq!(spans(a), ["Vita", "Nuova", "there"]);
    }
    assert_eq!(q.words[1].word, "Nuova");
    assert_eq!(c.words[1].word, "Nuova,", "as the family reported it");
    assert_eq!(
        (q.words[1].char_start, q.words[1].char_end),
        (c.words[1].char_start, c.words[1].char_end)
    );
    assert_eq!(
        (q.words[1].char_start, q.words[1].char_end),
        (Some(20), Some(25))
    );
}

#[tokio::test]
async fn the_answer_is_exactly_the_documented_keys() {
    let resp = align(json_answer(&crisperwhisper())).await;
    let v: Value = resp.json().await.unwrap();
    assert_eq!(
        v["words"][1],
        json!({
            "word": "Nuova,", "start": 1.12, "end": 1.68,
            "start_sample": 26880, "end_sample": 40320, "confidence": 0.0,
            "char_start": 20, "char_end": 25
        })
    );
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["language", "sample_rate", "text", "timing", "words"]);
    // The Audio Lab's wall/RTF stats read the engine's own figures from here.
    assert_eq!(
        v["timing"],
        json!({"wall_ms": 412.5, "audio_duration_ms": 8335.0, "rtf": 0.0494901})
    );
}

#[tokio::test]
async fn an_answer_with_no_word_timings_is_a_gateway_error_naming_the_model() {
    let resp = align(json_answer(&json!({"text": TRANSCRIPT, "segments": []}))).await;
    assert_eq!(resp.status().as_u16(), 502);
    let v: Value = resp.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("qwen3-asr"), "{msg}");
    assert!(msg.contains("no `words` list"), "{msg}");
}

#[tokio::test]
async fn a_refusal_from_the_model_is_relayed_as_it_was() {
    let resp = align(ResponseTemplate::new(400).set_body_json(json!({
        "error": {"message": "the clip is not audio", "type": "invalid_request_error"}
    })))
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    let v: Value = resp.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("the clip is not audio"),
        "{v}"
    );
}
