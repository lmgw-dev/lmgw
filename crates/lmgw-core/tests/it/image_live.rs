//! One real image, end to end: lmgw starts an actual `sd-server` container on
//! this box's GPU, `POST /v1/images/generations` renders through it, and a
//! PNG comes back (image-generation design §10, last bullet).
//!
//! Every other image test fakes the container. This one exists because the
//! things that can only be wrong against the real binary are exactly the ones
//! that matter: the rendered argv, the `/models` rewrite, the readiness probe,
//! and whether a generation actually completes inside the route's timeout.
//!
//! Gated, and it says why it skipped: podman, the `master-cuda` image, the
//! three Z-Image-Turbo files under the class models dir, and enough free VRAM
//! for the 7.1 GiB the spike measured this pipeline to be resident (§12.4).
//! On a box missing any of them this prints a line and passes.
//!
//! The container runs under its own `container_prefix`, so it can never
//! collide with — or be collected by — the dev/prod instance's containers, and
//! it is removed at the end even if an assertion panics.

use std::path::PathBuf;
use std::sync::Arc;

use lmgw_core::config::Settings;
use lmgw_core::runtime::registry::{Registry, TokioRunner};
use lmgw_core::server::build_router;
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewImageModel};
use serde_json::{json, Value};

/// The class models dir the spike's weights already live in, in the
/// `<owner>/<repo>/<file>` layout the downloader will write (§14).
fn models_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share/lmgw/sdcpp")
}

const IMAGE: &str = "ghcr.io/leejet/stable-diffusion.cpp:master-cuda";
const DIFFUSION: &str = "leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf";
const VAE: &str = "Comfy-Org/z_image_turbo/split_files/vae/ae.safetensors";
const LLM: &str = "unsloth/Qwen3-4B-Instruct-2507-GGUF/Qwen3-4B-Instruct-2507-Q4_K_M.gguf";
/// Free VRAM this pipeline needs: 7.1 GiB resident (§12.4) plus the compute
/// buffers of the largest render below — the 512² one, whose measured peak is
/// 8.7 GiB — rounded up to the next whole GiB.
const NEEDS_FREE_MIB: i64 = 9 * 1024;
const PREFIX: &str = "lmgwtest";

/// Why this test cannot run here, or `None` when it can.
fn blocked() -> Option<String> {
    if !std::process::Command::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Some("podman is not available on this box".into());
    }
    if !std::process::Command::new("podman")
        .args(["image", "exists", IMAGE])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Some(format!("the image {IMAGE} is not pulled"));
    }
    for f in [DIFFUSION, VAE, LLM] {
        let p = models_dir().join(f);
        if !p.is_file() {
            return Some(format!("{} is not on this box", p.display()));
        }
    }
    let smi = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&smi.stdout);
    let (used, total) = line.trim().split_once(',')?;
    let used: i64 = used.trim().parse().ok()?;
    let total: i64 = total.trim().parse().ok()?;
    if total - used < NEEDS_FREE_MIB {
        return Some(format!(
            "only {} MiB of VRAM are free and this pipeline needs {NEEDS_FREE_MIB}",
            total - used
        ));
    }
    None
}

/// Removes every container this test's prefix labelled, whatever happens —
/// including on a panic, which a plain call at the end would miss.
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

