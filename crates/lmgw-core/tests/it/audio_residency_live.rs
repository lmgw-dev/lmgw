//! Learned audio residency against real audio.cpp containers on this box's
//! GPU (realtime design §9.4, WP7's verification gate).
//!
//! Every other residency test fakes the driver. What only the real engine can
//! answer:
//!
//! - **(a)** whether the learned figure — the reading after the answer and,
//!   since fix package B7, lmgw's own samples while the request ran
//!   (`vram::residency::SAMPLE_INTERVAL`) — covers what the container held
//!   while it worked: compared with this test's own 20 Hz sampler running
//!   through the whole request, within 5 % (the first run, at e6d6fbf, read
//!   pocket-tts 0.921 GB after its answer against a 0.981 GB maximum);
//! - **(b)** whether an eager row (`lazy: false`) has its weights on the card
//!   by the time its readiness route answers — otherwise eager rows must be
//!   charged by the lazy rule;
//! - **(c)** what the three voice models learn, against the per-process
//!   figures measured before WP7 (§3.1): nemotron-asr 1.48 GB, pocket-tts
//!   0.97 GB, qwen3-asr 3.28 GB;
//! - **(d)** the at-rest rule (`vram::residency::looks_loaded`): each lazy
//!   container, ready and not yet asked anything, holds no more than
//!   `BARE_CONTEXT_CEILING` plus half its files, and the figure it learns
//!   is above that line.
//!
//! A failed check does not stop the run: every model is measured, and the
//! failures are listed together at the end.
//!
//! Gated twice: `#[ignore]`, and `LMGW_LIVE_AUDIO_RESIDENCY=1`. Without the
//! variable the test prints `SKIP` and passes. With it the owner asked for
//! the run, so anything missing fails it with the reason: podman, the
//! audio.cpp image, the three model directories, or the free VRAM it needs.
//! That need is worked out from the one model it holds at a time, each
//! figure with its source, and a shortfall names what holds the memory now
//! ([`crate::support::live_vram`]):
//!
//! ```sh
//! LMGW_LIVE_AUDIO_RESIDENCY=1 LMGW_LIVE_AUDIO_MODELS_DIR=/path/to/audio/models \
//!   cargo test -p lmgw-core --test it audio_residency_live:: -- --ignored --nocapture
//! ```
//!
//! `LMGW_LIVE_AUDIO_MODELS_DIR` is the audio class's models dir (what is
//! mounted at `/models`); `LMGW_LIVE_AUDIO_IMAGE` overrides the class's
//! default image, `LMGW_LIVE_AUDIO_VOICE` the Pocket voice (`alba`). The
//! models are read only. Containers run under their own `container_prefix`,
//! one at a time, and are removed at the end even on a panic.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lmgw_core::config::Settings;
use lmgw_core::runtime::registry::{Registry, TokioRunner};
use lmgw_core::runtime::Class;
use lmgw_core::server::build_router;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAudioModel};
use lmgw_core::vram::residency::{looks_loaded, BARE_CONTEXT_CEILING};
use lmgw_core::vram::{GpuProbe, NvmlProbe, ProcFs, ProcTree};
use serde_json::json;

use crate::support::live_vram::{self, Measured, Phase};

const PREFIX: &str = "lmgwtest";
const GB: f64 = 1e9;

const TEST: &str = "real_audio_containers_teach_their_residency";

/// `(model id, family, path under the models dir, task, measured GB §3.1)`.
const MODELS: [(&str, &str, &str, &str, f64); 3] = [
    (
        "pocket-tts-german-q8-0",
        "pocket_tts",
        "audio-cpp/audio.cpp-gguf/PocketTTS-GGUF/german",
        "tts",
        0.97,
    ),
    (
        "nemotron-asr-q8-0",
        "nemotron_asr",
        "audio-cpp/audio.cpp-gguf/Nemotron-3.5-ASR-Streaming-0.6B-GGUF",
        "asr",
        1.48,
    ),
    (
        "qwen3-asr-1-7b-q8-0",
        "qwen3_asr",
        "audio-cpp/audio.cpp-gguf/Qwen3-ASR-1.7B-GGUF",
        "asr",
        3.28,
    ),
];

