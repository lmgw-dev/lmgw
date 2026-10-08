//! Live probes of what only the real audio.cpp packages can answer, for the
//! owner to run (audio-class gaps 9 and 8). Each prints what it measured
//! and fails when lmgw's tables disagree with what the engine did.
//!
//! - **Fish tags** (gap 9b): whether Fish Audio S2 renders a free-form
//!   inline tag (`[laughs]`) or reads it out. lmgw strips Fish's tags until
//!   this says otherwise (`audio::families::free_form_tags`). The tagged
//!   sentence is sent to the container's own port — through lmgw the tag
//!   would be stripped — and both answers are transcribed by a local ASR
//!   row: a rendered tag makes the answer longer and leaves no "laugh" in
//!   the transcript.
//!
//! - **Supertonic streaming** (gap 8): the same package as two rows, one
//!   offline and one in streaming mode, asked the same sentence warm — the
//!   time to the first audio, the whole time, and the audio's length of a
//!   WAV answer against an SSE stream. What the owner decides
//!   `mode=streaming` for Supertonic (and Pocket) by; it fails only when
//!   the two disagree on what was said by more than a quarter of its length
//!   or the stream starts no sooner than the WAV ends.
//!
//! - **Supertonic characters** (`supertonic_chars`): whether the engine
//!   says every character lmgw sends a Supertonic row as it is — the
//!   tripwire for lmgw's copy of what its tokenizer rewrites and
//!   decomposes.
//!
//! Gated twice: `#[ignore]`, and `LMGW_LIVE_AUDIO_PROBES=1`. Without the
//! variable a probe prints `SKIP` and passes. With it the owner asked for
//! the run, so anything missing fails the probe with the reason: podman,
//! the audio.cpp image, the packages below under the audio class's models
//! dir (read only, mounted as the class mounts it), or the free VRAM it
//! needs. That need is worked out per probe from the models it holds at
//! once, each figure with its source, and a shortfall names what holds the
//! memory now ([`crate::support::live_vram`]):
//!
//! ```sh
//! LMGW_LIVE_AUDIO_PROBES=1 LMGW_LIVE_AUDIO_MODELS_DIR=/path/to/audio/models \
//!   cargo test -p lmgw-core --test it audio_probes_live:: -- --ignored --nocapture
//! ```
//!
//! `LMGW_LIVE_AUDIO_IMAGE` overrides the class's default image,
//! `LMGW_LIVE_SUPERTONIC_VOICE` the Supertonic voice (`F5`). Containers
//! run under their own `container_prefix` and are removed at the end, even
//! on a panic. The sentences are the test's own; no voice clip is used.

use std::path::PathBuf;
use std::sync::Arc;

use lmgw_core::config::Settings;
use lmgw_core::runtime::registry::{Registry, TokioRunner};
use lmgw_core::runtime::Class;
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAudioModel};
use serde_json::{json, Value};

use crate::support::live_vram::{self, Measured, Phase};

/// Supertonic's characters against a real container.
mod supertonic_chars;

const PREFIX: &str = "lmgwprobe";

const FISH: (&str, &str, &str) = (
    "fish-probe",
    "fish_audio",
    "audio-cpp/audio.cpp-gguf/Fish-Audio-S2-Pro-GGUF",
);
const ASR: (&str, &str, &str) = (
    "asr-probe",
    "qwen3_asr",
    "audio-cpp/audio.cpp-gguf/Qwen3-ASR-1.7B-GGUF",
);

const SUPERTONIC: (&str, &str) = ("supertonic", "audio-cpp/audio.cpp-gguf/Supertonic-3-GGUF");

/// What the ASR row's package holds, as WP7's live gate measured it (the
/// same package; the test's own row has learned nothing).
const ASR_MEASURED: Measured = Measured {
    bytes: 3_343_000_000,
    source: "realtime design §9.4: qwen3-asr-1.7b learned 3.343 GB on an RTX 4090, 2026-10-01",
};

