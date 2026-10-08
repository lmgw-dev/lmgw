//! Cloud audio rows record what the provider reports, and every audio row
//! what lmgw measured of the request itself (billable-units design §4.1–
//! §4.3). OpenAI's audio usage is only ever in the answer: a transcription's
//! JSON `usage` (tokens for the gpt-4o-* models, `duration` seconds for
//! `whisper-1`), a streamed transcription's `transcript.text.done`, and a
//! speech stream's `speech.audio.done`. `whisper-1`'s seconds are no tokens:
//! they become the row's `audio_in_ms`. lmgw measures a WAV upload's length
//! and a speech request's characters, and what the provider reports wins
//! over its own measurement. What neither gives — a binary answer's tokens,
//! an MP3's length — stays NULL and unpriced, never 0 and never an
//! estimate. A refused request carries no quantity. A local audio.cpp row's
//! reports are not read; its measured quantities are recorded, and it costs
//! a real 0.
//!
//! Usage objects are the shapes of OpenAI's API reference.

use bytes::Bytes;
use lmgw_core::config::{PriceScope, PriceUnit, Protocol, UpstreamKind};
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::realtime::audio::pcm::write_wav_pcm16_mono;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream, RequestLogRow};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{serve, Gw};
use crate::stream_usage::row_after;