/// What each of [`MODELS`] holds, as this test's first passing run learned
/// it (the same packages; the test's own rows have learned nothing yet) —
/// the figure the VRAM check counts it at, beside admission's charge.
const LEARNED: [Measured; 3] = [
    Measured {
        bytes: 981_000_000,
        source: "realtime design §9.4: pocket-tts-german learned 0.981 GB on an RTX 4090, \
                 2026-10-01",
    },
    Measured {
        bytes: 1_455_000_000,
        source: "realtime design §9.4: nemotron-asr learned 1.455 GB on an RTX 4090, 2026-10-01",
    },
    Measured {
        bytes: 3_343_000_000,
        source: "realtime design §9.4: qwen3-asr-1.7b learned 3.343 GB on an RTX 4090, \
                 2026-10-01",
    },
];

/// What the eager qwen3-asr row (b) holds at `ready`, before any request.
const EAGER_AT_READY: Measured = Measured {
    bytes: 2_875_000_000,
    source: "realtime design §9.4: qwen3-asr eager held 2.875 GB at ready on an RTX 4090, \
             2026-10-01",
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

/// Whether the test runs (module doc): skipped without
/// `LMGW_LIVE_AUDIO_RESIDENCY=1`; with it, a missing models dir, model
/// directory, podman or image fails it. Free VRAM is checked once its
/// gateway holds the rows ([`live_vram::require`]).
fn runs() -> bool {
    if std::env::var("LMGW_LIVE_AUDIO_RESIDENCY").unwrap_or_default() != "1" {
        eprintln!("SKIP {TEST}: set LMGW_LIVE_AUDIO_RESIDENCY=1 to run real audio.cpp containers");
        return false;
    }
    if let Some(why) = missing() {
        panic!("{TEST} cannot run: {why}");
    }
    true
}

/// What the test lacks here, or `None`.
fn missing() -> Option<String> {
    let Some(dir) = models_dir() else {
        return Some("set LMGW_LIVE_AUDIO_MODELS_DIR to the audio class's models dir".into());
    };
    for (_, _, path, _, _) in MODELS {
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

/// Removes every container this test's prefix labelled, whatever happens.
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

/// The host PID of a running container, by name.
fn host_pid(name: &str) -> Option<u32> {
    let out = std::process::Command::new("podman")
        .args(["inspect", "--format", "{{.State.Pid}}", name])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .ok()
        .filter(|p| *p > 0)
}

/// What the driver lists for a container's processes right now.
fn container_bytes(probe: &NvmlProbe, root: u32) -> Option<u64> {
    let pids: Vec<u32> = std::iter::once(root)
        .chain(ProcFs::default().descendants(root))
        .collect();
    let listed = probe.processes(&pids, &[]).ok()?;
    let mine: Vec<_> = listed.iter().filter(|p| pids.contains(&p.pid)).collect();
    (!mine.is_empty()).then(|| mine.iter().filter_map(|p| p.bytes).sum())
}

/// The largest figure the driver listed for `container` while `stop` was
/// unset, read every 50 ms (`vram::peak::IN_FLIGHT_INTERVAL`).
fn sampler(container: String, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<u64> {
    std::thread::spawn(move || {
        let probe = NvmlProbe::detect();
        let mut root = None;
        let mut max = 0;
        while !stop.load(Ordering::Relaxed) {
            if root.is_none() {
                root = host_pid(&container);
            }
            if let Some(b) = root.and_then(|r| container_bytes(&probe, r)) {
                max = max.max(b);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        max
    })
}

fn row(id: &str, family: &str, path: &str, task: &str, lazy: Option<bool>) -> NewAudioModel {
    NewAudioModel {
        model_id: id.into(),
        family: family.into(),
        path: path.into(),
        task: task.into(),
        mode: "offline".into(),
        lazy,
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

fn learned(state: &SharedState, id: &str) -> Option<u64> {
    let snap = state.snapshot();
    let m = snap.audio_models.iter().find(|m| m.model_id == id)?;
    lmgw_core::vram::residency::learned(m, &snap.settings.audio)
}

async fn reading_after(state: &SharedState, before: u64) {
    for _ in 0..600 {
        if state.vram.residency_readings() > before {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no residency reading within 30 s");
}

/// Until the sampler's stretch after `before` stretches has ended.
async fn stretch_after(state: &SharedState, before: u64) {
    for _ in 0..600 {
        if state.vram.residency_stretches() > before {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no sampler stretch ended within 30 s");
}

/// One inference request for `id` — speech, or a transcription of `wav` —
/// read to the end. Returns the body, or why it failed.
async fn infer(base: &str, id: &str, task: &str, wav: &[u8]) -> Result<Vec<u8>, String> {
    let http = reqwest::Client::new();
    let resp = if task == "tts" {
        let voice = std::env::var("LMGW_LIVE_AUDIO_VOICE").unwrap_or_else(|_| "alba".into());
        http.post(format!("{base}/v1/audio/speech"))
            .json(&json!({"model": format!("audio/{id}"),
                          "input": "Guten Morgen. Wie geht es dir heute?",
                          "voice": voice, "response_format": "wav"}))
            .send()
            .await
            .unwrap()
    } else {
        let part = reqwest::multipart::Part::bytes(wav.to_vec())
            .file_name("speech.wav")
            .mime_str("audio/wav")
            .unwrap();
        let form = reqwest::multipart::Form::new()
            .text("model", format!("audio/{id}"))
            .part("file", part);
        http.post(format!("{base}/v1/audio/transcriptions"))
            .multipart(form)
            .send()
            .await
            .unwrap()
    };
    let status = resp.status();
    let body = resp.bytes().await.unwrap().to_vec();
    if !status.is_success() {
        return Err(format!("{id}: {status} {}", String::from_utf8_lossy(&body)));
    }
    Ok(body)
}

#[tokio::test]
#[ignore = "runs real audio.cpp containers; set LMGW_LIVE_AUDIO_RESIDENCY=1"]
async fn real_audio_containers_teach_their_residency() {
    if !runs() {
        return;
    }
    let _sweep = Sweep;
    let dir = models_dir().unwrap();

    let state = AppState::init_for_tests().await.unwrap();
    state.set_runtime_for_tests(Arc::new(Registry::new(
        Arc::new(TokioRunner),
        reqwest::Client::new(),
    )));
    let mut s = Settings {
        container_prefix: PREFIX.into(),
        ..Default::default()
    };
    s.audio.models_dir = dir.display().to_string();
    s.audio.image = image();
    s.vram.load_timeout_seconds = 300;
    store::save_settings(&state.db, &s).await.unwrap();
    for (id, family, path, task, _) in MODELS {
        store::insert_audio_model(&state.db, &row(id, family, path, task, None))
            .await
            .unwrap();
    }
    // (b)'s row: qwen3-asr loaded at start. Its 2.5 GB of weights stand far
    // above a CUDA context, so a reading at `ready` tells the two apart.
    let (_, family, path, task, _) = MODELS[2];
    store::insert_audio_model(
        &state.db,
        &row("qwen3-asr-eager", family, path, task, Some(false)),
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    // This box's real driver: `init_for_tests` installs one that answers
    // "no telemetry" so the rest of the suite never depends on a GPU.
    state.vram.set_probe(lmgw_core::vram::detect_probe());
    // One container at a time: each model is stopped before the next starts,
    // and the eager row comes up last.
    let phases: Vec<Phase> = MODELS
        .iter()
        .zip(LEARNED)
        .map(|((id, ..), learned)| Phase {
            what: id,
            models: vec![(*id, Some(learned))],
        })
        .chain(std::iter::once(Phase {
            what: "qwen3-asr-eager, loaded at start",
            models: vec![("qwen3-asr-eager", Some(EAGER_AT_READY))],
        }))
        .collect();
    live_vram::require(&state, TEST, &phases).await;

    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // (d), then (a) and (c), one model at a time: the container is started
    // and read at rest, then its first request loads the model with both
    // samplers watching.
    let mut failures: Vec<String> = Vec::new();
    let mut wav: Vec<u8> = Vec::new();
    for (id, _, path, task, measured_gb) in MODELS {
        let on_disk = walk(&dir.join(path));
        let snap = state.snapshot();
        let rt = lmgw_core::runtime::descriptor::model_runtime(&snap, Class::Audio, id).unwrap();
        let spec = lmgw_core::runtime::lifecycle::acquire_spec(&state, &snap, &rt);
        drop(state.runtime().acquire(&spec).await.unwrap());
        let container = lmgw_core::runtime::container_name(PREFIX, Class::Audio, id);
        let root = host_pid(&container).expect("the container runs");
        let bare = tokio::task::spawn_blocking(move || {
            container_bytes(&NvmlProbe::detect(), root).unwrap_or(0)
        })
        .await
        .unwrap();
        eprintln!(
            "{id}: {:.3} GB at rest, {:.3} GB on disk, the at-rest line {:.3} GB",
            bare as f64 / GB,
            on_disk as f64 / GB,
            (BARE_CONTEXT_CEILING + on_disk / 2) as f64 / GB
        );
        if looks_loaded(bare, on_disk) {
            failures.push(format!(
                "(d) {id}: {bare} bytes at rest read as loaded — BARE_CONTEXT_CEILING is too low"
            ));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let watching = sampler(container, stop.clone());
        let (readings, stretches) = (
            state.vram.residency_readings(),
            state.vram.residency_stretches(),
        );
        let body = infer(&base, id, task, &wav).await;
        let body = match body {
            Ok(b) => b,
            Err(why) => {
                stop.store(true, Ordering::Relaxed);
                watching.join().unwrap();
                failures.push(format!("(a) {why}"));
                state.runtime().stop(Class::Audio, id, false).await.ok();
                continue;
            }
        };
        if task == "tts" {
            wav = body;
        }
        reading_after(&state, readings).await;
        stretch_after(&state, stretches).await;
        stop.store(true, Ordering::Relaxed);
        let in_flight_max = watching.join().unwrap();
        let Some(got) = learned(&state, id) else {
            failures.push(format!("(a) {id} learned nothing"));
            state.runtime().stop(Class::Audio, id, false).await.ok();
            continue;
        };
        eprintln!(
            "{id}: learned {:.3} GB, 20 Hz in-flight max {:.3} GB ({:.1} %), §3.1 measured \
             {measured_gb:.2} GB",
            got as f64 / GB,
            in_flight_max as f64 / GB,
            100.0 * got as f64 / in_flight_max.max(1) as f64
        );
        if (got as f64) < 0.95 * in_flight_max as f64 {
            failures.push(format!(
                "(a) {id}: the learned figure ({got}) misses what the container held while it \
                 worked ({in_flight_max})"
            ));
        }
        let ratio = got as f64 / (measured_gb * GB);
        if !(0.8..1.25).contains(&ratio) {
            failures.push(format!(
                "(c) {id}: learned {:.3} GB against {measured_gb} GB measured",
                got as f64 / GB
            ));
        }
        if !looks_loaded(got, on_disk) {
            failures.push(format!(
                "(d) {id}: its loaded figure {got} is under the at-rest line — an adopted \
                 container holding it would not be taken as loaded"
            ));
        }
        state.runtime().stop(Class::Audio, id, false).await.unwrap();
    }

    // (b): an eager row's container, read the moment it is ready.
    let (_, _, path, _, _) = MODELS[2];
    let snap = state.snapshot();
    let rt = lmgw_core::runtime::descriptor::model_runtime(&snap, Class::Audio, "qwen3-asr-eager")
        .unwrap();
    let spec = lmgw_core::runtime::lifecycle::acquire_spec(&state, &snap, &rt);
    drop(state.runtime().acquire(&spec).await.unwrap());
    let name = lmgw_core::runtime::container_name(PREFIX, Class::Audio, "qwen3-asr-eager");
    let root = host_pid(&name).expect("the eager container runs");
    let at_ready = tokio::task::spawn_blocking(move || {
        container_bytes(&NvmlProbe::detect(), root).unwrap_or(0)
    })
    .await
    .unwrap();
    let on_disk: u64 = walk(&dir.join(path));
    eprintln!(
        "qwen3-asr-eager: {:.3} GB on the card at `ready`, {:.3} GB on disk",
        at_ready as f64 / GB,
        on_disk as f64 / GB
    );
    if (at_ready as f64) < 0.9 * on_disk as f64 {
        failures.push(
            "(b) an eager row's readiness route answered before its weights were loaded — \
             without a reading at rest, eager rows must be charged by the lazy rule"
                .into(),
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The bytes under `dir`, the way the plan sizes an audio model.
fn walk(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| match e.file_type() {
                    Ok(t) if t.is_dir() => walk(&e.path()),
                    Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
                    Err(_) => 0,
                })
                .sum()
        })
        .unwrap_or(0)
}