fn models_dir() -> Option<PathBuf> {
    std::env::var("LMGW_LIVE_AUDIO_MODELS_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

fn image() -> String {
    std::env::var("LMGW_LIVE_AUDIO_IMAGE")
        .ok()
        .filter(|i| !i.is_empty())
        .unwrap_or_else(|| Settings::default().audio.image)
}

/// Whether `test` runs (module doc): skipped without
/// `LMGW_LIVE_AUDIO_PROBES=1`; with it, a missing models dir, package,
/// podman or image fails it. Free VRAM is checked once its gateway holds
/// the rows ([`live_vram::require`]).
fn runs(test: &str, paths: &[&str]) -> bool {
    if std::env::var("LMGW_LIVE_AUDIO_PROBES").unwrap_or_default() != "1" {
        eprintln!("SKIP {test}: set LMGW_LIVE_AUDIO_PROBES=1 to run real audio.cpp containers");
        return false;
    }
    if let Some(why) = missing(paths) {
        panic!("{test} cannot run: {why}");
    }
    true
}

/// What a probe needing the packages at `paths` lacks here, or `None`.
fn missing(paths: &[&str]) -> Option<String> {
    let Some(dir) = models_dir() else {
        return Some("set LMGW_LIVE_AUDIO_MODELS_DIR to the audio class's models dir".into());
    };
    for path in paths {
        if !dir.join(path).is_dir() {
            return Some(format!("{} is not on this box", dir.join(path).display()));
        }
    }
    let image = image();
    if !std::process::Command::new("podman")
        .args(["image", "exists", &image])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Some(format!("podman cannot run here, or {image} is not pulled"));
    }
    None
}

/// Removes every container this file's prefix labelled, whatever happens.
struct Sweep;

impl Drop for Sweep {
    fn drop(&mut self) {
        if let Ok(out) = std::process::Command::new("podman")
            .args([
                "ps",
                "-aq",
                "--filter",
                &format!("label=lmgw.instance={PREFIX}"),
            ])
            .output()
        {
            for id in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                let _ = std::process::Command::new("podman")
                    .args(["rm", "-f", id])
                    .output();
            }
        }
    }
}

