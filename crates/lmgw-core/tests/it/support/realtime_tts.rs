//! Audio-out helpers for the realtime suites (realtime design §16): a
//! scripted TTS upstream, the TTS alias that points at it, and the clips it
//! answers with — cut from the committed LJSpeech fixture, at 24 kHz or
//! resampled to 22.05 kHz in the test (nothing new is committed).
//!
//! The fake is an axum app like the chat and ASR fakes: each
//! `/v1/audio/speech` request takes the next scripted [`Tts`] answer (or the
//! default clip), every request body is kept, and `/v1/audio/voices` lists
//! the voices it was made with.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use lmgw_core::config::{KeyPolicy, Protocol, Settings, UpstreamKind};
use lmgw_core::realtime::audio::pcm::{
    decode_pcm16, f32_to_pcm16, pcm16_to_f32, write_wav_pcm16_mono,
};
use lmgw_core::realtime::audio::resample::resample;
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use tokio::sync::Notify;

use super::realtime_audio::fixture;
use super::realtime_fakes::{chat_fake, gateway, next_event, open, send, ChatFake, Ws};

/// The TTS alias the audio-out suites set up.
pub const TTS_ALIAS: &str = "speak";

/// The upstream model id behind [`TTS_ALIAS`].
pub const TTS_MODEL: &str = "pocket-tts";

/// One speech answer.
#[derive(Clone)]
pub enum Tts {
    /// This WAV.
    Wav(Bytes),
    /// Hold the answer until the test calls `notify_one`, then say this.
    Held(Arc<Notify>, Bytes),
    Status(u16, Value),
}

#[derive(Default)]
pub struct TtsSeen {
    /// Every `/v1/audio/speech` body, in arrival order.
    pub bodies: Mutex<Vec<Value>>,
    /// When each of them arrived.
    pub times: Mutex<Vec<std::time::Instant>>,
    /// `/v1/audio/voices` requests, answered or not.
    pub voice_lists: std::sync::atomic::AtomicUsize,
}

