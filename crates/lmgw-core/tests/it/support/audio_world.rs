//! A gateway with local audio rows on one audio.cpp container stand-in (the
//! audio-class gap suites): every start lands on the same wiremock server,
//! `podman` starts nothing and counts its `run`s, and the rows' packages are
//! synthetic ([`super::audiocpp_gguf`]) in a temporary models dir. The
//! stand-in judges a speech request by the `server.json` its model was
//! started with, as audio.cpp's engine would ([`super::audiocpp_options`]).

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAudioModel};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::audiocpp_options::{self, Served};
use crate::common::{serve, Gw};

/// A `podman` that starts nothing, counts its `run`s and keeps what each
/// model was started with.
#[derive(Default)]
pub struct Podman {
    pub runs: Mutex<usize>,
    /// Model id -> its `server.json`'s family and default request options.
    pub served: Mutex<HashMap<String, Served>>,
}

#[async_trait::async_trait]
impl CommandRunner for Podman {
    async fn run(&self, _program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        if args.first().map(String::as_str) == Some("run") {
            *self.runs.lock().unwrap() += 1;
            let model = args.iter().find_map(|a| a.strip_prefix("lmgw.model="));
            if let (Some(model), Some(served)) = (model, audiocpp_options::served(args)) {
                self.served
                    .lock()
                    .unwrap()
                    .insert(model.to_string(), served);
            }
        }
        Ok(CmdOutput {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

pub struct World {
    pub state: SharedState,
    pub gw: Gw,
    pub container: MockServer,
    pub podman: Arc<Podman>,
    pub models: tempfile::TempDir,
}

/// The gateway, one audio.cpp container stand-in every start lands on, and
/// a models dir.
pub async fn world() -> World {
    let container = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&container)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    let podman = Arc::new(Podman::default());
    let port = container.address().port();
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        podman.clone(),
        reqwest::Client::new(),
        Arc::new(move || Ok(port)),
    )));
    let models = tempfile::tempdir().unwrap();
    let mut s = lmgw_core::config::Settings::default();
    s.vram.load_timeout_seconds = 2;
    s.audio.models_dir = models.path().display().to_string();
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    World {
        state,
        gw,
        container,
        podman,
        models,
    }
}

/// An offline TTS row `model_id` of `family` on `<models>/<model_id>/`.
pub fn tts_row(model_id: &str, family: &str) -> NewAudioModel {
    NewAudioModel {
        model_id: model_id.into(),
        family: family.into(),
        path: model_id.into(),
        task: "tts".into(),
        mode: "offline".into(),
        lazy: None,
        busy_timeout_ms: None,
        backend: None,
        threads: None,
        load_options: Default::default(),
        session_options: Default::default(),
        default_request_options: Default::default(),
        model_spec_override: None,
        config_id: None,
        weight_id: None,
        voice_presets: Default::default(),
        default_voice_preset: None,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

impl World {
    /// A TTS row `model_id` of `family` on `models/<model_id>/`, whose
    /// package `write` puts there.
    pub async fn row(&self, model_id: &str, family: &str, write: impl FnOnce(&Path)) {
        self.row_with(model_id, family, write, |_| {}).await;
    }

    /// [`Self::row`], with `tweak` applied to the row first.
    pub async fn row_with(
        &self,
        model_id: &str,
        family: &str,
        write: impl FnOnce(&Path),
        tweak: impl FnOnce(&mut NewAudioModel),
    ) {
        let root = self.models.path().join(model_id);
        std::fs::create_dir_all(&root).unwrap();
        write(&root);
        let mut row = tts_row(model_id, family);
        tweak(&mut row);
        store::insert_audio_model(&self.state.db, &row)
            .await
            .unwrap();
        self.state.reload_snapshot().await.unwrap();
    }

    pub async fn voices(&self, model_id: &str) -> Value {
        let resp = self
            .gw
            .client()
            .get(format!(
                "{}/v1/audio/voices?model=audio/{model_id}",
                self.gw
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.headers()["x-lmgw-voices-source"], "config");
        resp.json().await.unwrap()
    }

    pub async fn speak(&self, body: Value) -> reqwest::Response {
        self.gw
            .client()
            .post(format!("{}/v1/audio/speech", self.gw))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// The bodies the container was sent on `/v1/audio/speech`.
    pub async fn sent(&self) -> Vec<Value> {
        self.container
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/v1/audio/speech")
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    pub fn runs(&self) -> usize {
        *self.podman.runs.lock().unwrap()
    }

    /// Every speech request answered with a WAV — or refused, as audio.cpp
    /// refuses options its engine cannot take together.
    pub async fn answer_wav(&self) {
        let podman = self.podman.clone();
        Mock::given(method("POST"))
            .and(path("/v1/audio/speech"))
            .respond_with(move |req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
                let served = podman.served.lock().unwrap();
                body["model"]
                    .as_str()
                    .and_then(|m| served.get(m))
                    .and_then(|s| audiocpp_options::refusal(s, &body))
                    .unwrap_or_else(wav)
            })
            .mount(&self.container)
            .await;
    }
}

/// A minimal 24 kHz mono PCM16 WAV answer.
pub fn wav() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "audio/wav")
        .set_body_bytes(wav_bytes(24_000, 480))
}

/// A mono PCM16 WAV of `samples` silent samples at `rate`.
pub fn wav_bytes(rate: u32, samples: u32) -> Vec<u8> {
    let data = samples * 2;
    let mut b = Vec::with_capacity(44 + data as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    b.resize(44 + data as usize, 0);
    b
}