fn row(id: &str, family: &str, path: &str, task: &str, mode: &str) -> NewAudioModel {
    NewAudioModel {
        model_id: id.into(),
        family: family.into(),
        path: path.into(),
        task: task.into(),
        mode: mode.into(),
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

/// A gateway on real containers with `rows`, and its base URL.
async fn gateway(rows: &[NewAudioModel]) -> (SharedState, String) {
    let state = AppState::init_for_tests().await.unwrap();
    state.set_runtime_for_tests(Arc::new(Registry::new(
        Arc::new(TokioRunner),
        reqwest::Client::new(),
    )));
    let mut s = Settings {
        container_prefix: PREFIX.into(),
        ..Default::default()
    };
    s.audio.models_dir = models_dir().unwrap().display().to_string();
    s.audio.image = image();
    s.vram.load_timeout_seconds = 300;
    store::save_settings(&state.db, &s).await.unwrap();
    for r in rows {
        store::insert_audio_model(&state.db, r).await.unwrap();
    }
    state.reload_snapshot().await.unwrap();
    state.vram.set_probe(lmgw_core::vram::detect_probe());
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (state, base)
}

/// The seconds of audio in a WAV.
fn seconds(wav: &[u8]) -> f64 {
    let w = lmgw_core::realtime::audio::pcm::parse_wav(wav).expect("a WAV");
    w.samples.len() as f64 / f64::from(w.rate)
}

async fn speak(url: &str, body: Value) -> Vec<u8> {
    let resp = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.bytes().await.unwrap().to_vec();
    assert!(
        status.is_success(),
        "{url}: {status} {}",
        String::from_utf8_lossy(&bytes)
    );
    bytes
}

async fn transcribe(base: &str, wav: Vec<u8>) -> String {
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("probe.wav")
        .mime_str("audio/wav")
        .unwrap();
    let form = reqwest::multipart::Form::new()
        .text("model", format!("audio/{}", ASR.0))
        .part("file", part);
    let v: Value = reqwest::Client::new()
        .post(format!("{base}/v1/audio/transcriptions"))
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["text"].as_str().unwrap_or_default().to_string()
}

const PLAIN: &str = "That is the funniest thing I have heard all week. I really did not \
                     expect that.";
const TAGGED: &str = "That is the funniest thing I have heard all week. [laughs] I really \
                      did not expect that.";

#[tokio::test]
#[ignore = "runs real audio.cpp containers; set LMGW_LIVE_AUDIO_PROBES=1"]
async fn fish_free_form_tags_are_heard_or_read_out() {
    const TEST: &str = "fish_free_form_tags_are_heard_or_read_out";
    if !runs(TEST, &[FISH.2, ASR.2]) {
        return;
    }
    let _sweep = Sweep;
    let (state, base) = gateway(&[
        row(FISH.0, FISH.1, FISH.2, "tts", "offline"),
        row(ASR.0, ASR.1, ASR.2, "asr", "offline"),
    ])
    .await;
    // Fish speaks both sentences and is stopped before the ASR model
    // starts: one model at a time.
    live_vram::require(
        &state,
        TEST,
        &[
            Phase {
                what: "Fish, speaking both sentences",
                models: vec![(FISH.0, None)],
            },
            Phase {
                what: "the ASR model, once Fish is stopped",
                models: vec![(ASR.0, Some(ASR_MEASURED))],
            },
        ],
    )
    .await;

    // Through lmgw: starts the container, and the plain sentence.
    let plain = speak(
        &format!("{base}/v1/audio/speech"),
        json!({"model": format!("audio/{}", FISH.0), "input": PLAIN, "seed": 7,
               "response_format": "wav"}),
    )
    .await;
    // The container's own port: the tag reaches the engine as written.
    let port = state
        .runtime()
        .list()
        .into_iter()
        .find(|e| e.class == Class::Audio && e.model_id == FISH.0)
        .map(|e| e.port)
        .expect("the Fish container runs");
    let tagged = speak(
        &format!("http://127.0.0.1:{port}/v1/audio/speech"),
        json!({"model": FISH.0, "input": TAGGED, "seed": 7, "response_format": "wav"}),
    )
    .await;
    state.runtime().stop(Class::Audio, FISH.0, false).await.ok();

    let (plain_s, tagged_s) = (seconds(&plain), seconds(&tagged));
    let heard = transcribe(&base, tagged).await;
    let read_out = heard.to_lowercase().contains("laugh");
    let rendered = !read_out && tagged_s > plain_s + 0.3;
    eprintln!(
        "fish: plain {plain_s:.2} s, tagged {tagged_s:.2} s; the tagged answer transcribed: \
         {heard:?}; [laughs] {}",
        if rendered {
            "rendered as a sound"
        } else if read_out {
            "READ OUT as a word"
        } else {
            "not read out, but no longer either (inconclusive: maybe ignored)"
        }
    );

    // What lmgw does with Fish's tags today, from the package itself.
    let models = models_dir().unwrap();
    let gguf = lmgw_core::audio::files::row_gguf(&models, &models.join(FISH.2), None, FISH.1)
        .map(|(p, _)| p);
    let snap = state.snapshot();
    let fish = snap
        .audio_models
        .iter()
        .find(|m| m.model_id == FISH.0)
        .unwrap();
    let profile = lmgw_core::audio::profile::compute(fish, None, gguf.as_deref());
    let lmgw_keeps = profile.inline_tags == lmgw_core::audio::tags::TagMode::Free;
    assert_eq!(
        rendered,
        lmgw_keeps,
        "Fish {} free-form tags, but lmgw {} them: change \
         audio::families::free_form_tags(\"fish_audio\") to {rendered}",
        if rendered {
            "renders"
        } else {
            "does not render"
        },
        if lmgw_keeps { "keeps" } else { "strips" },
    );
}

/// One timed answer: to its first audio, to its end, and how much audio.
struct Timed {
    first_ms: u128,
    total_ms: u128,
    seconds: f64,
}

/// The offline row's WAV answer, timed.
async fn timed_wav(base: &str, model: &str, voice: &str) -> Timed {
    let t0 = std::time::Instant::now();
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/audio/speech"))
        .json(&json!({"model": model, "input": STREAMED, "voice": voice,
                      "response_format": "wav"}))
        .send()
        .await
        .unwrap();
    let first_ms = t0.elapsed().as_millis();
    assert!(resp.status().is_success(), "{model}: {}", resp.status());
    let wav = resp.bytes().await.unwrap();
    Timed {
        first_ms,
        total_ms: t0.elapsed().as_millis(),
        seconds: seconds(&wav),
    }
}

/// The streaming row's SSE answer, timed to its first `speech.audio.delta`.
async fn timed_stream(base: &str, model: &str, voice: &str) -> Timed {
    use base64::Engine as _;
    use futures::StreamExt;
    let t0 = std::time::Instant::now();
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/audio/speech"))
        .json(&json!({"model": model, "input": STREAMED, "voice": voice,
                      "stream_format": "sse", "response_format": "pcm"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{model}: {}", resp.status());
    let rate: u32 = resp
        .headers()
        .get("x-lmgw-sample-rate")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("x-lmgw-sample-rate, learned from the row's WAV answer");
    let mut first_ms = None;
    let mut text = String::new();
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.unwrap();
        if first_ms.is_none() && String::from_utf8_lossy(&chunk).contains("speech.audio.delta") {
            first_ms = Some(t0.elapsed().as_millis());
        }
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    let samples: usize = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter(|e| e["type"] == "speech.audio.delta")
        .filter_map(|e| {
            base64::engine::general_purpose::STANDARD
                .decode(e["audio"].as_str()?)
                .ok()
        })
        .map(|pcm| pcm.len() / 2)
        .sum();
    Timed {
        first_ms: first_ms.expect("at least one audio delta"),
        total_ms: t0.elapsed().as_millis(),
        seconds: samples as f64 / f64::from(rate),
    }
}

const STREAMED: &str = "Streaming speech starts while the rest of the sentence is still being \
                        made, so the listener hears the first words sooner.";

#[tokio::test]
#[ignore = "runs real audio.cpp containers; set LMGW_LIVE_AUDIO_PROBES=1"]
async fn supertonic_streaming_against_offline() {
    const TEST: &str = "supertonic_streaming_against_offline";
    if !runs(TEST, &[SUPERTONIC.1]) {
        return;
    }
    let _sweep = Sweep;
    let voice = std::env::var("LMGW_LIVE_SUPERTONIC_VOICE").unwrap_or_else(|_| "F5".into());
    let (offline, stream) = ("supertonic-offline", "supertonic-stream");
    let (state, base) = gateway(&[
        row(offline, SUPERTONIC.0, SUPERTONIC.1, "tts", "offline"),
        row(stream, SUPERTONIC.0, SUPERTONIC.1, "tts", "streaming"),
    ])
    .await;
    // Both rows are warmed and then timed warm, so both stay up together.
    live_vram::require(
        &state,
        TEST,
        &[Phase {
            what: "Supertonic, its offline and its streaming row side by side",
            models: vec![(offline, None), (stream, None)],
        }],
    )
    .await;
    let (offline_alias, stream_alias) = (format!("audio/{offline}"), format!("audio/{stream}"));

    // Warm both: the first request loads the model. The streaming row's
    // plain request answers a WAV too, which teaches its sample rate.
    timed_wav(&base, &offline_alias, &voice).await;
    let plain_on_stream = timed_wav(&base, &stream_alias, &voice).await;
    let wav = timed_wav(&base, &offline_alias, &voice).await;
    let sse = timed_stream(&base, &stream_alias, &voice).await;
    state
        .runtime()
        .stop(Class::Audio, offline, false)
        .await
        .ok();
    state.runtime().stop(Class::Audio, stream, false).await.ok();

    eprintln!(
        "supertonic ({voice}), warm:\n  offline WAV:   first audio {} ms, done {} ms, {:.2} s of \
         audio\n  streaming SSE: first audio {} ms, done {} ms, {:.2} s of audio\n  plain \
         request on the streaming row: {:.2} s of audio in {} ms",
        wav.first_ms,
        wav.total_ms,
        wav.seconds,
        sse.first_ms,
        sse.total_ms,
        sse.seconds,
        plain_on_stream.seconds,
        plain_on_stream.total_ms,
    );
    let ratio = sse.seconds / wav.seconds.max(0.01);
    assert!(
        (0.75..1.25).contains(&ratio),
        "the stream said {:.2} s against the WAV's {:.2} s",
        sse.seconds,
        wav.seconds
    );
    assert!(
        sse.first_ms < wav.total_ms,
        "the stream's first audio ({} ms) came no sooner than the whole WAV ({} ms)",
        sse.first_ms,
        wav.total_ms
    );
}