/// `cloud-asr` (gpt-4o-mini-transcribe), `cloud-whisper` (whisper-1),
/// `cloud-tts` (gpt-4o-mini-tts) and `cloud-tts1` (tts-1) on an upstream of
/// `kind` at `mock`. The two token-billed aliases are priced at 1.00 in /
/// 10.00 out per 1M tokens, so micro-units are tokens × rate; `whisper-1`
/// and `tts-1` bill per minute and per character, and each test that
/// prices them says so ([`price`]).
async fn setup(mock: &MockServer, kind: UpstreamKind) -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let up_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "openai".into(),
            protocol: Protocol::Openai,
            kind,
            base_url: format!("{}/v1", mock.uri()),
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
    for (alias, model) in [
        ("cloud-asr", "gpt-4o-mini-transcribe"),
        ("cloud-whisper", "whisper-1"),
        ("cloud-tts", "gpt-4o-mini-tts"),
        ("cloud-tts1", "tts-1"),
    ] {
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: alias.into(),
                upstream_id: up_id,
                upstream_model_id: model.into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: None,
            },
        )
        .await
        .unwrap();
    }
    let sheet = Prices {
        price_in: Some(1.0),
        price_out: Some(10.0),
        source: PriceSource::Manual,
        ..Default::default()
    };
    for alias in ["cloud-asr", "cloud-tts"] {
        store::upsert_price(
            &state.db,
            PriceScope::Alias,
            alias,
            PriceUnit::PerMtok,
            &sheet,
            None,
            None,
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;
    (state, base)
}

/// A manual non-token price of `rate` per `unit` on `alias`.
async fn price(state: &SharedState, alias: &str, unit: PriceUnit, rate: f64) {
    let sheet = Prices {
        source: PriceSource::Manual,
        ..Default::default()
    };
    store::upsert_price(
        &state.db,
        PriceScope::Alias,
        alias,
        unit,
        &sheet,
        Some(rate),
        None,
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// The example `usage` of OpenAI's transcription object.
fn token_usage() -> Value {
    json!({"type": "tokens", "input_tokens": 14,
           "input_token_details": {"text_tokens": 10, "audio_tokens": 4},
           "output_tokens": 101, "total_tokens": 115})
}

async fn answer(mock: &MockServer, route: &str, reply: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(reply)
        .mount(mock)
        .await;
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// A 16 kHz mono PCM16 WAV of `ms` milliseconds of silence.
fn wav(ms: usize) -> Vec<u8> {
    write_wav_pcm16_mono(&vec![0i16; ms * 16], 16_000).unwrap()
}

/// A multipart transcription upload of `file` named `name` for `alias`.
fn upload_file(alias: &str, file: Vec<u8>, name: &str) -> reqwest::multipart::Form {
    reqwest::multipart::Form::new()
        .text("model", alias.to_string())
        .part(
            "file",
            reqwest::multipart::Part::bytes(file).file_name(name.to_string()),
        )
}

/// A multipart transcription upload for `alias`, plus `extra` text fields;
/// its file is no WAV lmgw can read, so nothing is measured.
fn upload(alias: &str, extra: &[(&str, &str)]) -> reqwest::multipart::Form {
    let mut form = reqwest::multipart::Form::new().text("model", alias.to_string());
    for (k, v) in extra {
        form = form.text(k.to_string(), v.to_string());
    }
    form.part(
        "file",
        reqwest::multipart::Part::bytes(b"RIFFaudio".to_vec()).file_name("in.wav"),
    )
}

async fn post_transcription(base: &Gw, form: reqwest::multipart::Form) -> reqwest::Response {
    base.client()
        .post(format!("{base}/v1/audio/transcriptions"))
        .multipart(form)
        .send()
        .await
        .unwrap()
}

async fn transcribe(base: &Gw, form: reqwest::multipart::Form) -> reqwest::Response {
    let resp = post_transcription(base, form).await;
    assert_eq!(resp.status(), 200);
    resp
}

async fn speak(base: &Gw, body: Value) -> reqwest::Response {
    base.client()
        .post(format!("{base}/v1/audio/speech"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// "The quick brown fox." — 20 characters.
const FOX: &str = "The quick brown fox.";

fn assert_tokens(row: &RequestLogRow, prompt: i64, completion: i64) {
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens),
        (Some(prompt), Some(completion)),
        "{row:?}"
    );
    assert_eq!(row.cost_micro, Some(prompt + completion * 10), "{row:?}");
}

fn assert_unpriced(row: &RequestLogRow) {
    assert_eq!(
        (row.prompt_tokens, row.completion_tokens),
        (None, None),
        "{row:?}"
    );
    assert_eq!(row.cost_micro, None, "unknown is NULL, never 0: {row:?}");
}

fn assert_no_quantities(row: &RequestLogRow) {
    assert_eq!(
        (row.audio_in_ms, row.chars_in, row.images_out),
        (None, None, None),
        "{row:?}"
    );
}

#[tokio::test]
async fn a_json_transcription_records_the_reported_tokens() {
    let mock = MockServer::start().await;
    let body = json!({"text": "Imagine the wildest idea.", "usage": token_usage()});
    answer(
        &mock,
        "/v1/audio/transcriptions",
        ResponseTemplate::new(200).set_body_json(body.clone()),
    )
    .await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;

    let resp = transcribe(&base, upload("cloud-asr", &[])).await;
    let got: Value = resp.json().await.unwrap();
    assert_eq!(got, body, "the answer is relayed untouched");
    let row = row_after(&state, 0).await;
    assert_tokens(&row, 14, 101);
    assert_eq!(row.audio_in_ms, None, "tokens are no duration");
}

/// `whisper-1` bills by the minute and says so: its seconds are no tokens,
/// they are the row's `audio_in_ms` — NULL-priced without a per-minute
/// rate, §3.3's golden with one (27 s at 0.006/min is 2 700 micro).
#[tokio::test]
async fn whisper_duration_usage_is_not_turned_into_tokens() {
    let mock = MockServer::start().await;
    answer(
        &mock,
        "/v1/audio/transcriptions",
        ResponseTemplate::new(200).set_body_json(json!({
            "text": "hello", "usage": {"type": "duration", "seconds": 27}
        })),
    )
    .await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;

    transcribe(&base, upload("cloud-whisper", &[])).await;
    let row = row_after(&state, 0).await;
    assert_unpriced(&row);
    assert_eq!(row.audio_in_ms, Some(27_000));

    price(&state, "cloud-whisper", PriceUnit::PerAudioMinute, 0.006).await;
    transcribe(&base, upload("cloud-whisper", &[])).await;
    let row = row_after(&state, 1).await;
    assert_eq!((row.prompt_tokens, row.completion_tokens), (None, None));
    assert_eq!(row.audio_in_ms, Some(27_000));
    assert_eq!(row.cost_micro, Some(2_700), "{row:?}");
    assert_eq!(row.cost_units_micro, Some(2_700));
    assert_eq!(row.price_per_audio_minute, Some(0.006));
    assert_eq!(row.price_source.as_deref(), Some("manual"));
}

/// `seconds` may carry a fraction: 27.4 s is 27 400 ms, rounded, and 2 740
/// micro at 0.006/min.
#[tokio::test]
async fn a_fractional_duration_is_kept_to_the_millisecond() {
    let mock = MockServer::start().await;
    answer(
        &mock,
        "/v1/audio/transcriptions",
        ResponseTemplate::new(200).set_body_json(json!({
            "text": "hello", "usage": {"type": "duration", "seconds": 27.4}
        })),
    )
    .await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;
    price(&state, "cloud-whisper", PriceUnit::PerAudioMinute, 0.006).await;

    transcribe(&base, upload("cloud-whisper", &[])).await;
    let row = row_after(&state, 0).await;
    assert_eq!(row.audio_in_ms, Some(27_400));
    assert_eq!(row.cost_micro, Some(2_740), "{row:?}");
}

/// An upstream that reports no duration: a WAV upload is priced from its
/// measured length (1.5 s at 0.006/min is 150 micro), an MP3 one stays
/// NULL — lmgw reads WAV only (Q4), and never guesses from the bytes.
#[tokio::test]
async fn a_wav_upload_is_priced_from_its_length_and_an_mp3_is_not() {
    let mock = MockServer::start().await;
    answer(
        &mock,
        "/v1/audio/transcriptions",
        ResponseTemplate::new(200).set_body_json(json!({"text": "hello"})),
    )
    .await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;
    price(&state, "cloud-whisper", PriceUnit::PerAudioMinute, 0.006).await;

    transcribe(&base, upload_file("cloud-whisper", wav(1_500), "in.wav")).await;
    let row = row_after(&state, 0).await;
    assert_eq!(row.audio_in_ms, Some(1_500));
    assert_eq!(row.cost_micro, Some(150), "{row:?}");

    let mp3 = b"ID3\x03\x00\x00\x00\x00\x00\x00fake-mp3-frames".to_vec();
    transcribe(&base, upload_file("cloud-whisper", mp3, "in.mp3")).await;
    let row = row_after(&state, 1).await;
    assert_eq!(row.audio_in_ms, None);
    assert_eq!(row.cost_micro, None, "unknown is NULL, never 0: {row:?}");
    assert_eq!(row.price_per_audio_minute, Some(0.006), "the rate is kept");
}

/// What the provider reports wins over lmgw's own measurement: it bills on
/// its own figure, and the row explains a bill (decision 14).
#[tokio::test]
async fn the_reported_duration_wins_over_the_measured_one() {
    let mock = MockServer::start().await;
    answer(
        &mock,
        "/v1/audio/transcriptions",
        ResponseTemplate::new(200).set_body_json(json!({
            "text": "hello", "usage": {"type": "duration", "seconds": 27.4}
        })),
    )
    .await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;

    transcribe(&base, upload_file("cloud-whisper", wav(1_500), "in.wav")).await;
    assert_eq!(row_after(&state, 0).await.audio_in_ms, Some(27_400));
}

/// A refused request was never billed: no quantity rides on its row, and
/// no fee (decision 15).
#[tokio::test]
async fn an_upstream_refusal_carries_no_quantities() {
    let mock = MockServer::start().await;
    let refusal = ResponseTemplate::new(400).set_body_json(json!({
        "error": {"message": "bad audio", "type": "invalid_request_error"}
    }));
    answer(&mock, "/v1/audio/transcriptions", refusal.clone()).await;
    answer(&mock, "/v1/audio/speech", refusal).await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;
    price(&state, "cloud-whisper", PriceUnit::PerAudioMinute, 0.006).await;
    price(&state, "cloud-tts1", PriceUnit::PerMchar, 15.0).await;
    price(&state, "cloud-tts1", PriceUnit::PerRequest, 0.01).await;

    let resp = post_transcription(&base, upload_file("cloud-whisper", wav(1_500), "in.wav")).await;
    assert_eq!(resp.status(), 400);
    let row = row_after(&state, 0).await;
    assert_no_quantities(&row);
    assert_eq!(row.cost_micro, None, "{row:?}");

    let resp = speak(
        &base,
        json!({"model": "cloud-tts1", "input": FOX, "voice": "alloy"}),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let row = row_after(&state, 1).await;
    assert_no_quantities(&row);
    assert_eq!(row.cost_micro, None, "{row:?}");
}

/// `response_format=text` answers the bare transcript: there is no usage to
/// read, and none is invented.
#[tokio::test]
async fn a_plain_text_transcript_stays_unpriced() {
    let mock = MockServer::start().await;
    answer(
        &mock,
        "/v1/audio/transcriptions",
        ResponseTemplate::new(200).set_body_raw("hello\n", "text/plain; charset=utf-8"),
    )
    .await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;

    let resp = transcribe(&base, upload("cloud-asr", &[("response_format", "text")])).await;
    assert_eq!(resp.text().await.unwrap(), "hello\n");
    assert_unpriced(&row_after(&state, 0).await);
}

/// `stream=true`: the deltas pass through as they came, and the
/// `transcript.text.done` event's usage lands on the row.
#[tokio::test]
async fn a_streamed_transcription_records_the_done_events_tokens() {
    let mock = MockServer::start().await;
    let stream = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
        json!({"type": "transcript.text.delta", "delta": "I see"}),
        json!({"type": "transcript.text.delta", "delta": " skies"}),
        json!({"type": "transcript.text.done", "text": "I see skies", "usage": token_usage()}),
    );
    answer(&mock, "/v1/audio/transcriptions", sse(stream.clone())).await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;

    let resp = transcribe(&base, upload("cloud-asr", &[("stream", "true")])).await;
    assert_eq!(resp.text().await.unwrap(), stream, "relayed byte for byte");
    let row = row_after(&state, 0).await;
    assert!(row.streamed);
    assert_tokens(&row, 14, 101);
}

/// `stream_format: "sse"`: the base64 audio deltas pass through, and
/// `speech.audio.done` carries the usage. The characters sent are recorded
/// beside the tokens.
#[tokio::test]
async fn a_speech_event_stream_records_the_done_events_tokens() {
    let mock = MockServer::start().await;
    let stream = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"type": "speech.audio.delta", "audio": "AAABAAIAAwA="}),
        json!({"type": "speech.audio.done",
               "usage": {"input_tokens": 14, "output_tokens": 101, "total_tokens": 115}}),
    );
    answer(&mock, "/v1/audio/speech", sse(stream.clone())).await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;

    let resp = speak(
        &base,
        json!({"model": "cloud-tts", "input": FOX, "voice": "alloy", "stream_format": "sse"}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), stream, "relayed byte for byte");
    let row = row_after(&state, 0).await;
    assert_tokens(&row, 14, 101);
    assert_eq!(row.chars_in, Some(20));
}

/// OpenAI's binary speech answer carries no usage, in the body or a header:
/// under a token price the row stays NULL rather than an estimate from the
/// input's length — its characters are recorded all the same.
#[tokio::test]
async fn a_binary_speech_answer_stays_unpriced_under_a_token_price() {
    let mock = MockServer::start().await;
    answer(
        &mock,
        "/v1/audio/speech",
        ResponseTemplate::new(200)
            .insert_header("content-type", "audio/mpeg")
            .set_body_bytes(b"ID3fake-mp3".to_vec()),
    )
    .await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;

    let resp = speak(
        &base,
        json!({"model": "cloud-tts", "input": FOX, "voice": "alloy"}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(&resp.bytes().await.unwrap()[..], b"ID3fake-mp3");
    let row = row_after(&state, 0).await;
    assert_unpriced(&row);
    assert_eq!(
        row.chars_in,
        Some(20),
        "measured, never estimated into tokens"
    );
}

/// `tts-1` bills per character: the same binary answer is priced from the
/// characters sent, 20 at 15 per 1M = 300 micro, plus its request fee.
#[tokio::test]
async fn a_binary_speech_answer_is_priced_per_character() {
    let mock = MockServer::start().await;
    answer(
        &mock,
        "/v1/audio/speech",
        ResponseTemplate::new(200)
            .insert_header("content-type", "audio/mpeg")
            .set_body_bytes(b"ID3fake-mp3".to_vec()),
    )
    .await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;
    price(&state, "cloud-tts1", PriceUnit::PerMchar, 15.0).await;

    let resp = speak(
        &base,
        json!({"model": "cloud-tts1", "input": FOX, "voice": "alloy"}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    resp.bytes().await.unwrap();
    let row = row_after(&state, 0).await;
    assert_eq!(row.chars_in, Some(20));
    assert_eq!(row.cost_micro, Some(300), "{row:?}");
    assert_eq!(row.price_per_mchar, Some(15.0));
    assert_eq!(row.cost_in_micro, None, "no token row, no token part");

    price(&state, "cloud-tts1", PriceUnit::PerRequest, 0.001).await;
    let resp = speak(
        &base,
        json!({"model": "cloud-tts1", "input": FOX, "voice": "alloy"}),
    )
    .await;
    resp.bytes().await.unwrap();
    let row = row_after(&state, 1).await;
    assert_eq!(
        row.cost_micro,
        Some(1_300),
        "the answered request pays its fee"
    );
}

/// The in-process transcription (the Chat's attachments and dictation, a
/// realtime turn) reads the body itself, and records the same tokens.
#[tokio::test]
async fn an_in_process_transcription_records_the_reported_tokens() {
    let mock = MockServer::start().await;
    answer(
        &mock,
        "/v1/audio/transcriptions",
        ResponseTemplate::new(200).set_body_json(json!({"text": "hi", "usage": token_usage()})),
    )
    .await;
    let (state, _base) = setup(&mock, UpstreamKind::Generic).await;

    let text = lmgw_core::proxy::transcribe(
        &state,
        "cloud-asr",
        Bytes::from_static(b"RIFFaudio"),
        "in.wav",
        "audio/wav",
    )
    .await
    .unwrap();
    assert_eq!(text, "hi");
    assert_tokens(&row_after(&state, 0).await, 14, 101);
}

/// A realtime turn, a dictation or an attachment uploads a WAV in process:
/// its row carries the WAV's length, and a per-minute rate prices it.
#[tokio::test]
async fn an_in_process_wav_transcription_records_its_length() {
    let mock = MockServer::start().await;
    answer(
        &mock,
        "/v1/audio/transcriptions",
        ResponseTemplate::new(200).set_body_json(json!({"text": "hi"})),
    )
    .await;
    let (state, _base) = setup(&mock, UpstreamKind::Generic).await;
    price(&state, "cloud-whisper", PriceUnit::PerAudioMinute, 0.006).await;

    let text = lmgw_core::proxy::transcribe(
        &state,
        "cloud-whisper",
        Bytes::from(wav(2_000)),
        "input.wav",
        "audio/wav",
    )
    .await
    .unwrap();
    assert_eq!(text, "hi");
    let row = row_after(&state, 0).await;
    assert_eq!(row.audio_in_ms, Some(2_000));
    assert_eq!(row.cost_micro, Some(200), "{row:?}");
}

/// A local audio.cpp row is free, and what it reports is left unread — even
/// should the engine one day put a `usage` in its answer. What lmgw measured
/// is recorded, for the statistics (decision 16), and the row costs a real
/// 0 (Q1).
#[tokio::test]
async fn a_local_audio_row_is_left_alone() {
    let mock = MockServer::start().await;
    answer(
        &mock,
        "/v1/audio/transcriptions",
        ResponseTemplate::new(200).set_body_json(json!({"text": "hi", "usage": {
            "type": "duration", "seconds": 99, "input_tokens": 14, "output_tokens": 101}})),
    )
    .await;
    let (state, base) = setup(&mock, UpstreamKind::AudioCpp).await;

    transcribe(&base, upload_file("cloud-asr", wav(1_500), "in.wav")).await;
    let row = row_after(&state, 0).await;
    assert_eq!((row.prompt_tokens, row.completion_tokens), (None, None));
    assert_eq!(
        row.audio_in_ms,
        Some(1_500),
        "measured, not the 99 s it said"
    );
    assert_eq!(row.cost_micro, Some(0));
    assert_eq!(row.price_source.as_deref(), Some("free_local"));
}

/// A stream that ends on its `speech.audio.done` without the closing blank
/// line still has that event counted.
#[tokio::test]
async fn an_unterminated_last_event_still_counts() {
    let mock = MockServer::start().await;
    let stream = format!(
        "data: {}\n\ndata: {}",
        json!({"type": "speech.audio.delta", "audio": "AAABAAIAAwA="}),
        json!({"type": "speech.audio.done",
               "usage": {"input_tokens": 14, "output_tokens": 101, "total_tokens": 115}}),
    );
    answer(&mock, "/v1/audio/speech", sse(stream)).await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;

    let resp = speak(
        &base,
        json!({"model": "cloud-tts", "input": FOX, "voice": "alloy", "stream_format": "sse"}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    resp.bytes().await.unwrap();
    assert_tokens(&row_after(&state, 0).await, 14, 101);
}

/// Only the transcription routes are read as JSON: a task's answer — which
/// a remote audio.cpp fills with base64 audio — is relayed and never parsed,
/// whatever it holds, and nothing is measured of its request.
#[tokio::test]
async fn a_task_answer_is_not_read() {
    let mock = MockServer::start().await;
    answer(
        &mock,
        "/v1/tasks/run",
        ResponseTemplate::new(200).set_body_json(json!({"audio": "AAAA", "usage": token_usage()})),
    )
    .await;
    let (state, base) = setup(&mock, UpstreamKind::Generic).await;

    let resp = base
        .client()
        .post(format!("{base}/v1/tasks/run"))
        .json(&json!({"model": "cloud-asr", "request": {"task": "separate"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.bytes().await.unwrap();
    let row = row_after(&state, 0).await;
    assert_unpriced(&row);
    assert_no_quantities(&row);
}
