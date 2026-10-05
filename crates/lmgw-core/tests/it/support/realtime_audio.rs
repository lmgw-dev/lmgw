//! Audio-in helpers for the realtime suites (realtime design §16): a
//! scripted ASR upstream, the ASR alias that points at it, and the committed
//! synthetic fixtures streamed as `input_audio_buffer.append` events.
//!
//! The ASR fake is an axum app like the chat fake (`realtime_fakes`): each
//! `/v1/audio/transcriptions` request takes the next scripted [`Asr`] answer,
//! and every upload is kept so a test can read the WAV the gateway sent.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::realtime::audio::pcm::{encode_pcm16, f32_to_pcm16, parse_wav};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use tokio::sync::Notify;

use super::realtime_fakes::{send, Ws};

/// The ASR alias the audio suites set up.
pub const ASR_ALIAS: &str = "hear";

/// One transcription answer.
#[derive(Clone)]
pub enum Asr {
    Text(&'static str),
    /// Hold the answer until the test calls `notify_one`, then say this.
    HeldText(Arc<Notify>, &'static str),
    Status(u16, Value),
    /// Hold the answer until the test calls `notify_one`, then fail with
    /// this status and body.
    HeldStatus(Arc<Notify>, u16, Value),
}

#[derive(Default)]
pub struct AsrSeen {
    /// Every upload's raw multipart body, in arrival order.
    pub bodies: Mutex<Vec<Bytes>>,
}

impl AsrSeen {
    pub fn count(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }

    /// The WAV inside upload `n`: its sample rate, channels and samples.
    pub fn wav(&self, n: usize) -> (u32, u16, usize) {
        let body = self.bodies.lock().unwrap()[n].clone();
        let at = body
            .windows(4)
            .position(|w| w == b"RIFF")
            .expect("the upload carries a WAV");
        let wav = parse_wav(&body[at..]).expect("the WAV parses");
        (wav.rate, wav.channels, wav.samples.len())
    }
}

struct Fake {
    script: Mutex<VecDeque<Asr>>,
    seen: Arc<AsrSeen>,
}

pub struct AsrFake {
    pub url: String,
    pub seen: Arc<AsrSeen>,
    fake: Arc<Fake>,
}

impl AsrFake {
    /// Queue the answer to the next upload; with nothing queued the fake
    /// says "hello".
    pub fn push(&self, a: Asr) {
        self.fake.script.lock().unwrap().push_back(a);
    }
}

pub async fn asr_fake() -> AsrFake {
    let seen = Arc::new(AsrSeen::default());
    let fake = Arc::new(Fake {
        script: Mutex::new(VecDeque::new()),
        seen: seen.clone(),
    });
    let app = Router::new()
        .route("/v1/audio/transcriptions", post(transcribe))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    AsrFake {
        url: format!("http://{addr}"),
        seen,
        fake,
    }
}

async fn transcribe(State(f): State<Arc<Fake>>, body: Bytes) -> Response {
    f.seen.bodies.lock().unwrap().push(body);
    let answer = f
        .script
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(Asr::Text("hello"));
    let text = match answer {
        Asr::Text(t) => t,
        Asr::HeldText(n, t) => {
            n.notified().await;
            t
        }
        Asr::Status(s, body) => {
            return (StatusCode::from_u16(s).unwrap(), Json(body)).into_response()
        }
        Asr::HeldStatus(n, s, body) => {
            n.notified().await;
            return (StatusCode::from_u16(s).unwrap(), Json(body)).into_response();
        }
    };
    Json(json!({ "text": text })).into_response()
}

/// Add the ASR alias [`ASR_ALIAS`] on an audio.cpp-kind upstream at `fake`,
/// declared a speech-to-text model, and reload the snapshot.
pub async fn add_asr_alias(state: &SharedState, fake: &AsrFake) {
    add_asr_alias_named(state, fake, ASR_ALIAS).await;
}

/// [`add_asr_alias`] under the name `alias`, on an upstream of its own.
pub async fn add_asr_alias_named(state: &SharedState, fake: &AsrFake, alias: &str) {
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: if alias == ASR_ALIAS {
                "audiocpp".into()
            } else {
                format!("audiocpp-{alias}")
            },
            protocol: Protocol::Openai,
            kind: UpstreamKind::AudioCpp,
            base_url: format!("{}/v1", fake.url),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 10_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: alias.into(),
            upstream_id: up,
            upstream_model_id: "nemotron-asr".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({ "capabilities": {
                "task": "asr", "endpoints": ["/v1/audio/transcriptions"], "source": "owner"
            } })),
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// A committed 24 kHz fixture (`tests/fixtures/realtime/audio/`) as PCM16.
pub fn fixture(name: &str) -> Vec<i16> {
    let path = format!(
        "{}/tests/fixtures/realtime/audio/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let wav = parse_wav(&bytes).unwrap();
    assert_eq!(wav.rate, 24_000, "{name}");
    f32_to_pcm16(&wav.samples)
}

/// `ms` of digital silence at 24 kHz.
pub fn silence(ms: usize) -> Vec<i16> {
    vec![0; ms * 24]
}

/// Stream `pcm` as `input_audio_buffer.append` events in the chunk sizes
/// clients use — 20, 40, 100 and 200 ms, in turn (the stock `@openai/agents`
/// capture sends 200 ms) — so frames straddle appends.
pub async fn stream(ws: &mut Ws, pcm: &[i16]) {
    let mut at = 0;
    for ms in [20usize, 40, 100, 200].iter().cycle() {
        if at >= pcm.len() {
            break;
        }
        let end = (at + ms * 24).min(pcm.len());
        append(ws, &pcm[at..end]).await;
        at = end;
    }
}

/// One `input_audio_buffer.append`.
pub async fn append(ws: &mut Ws, pcm: &[i16]) {
    send(
        ws,
        json!({"type": "input_audio_buffer.append", "audio": encode_pcm16(pcm)}),
    )
    .await;
}