impl TtsSeen {
    pub fn voice_lists(&self) -> usize {
        self.voice_lists.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl TtsSeen {
    pub fn count(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }

    pub fn body(&self, n: usize) -> Value {
        self.bodies.lock().unwrap()[n].clone()
    }
}

struct Fake {
    script: Mutex<VecDeque<Tts>>,
    default: Mutex<Bytes>,
    voices: Vec<String>,
    /// While set, `/v1/audio/voices` answers nothing until notified — a
    /// container that is up and does not answer.
    voices_held: Mutex<Option<Arc<Notify>>>,
    seen: Arc<TtsSeen>,
}

pub struct TtsFake {
    pub url: String,
    pub seen: Arc<TtsSeen>,
    fake: Arc<Fake>,
}

impl TtsFake {
    /// Queue the answer to the next request.
    pub fn push(&self, t: Tts) {
        self.fake.script.lock().unwrap().push_back(t);
    }

    /// What every request with nothing queued is answered with.
    pub fn set_default(&self, wav: Bytes) {
        *self.fake.default.lock().unwrap() = wav;
    }

    /// Hold every `/v1/audio/voices` answer until the returned notify fires
    /// (once per held request).
    pub fn hold_voices(&self) -> Arc<Notify> {
        let n = Arc::new(Notify::new());
        *self.fake.voices_held.lock().unwrap() = Some(n.clone());
        n
    }
}

/// A TTS fake listing `voices`, answering each clause with 300 ms of speech
/// at 24 kHz until told otherwise.
pub async fn tts_fake(voices: &[&str]) -> TtsFake {
    let seen = Arc::new(TtsSeen::default());
    let fake = Arc::new(Fake {
        script: Mutex::new(VecDeque::new()),
        default: Mutex::new(wav(&speech(300), 24_000)),
        voices: voices.iter().map(|v| v.to_string()).collect(),
        voices_held: Mutex::new(None),
        seen: seen.clone(),
    });
    let app = Router::new()
        .route("/v1/audio/speech", post(speak))
        .route("/v1/audio/voices", get(list))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    TtsFake {
        url: format!("http://{addr}"),
        seen,
        fake,
    }
}

async fn speak(State(f): State<Arc<Fake>>, Json(body): Json<Value>) -> Response {
    f.seen.bodies.lock().unwrap().push(body);
    f.seen.times.lock().unwrap().push(std::time::Instant::now());
    let answer = f.script.lock().unwrap().pop_front();
    let answer = answer.unwrap_or_else(|| Tts::Wav(f.default.lock().unwrap().clone()));
    let bytes = match answer {
        Tts::Wav(b) => b,
        Tts::Held(n, b) => {
            n.notified().await;
            b
        }
        Tts::Status(s, body) => {
            return (StatusCode::from_u16(s).unwrap(), Json(body)).into_response()
        }
    };
    ([(header::CONTENT_TYPE, "audio/wav")], bytes).into_response()
}

async fn list(State(f): State<Arc<Fake>>) -> Response {
    f.seen
        .voice_lists
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let held = f.voices_held.lock().unwrap().clone();
    if let Some(n) = held {
        n.notified().await;
    }
    Json(json!({ "voices": f.voices })).into_response()
}

/// Add the TTS alias [`TTS_ALIAS`] on an audio.cpp-kind upstream at `fake`,
/// declared a text-to-speech model, and reload the snapshot.
pub async fn add_tts_alias(state: &SharedState, fake: &TtsFake) {
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "tts-audiocpp".into(),
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
            alias: TTS_ALIAS.into(),
            upstream_id: up,
            upstream_model_id: TTS_MODEL.into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({ "capabilities": {
                "task": "tts", "endpoints": ["/v1/audio/speech"], "source": "owner"
            } })),
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// Add `alias` as a cloud text-to-speech alias — an OpenAI-kind (`generic`)
/// upstream at `fake`, which has no voice list of its own to read — and
/// reload the snapshot.
pub async fn add_cloud_tts_alias(state: &SharedState, fake: &TtsFake, alias: &str) {
    add_cloud_tts(state, fake, alias, None).await;
}

/// [`add_cloud_tts_alias`], with the owner's `capabilities.speech` override
/// (`instructions`, `inline_tags`, `tags`).
pub async fn add_described_cloud_tts_alias(
    state: &SharedState,
    fake: &TtsFake,
    alias: &str,
    speech: Value,
) {
    add_cloud_tts(state, fake, alias, Some(speech)).await;
}

async fn add_cloud_tts(state: &SharedState, fake: &TtsFake, alias: &str, speech: Option<Value>) {
    let mut caps = json!({"task": "tts", "endpoints": ["/v1/audio/speech"], "source": "owner"});
    if let Some(s) = speech {
        caps["speech"] = s;
    }
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: format!("{alias}-cloud"),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
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
            upstream_model_id: "gpt-4o-mini-tts".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: Some(json!({ "capabilities": caps })),
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// `ms` of LJSpeech speech at 24 kHz, from the committed fixture — its
/// speech starts 300 ms in.
pub fn speech(ms: usize) -> Vec<i16> {
    let clip = fixture("en_complete_short.wav");
    let start = 300 * 24;
    clip[start..start + ms * 24].to_vec()
}

/// `samples` as a mono PCM16 WAV at `rate`.
pub fn wav(samples: &[i16], rate: u32) -> Bytes {
    Bytes::from(write_wav_pcm16_mono(samples, rate).unwrap())
}

/// `ms` of the same speech resampled to 22.05 kHz: what a TTS model with
/// another native rate answers.
pub fn speech_22k(ms: usize) -> Vec<i16> {
    let src = pcm16_to_f32(&speech(ms));
    let out = resample(&src, 24_000, 22_050, src.len()).unwrap();
    f32_to_pcm16(&out)
}

/// The voices the TTS fake of [`speech_gateway`] lists.
pub const VOICES: [&str; 2] = ["alba", "cosette"];

/// A gateway with the chat fake (`chatty`) and the TTS fake ([`TTS_ALIAS`],
/// listing [`VOICES`]): `realtime.tts_alias` on it and
/// `realtime.default_voice` `alba`, then `tweak`.
pub async fn speech_gateway(
    auth: bool,
    policy: Option<KeyPolicy>,
    tweak: impl FnOnce(&mut Settings),
) -> (SharedState, String, ChatFake, TtsFake) {
    let chat = chat_fake().await;
    let tts = tts_fake(&VOICES).await;
    let (state, addr) = gateway(&chat, auth, policy, |s| {
        s.realtime.tts_alias = TTS_ALIAS.into();
        s.realtime.default_voice = "alba".into();
        tweak(s);
    })
    .await;
    add_tts_alias(&state, &tts).await;
    (state, addr, chat, tts)
}

/// A session on `chatty` with audio output and `lead_ms` of lead, past its
/// `session.created` and `session.updated`; `extra` is merged into the
/// update. Returns the socket and the `session.updated`.
pub async fn spoken_session(
    addr: &str,
    headers: &[(&str, &str)],
    lead_ms: u32,
    extra: Value,
) -> (Ws, Value) {
    let mut ws = open(addr, "/v1/realtime?model=chatty", headers).await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    let mut update = json!({"type": "realtime", "output_modalities": ["audio"],
                            "lmgw": {"output_lead_ms": lead_ms}});
    for (k, v) in extra.as_object().unwrap() {
        update[k] = v.clone();
    }
    send(
        &mut ws,
        json!({"type": "session.update", "session": update}),
    )
    .await;
    let updated = next_event(&mut ws).await;
    assert_eq!(updated["type"], "session.updated", "{updated}");
    (ws, updated)
}

/// The PCM16 of every `response.output_audio.delta` in `events`, in order.
pub fn audio_of(events: &[Value]) -> Vec<i16> {
    events
        .iter()
        .filter(|e| e["type"] == "response.output_audio.delta")
        .flat_map(|e| decode_pcm16(e["delta"].as_str().unwrap()).unwrap())
        .collect()
}

/// The TTS rows: `(status, client_key)` per row of the TTS alias.
pub async fn tts_rows(state: &SharedState) -> Vec<(i64, Option<String>)> {
    sqlx::query_as(
        "SELECT status, client_key FROM request_logs WHERE requested_alias = ?1 AND class = \
         'audio' ORDER BY id",
    )
    .bind(TTS_ALIAS)
    .fetch_all(&state.db)
    .await
    .unwrap()
}