#[tokio::test]
async fn a_real_sd_server_container_renders_a_png() {
    if let Some(why) = blocked() {
        eprintln!("SKIP a_real_sd_server_container_renders_a_png: {why}");
        return;
    }
    let _sweep = Sweep;
    let started = std::time::Instant::now();

    let state = AppState::init_for_tests().await.unwrap();
    state.set_runtime_for_tests(Arc::new(Registry::new(
        Arc::new(TokioRunner),
        reqwest::Client::new(),
    )));
    let mut s = Settings {
        // Its own prefix: this one starts a real container, and the name must
        // never collide with the dev or prod instance's.
        container_prefix: PREFIX.into(),
        ..Default::default()
    };
    s.image.models_dir = models_dir().display().to_string();
    // The eager load reads ~6.2 GB off disk before the server answers its
    // first probe; the spike measured 4-10 s warm, more on a cold page cache.
    s.vram.load_timeout_seconds = 300;
    store::save_settings(&state.db, &s).await.unwrap();

    let mut files = serde_json::Map::new();
    files.insert("diffusion_model".into(), json!(DIFFUSION));
    files.insert("vae".into(), json!(VAE));
    files.insert("llm".into(), json!(LLM));
    let mut args = serde_json::Map::new();
    args.insert("diffusion_fa".into(), json!(true));
    args.insert("cfg_scale".into(), json!(1.0));
    store::insert_image_model(
        &state.db,
        &NewImageModel {
            model_id: "z-image-turbo".into(),
            files,
            args,
            modes: vec![],
            edit: false,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            idle_seconds: 0,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();

    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // This box's real driver. `AppState::init_for_tests` installs a probe that
    // answers "no telemetry" precisely so the rest of the suite can never
    // depend on one — and the learned peak is measured from the driver, so
    // this is the one test that has to put the real one back.
    state.vram.set_probe(lmgw_core::vram::detect_probe());
    // The VRAM peak sampler (§9), which `spawn_background_tasks` starts in the
    // real gateway and `build_router` does not.
    tokio::spawn(lmgw_core::vram::peak::run(state.clone()));

    // 256² at 4 steps with a fixed seed: under a second of GPU time once the
    // pipeline is resident, so what this measures is the start plus the route.
    let body = json!({
        "model": "image/z-image-turbo",
        "prompt": "a red cube on a white table \
                   <sd_cpp_extra_args>{\"seed\":42,\"sample_params\":{\"sample_steps\":4}}\
                   </sd_cpp_extra_args>",
        "size": "256x256",
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    let elapsed = started.elapsed();
    eprintln!("container start + 256x256/4-step generation: {elapsed:.2?}");
    assert_eq!(status, 200, "{v}");

    // A real PNG, not an empty envelope. `iVBORw0KGgo` is the base64 of the
    // eight-byte PNG signature, so this is the signature check without pulling
    // a decoder into the dev-dependencies for one assertion.
    assert_eq!(v["output_format"], "png", "{v}");
    let b64 = v["data"][0]["b64_json"]
        .as_str()
        .unwrap_or_else(|| panic!("no b64_json in {v}"));
    assert!(
        b64.starts_with("iVBORw0KGgo"),
        "not a PNG: {}",
        &b64[..20.min(b64.len())]
    );
    assert!(
        b64.len() > 1_000,
        "a 256x256 PNG is longer than {} base64 bytes",
        b64.len()
    );

    // Exactly one log row, free, on this class.
    let logs = store::query_logs(&state.db, &store::LogFilter::default())
        .await
        .unwrap();
    assert_eq!(logs.len(), 1, "{logs:#?}");
    assert_eq!(logs[0].status, 200);
    assert_eq!(logs[0].class.as_deref(), Some("image"));
    assert_eq!(logs[0].requested_alias, "image/z-image-turbo");
    assert_eq!(logs[0].prompt_tokens, None);
    assert_eq!(logs[0].completion_tokens, None);

    // The operator path over the same warm container (WP4): `local_model_test`
    // dispatches the same request through the same handler, so against the
    // real binary this costs one more 256² render (~0.23 s, §12.3) and proves
    // the tool an owner actually reaches for, not just the route under it.
    let probe = lmgw_core::modelinfo::local_model_test(&state, "z-image-turbo", Some("image"))
        .await
        .unwrap();
    eprintln!("local_model_test target=image: {probe}");
    assert_eq!(probe["ok"], true, "{probe}");
    assert_eq!(probe["output_format"], "png");
    assert!(probe["bytes"].as_u64().unwrap_or(0) > 1_000, "{probe}");

    // The learned transient peak (§9), which is the whole reason this class
    // needs a mechanism no other class has. Both renders so far are 256², the
    // one size the spike measured "no measurable rise above idle" for — so
    // they teach nothing, which is the design working rather than failing: a
    // delta of zero is indistinguishable from a job that finished between two
    // samples, and recording it would claim this pipeline needs nothing.
    //
    // 512² is the smallest render that moves the driver's figure (8.7 GiB peak
    // against 7.1 GiB idle, §12.4), and it is what the gate above reserves
    // room for. The pipeline is warm, so this costs well under a second.
    let peak_now = || {
        state
            .snapshot()
            .image_models
            .iter()
            .find(|m| m.model_id == "z-image-turbo")
            .and_then(|m| m.peak_extra_bytes)
    };
    // What the two 256² renders taught, once it has settled — the sampler
    // writes a window after it closes, and a figure read while that is still
    // in flight would be credited to the render below. Two equal reads in a
    // row is the settle, bounded so a box that teaches nothing here does not
    // hang. (The spike called this size "no measurable rise above idle" at
    // 4 Hz; at 20 Hz there usually is one, and it is small — which is exactly
    // why the row keeps the largest window rather than the last.)
    let mut after_small = peak_now();
    for _ in 0..12 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let now = peak_now();
        if now == after_small {
            break;
        }
        after_small = now;
    }
    eprintln!("after two 256x256 renders: {after_small:?}");

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({
            "model": "image/z-image-turbo",
            "prompt": "a red cube on a white table \
                       <sd_cpp_extra_args>{\"seed\":42,\"sample_params\":{\"sample_steps\":8}}\
                       </sd_cpp_extra_args>",
            "size": "512x512",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "{:?}", resp.text().await);

    // The window closes on the first sample that sees the claim released, so
    // the bigger figure lands within a sampling interval of the response.
    // Waited for by *change* rather than by presence — the renders above have
    // already taught the row something, so polling for "is it set" would read
    // the old value back before this window had closed — and bounded, because
    // a sampled maximum is allowed to miss a transient shorter than one
    // interval and the row keeps what it had.
    for _ in 0..40 {
        if peak_now() != after_small {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let peak =
        peak_now().unwrap_or_else(|| panic!("three real generations taught the sampler nothing"));
    eprintln!(
        "learned peak above idle residency: {} ({peak} bytes)",
        lmgw_core::hf::fmt_bytes(peak)
    );
    assert!(peak > 0);

    // And it is charged where it matters: the resident says so, and the free
    // figure is smaller than the driver's by exactly that much.
    let v = state.vram.view(&state).await;
    let r = v
        .resident
        .iter()
        .find(|r| r.model == "z-image-turbo")
        .expect("the pipeline is resident");
    assert_eq!(r.peak_extra_bytes, Some(peak));
    assert!(
        r.note
            .as_deref()
            .unwrap_or_default()
            .contains("charged at admission"),
        "{:?}",
        r.note
    );

    // And the container is stopped rather than left holding 7 GiB.
    lmgw_core::runtime::lifecycle::shutdown(&state).await;
    assert!(state.runtime().list().is_empty());
}
