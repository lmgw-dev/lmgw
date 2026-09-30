//! Runtime descriptor derivation (design §3.1, §6): per-model-vs-class-
//! settings override resolution, and the `RenderSpec` bridge into WP1's argv
//! renderer end to end.
//!
//! Mirrors `tests/it/runtime_argv.rs`'s golden-argv style for the render_spec
//! test — this is the same renderer, just reached through the descriptor
//! rather than a hand-built `RenderSpec`.
//!
//! The audio section (§3.6, "Audio config") covers the per-model
//! `server.json` renderer end to end: the single-model JSON shape against
//! the multi-model renderer's own shape for the same model, the
//! `<data_dir>/audiocpp/<slug>-<hash6>/server.json` path scheme, the atomic write,
//! and the full podman argv `render_spec` now produces for an audio model.

use std::path::Path;

use lmgw_core::config::{
    AudioModel, AudioSettings, AuxKind, AuxModel, ImageModel, ImageSettings, LlamaParams,
    LocalModel, RouterSettings, Settings, Snapshot,
};
use lmgw_core::ladder::Rung;
use lmgw_core::runtime::argv::podman_run_argv;
use lmgw_core::runtime::audio as audio_cfg;
use lmgw_core::runtime::audio::render_server_config;
use lmgw_core::runtime::descriptor::{model_runtime, model_runtime_at, model_runtimes};
use lmgw_core::runtime::{container_name, hash6, slug, Class};
use serde_json::Value;

fn local(model_id: &str, image: Option<&str>, extra: Option<Vec<&str>>, warm: bool) -> LocalModel {
    LocalModel {
        id: 1,
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        params: LlamaParams::default(),
        args: vec![],
        idle_seconds: 300,
        enabled: true,
        public: true,
        image: image.map(String::from),
        extra_run_args: extra.map(|v| v.into_iter().map(String::from).collect()),
        warm_start: warm,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

fn aux(model_id: &str, image: Option<&str>, extra: Option<Vec<&str>>, warm: bool) -> AuxModel {
    AuxModel {
        id: 2,
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        kind: AuxKind::Embed,
        pooling: None,
        ctx_size: None,
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        image: image.map(String::from),
        extra_run_args: extra.map(|v| v.into_iter().map(String::from).collect()),
        warm_start: warm,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

fn audio(model_id: &str, image: Option<&str>, warm: bool) -> AudioModel {
    AudioModel {
        id: 3,
        model_id: model_id.into(),
        family: "qwen3_tts".into(),
        path: "voices/qwen".into(),
        task: "tts".into(),
        mode: "offline".into(),
        lazy: None,
        busy_timeout_ms: None,
        load_options: Default::default(),
        session_options: Default::default(),
        default_request_options: Default::default(),
        model_spec_override: None,
        config_id: None,
        weight_id: None,
        voice_presets: Default::default(),
        default_voice_preset: None,
        enabled: true,
        image: image.map(String::from),
        extra_run_args: None,
        warm_start: warm,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

fn image(model_id: &str, image: Option<&str>, warm: bool) -> ImageModel {
    ImageModel {
        id: 4,
        model_id: model_id.into(),
        files: serde_json::json!({
            "diffusion_model": "leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf",
            "vae": "Comfy-Org/z_image_turbo/ae.safetensors"
        })
        .as_object()
        .cloned()
        .unwrap(),
        args: serde_json::json!({"diffusion_fa": true, "cfg_scale": 1.0})
            .as_object()
            .cloned()
            .unwrap(),
        modes: vec!["img_gen".into()],
        edit: false,
        enabled: true,
        image: image.map(String::from),
        extra_run_args: None,
        warm_start: warm,
        idle_seconds: 600,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        peak_extra_bytes: None,
        peak_learned_at: None,
    }
}

/// A snapshot with distinct, recognizable class-settings image/extra_run_args
/// per class, so an inherited value and an overridden one are never
/// confusable in an assertion failure.
fn snapshot(
    local_models: Vec<LocalModel>,
    aux_models: Vec<AuxModel>,
    audio_models: Vec<AudioModel>,
) -> Snapshot {
    snapshot_with_images(local_models, aux_models, audio_models, vec![])
}

fn snapshot_with_images(
    local_models: Vec<LocalModel>,
    aux_models: Vec<AuxModel>,
    audio_models: Vec<AudioModel>,
    image_models: Vec<ImageModel>,
) -> Snapshot {
    Snapshot {
        local_models,
        aux_models,
        audio_models,
        image_models,
        settings: Settings {
            router: RouterSettings {
                image: "class-chat-image".into(),
                extra_run_args: vec!["--chat-class-flag".into()],
                models_dir: "/srv/chat-models".into(),
                ..RouterSettings::default()
            },
            aux_router: RouterSettings {
                image: "class-aux-image".into(),
                extra_run_args: vec!["--aux-class-flag".into()],
                models_dir: "/srv/aux-models".into(),
                ..RouterSettings::default_aux()
            },
            audio: AudioSettings {
                image: "class-audio-image".into(),
                extra_run_args: vec!["--audio-class-flag".into()],
                models_dir: "/srv/audio-models".into(),
                ..AudioSettings::default()
            },
            image: ImageSettings {
                image: "class-image-image".into(),
                extra_run_args: vec!["--image-class-flag".into()],
                models_dir: "/srv/image-models".into(),
                ..ImageSettings::default()
            },
            ..Settings::default()
        },
        ..Snapshot::default()
    }
}

// ---------------------------------------------------------------------------
// Override vs inherit
// ---------------------------------------------------------------------------

#[test]
fn chat_and_aux_inherit_the_class_image_and_extra_run_args_when_unset() {
    let snap = snapshot(
        vec![local("m1", None, None, false)],
        vec![aux("a1", None, None, false)],
        vec![],
    );
    let chat = model_runtime(&snap, Class::Chat, "m1").unwrap();
    assert_eq!(chat.image, "class-chat-image");
    assert_eq!(chat.extra_run_args, vec!["--chat-class-flag".to_string()]);

    let aux = model_runtime(&snap, Class::Aux, "a1").unwrap();
    assert_eq!(aux.image, "class-aux-image");
    assert_eq!(aux.extra_run_args, vec!["--aux-class-flag".to_string()]);
}

#[test]
fn a_per_model_override_wins_over_the_class_settings() {
    let snap = snapshot(
        vec![local(
            "m1",
            Some("my/own-image"),
            Some(vec!["--only-mine"]),
            false,
        )],
        vec![],
        vec![],
    );
    let rt = model_runtime(&snap, Class::Chat, "m1").unwrap();
    assert_eq!(rt.image, "my/own-image");
    assert_eq!(rt.extra_run_args, vec!["--only-mine".to_string()]);
}

#[test]
fn audio_resolves_image_the_same_way_and_carries_no_llama_args() {
    let snap = snapshot(vec![], vec![], vec![audio("voice1", None, false)]);
    let rt = model_runtime(&snap, Class::Audio, "voice1").unwrap();
    assert_eq!(rt.image, "class-audio-image");
    assert!(rt.llama.is_none(), "audio has no CLI-flag model");
    assert!(
        rt.audio.is_some(),
        "the row is carried for the later config-dir renderer"
    );

    let snap = snapshot(
        vec![],
        vec![],
        vec![audio("voice2", Some("my/audio-image"), false)],
    );
    let rt = model_runtime(&snap, Class::Audio, "voice2").unwrap();
    assert_eq!(rt.image, "my/audio-image");
}

// ---------------------------------------------------------------------------
// warm_start / enumeration
// ---------------------------------------------------------------------------

#[test]
fn warm_start_flows_through_and_can_filter_the_boot_list() {
    let snap = snapshot(
        vec![
            local("cold", None, None, false),
            local("warm", None, None, true),
        ],
        vec![],
        vec![],
    );
    let warm: Vec<String> = model_runtimes(&snap)
        .into_iter()
        .filter(|r| r.warm_start)
        .map(|r| r.model_id)
        .collect();
    assert_eq!(warm, vec!["warm".to_string()]);
}

#[test]
fn model_runtimes_covers_every_class() {
    let snap = snapshot_with_images(
        vec![local("c1", None, None, false)],
        vec![aux("a1", None, None, false)],
        vec![audio("d1", None, false)],
        vec![image("i1", None, false)],
    );
    let all = model_runtimes(&snap);
    assert_eq!(all.len(), 4, "{all:?}");
    // Class order: chat, aux, audio, image — the order `ops::models` lists
    // them in, so a rendered list does not reshuffle between surfaces.
    assert_eq!(
        all.iter().map(|r| r.class).collect::<Vec<_>>(),
        vec![Class::Chat, Class::Aux, Class::Audio, Class::Image]
    );
    assert!(all
        .iter()
        .any(|r| r.class == Class::Chat && r.model_id == "c1"));
    assert!(all
        .iter()
        .any(|r| r.class == Class::Aux && r.model_id == "a1"));
    assert!(all
        .iter()
        .any(|r| r.class == Class::Audio && r.model_id == "d1"));
    assert!(all
        .iter()
        .any(|r| r.class == Class::Image && r.model_id == "i1"));
}

/// The image class resolves its overrides exactly like the other three — and
/// carries the row itself plus its **own** `idle_seconds`, which audio does
/// not have (design §3: a loaded pipeline is 7–13 GiB, so idle unload matters
/// more here than anywhere).
#[test]
fn image_resolves_image_and_keeps_its_own_idle_seconds() {
    let snap = snapshot_with_images(
        vec![],
        vec![],
        vec![],
        vec![
            image("inherits", None, false),
            ImageModel {
                extra_run_args: Some(vec!["--device".into(), "amd.com/gpu=all".into()]),
                ..image("overrides", Some("localhost/sdcpp:pinned"), true)
            },
        ],
    );
    let inherits = model_runtime(&snap, Class::Image, "inherits").unwrap();
    assert_eq!(inherits.image, "class-image-image");
    assert_eq!(inherits.extra_run_args, vec!["--image-class-flag"]);
    assert_eq!(inherits.idle_seconds, 600);
    assert!(!inherits.warm_start);
    assert!(inherits.llama.is_none() && inherits.audio.is_none());
    assert!(inherits.image_model.is_some());
    // Unprobed: the registry attaches the image's own vocabulary at start,
    // everything else renders against the embedded one.
    assert!(inherits.sdcpp_caps.is_none());

    let overrides = model_runtime(&snap, Class::Image, "overrides").unwrap();
    assert_eq!(overrides.image, "localhost/sdcpp:pinned");
    assert_eq!(
        overrides.extra_run_args,
        vec!["--device".to_string(), "amd.com/gpu=all".to_string()]
    );
    assert!(overrides.warm_start);
}

#[test]
fn unknown_model_id_resolves_to_none() {
    let snap = snapshot(vec![], vec![], vec![]);
    assert!(model_runtime(&snap, Class::Chat, "nope").is_none());
}

// ---------------------------------------------------------------------------
// render_spec bridge into WP1's argv renderer
// ---------------------------------------------------------------------------

#[test]
fn render_spec_for_a_chat_model_produces_the_expected_podman_argv() {
    let snap = snapshot(
        vec![LocalModel {
            params: LlamaParams {
                n_gpu_layers: Some(999),
                ..LlamaParams::default()
            },
            extra_run_args: Some(vec!["--device".into(), "nvidia.com/gpu=all".into()]),
            ..local(
                "gemma4-12b",
                Some("ghcr.io/ggml-org/llama.cpp:server-cuda"),
                None,
                false,
            )
        }],
        vec![],
        vec![],
    );
    let rt = model_runtime(&snap, Class::Chat, "gemma4-12b").unwrap();
    // `data_dir` is never read for a chat runtime (only audio's config-dir
    // renderer touches it) — a path that does not exist proves that.
    let spec = rt
        .render_spec(
            "lmgw",
            9101,
            "/srv/models/",
            Path::new("/nonexistent/data-dir"),
        )
        .expect("a chat runtime always carries llama args and never touches disk");
    assert_eq!(spec.health_path, "/health");
    assert_eq!(
        spec.container_name,
        container_name("lmgw", Class::Chat, "gemma4-12b")
    );

    let args = podman_run_argv(&spec);
    let expected: Vec<String> = [
        "run",
        "-d",
        "--replace",
        "--name",
        &spec.container_name,
        "--label",
        "lmgw.instance=lmgw",
        "--label",
        "lmgw.class=chat",
        "--label",
        "lmgw.model=gemma4-12b",
        "--label",
        "lmgw.engine=llama",
        "--device",
        "nvidia.com/gpu=all",
        "-p",
        "127.0.0.1:9101:8080",
        "-v",
        "/srv/models:/models:ro",
        "ghcr.io/ggml-org/llama.cpp:server-cuda",
        "-m",
        "/models/gemma4-12b.gguf",
        "--alias",
        "gemma4-12b",
        "--host",
        "0.0.0.0",
        "--port",
        "8080",
        "--n-gpu-layers",
        "999",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(args, expected);
}

// ---------------------------------------------------------------------------
// Ladder rungs (ladder design §4.1, §8 WP1)
// ---------------------------------------------------------------------------

fn preview_argv(rt: &lmgw_core::runtime::descriptor::ModelRuntime) -> Vec<String> {
    let spec = rt
        .preview_spec("lmgw", 9101, "/srv/models", Path::new("/nonexistent"))
        .expect("a chat runtime always carries llama args and never touches disk");
    podman_run_argv(&spec)
}

/// Rung 0 renders exactly as `model_runtime` always has — the done
/// criterion is byte-identical argv, not merely "the same fields".
#[test]
fn rung_zero_is_byte_identical_to_the_base() {
    let snap = snapshot(vec![local("m1", None, None, false)], vec![], vec![]);
    let base = model_runtime(&snap, Class::Chat, "m1").unwrap();
    let rung0 = model_runtime_at(&snap, Class::Chat, "m1", 0).unwrap();
    assert_eq!(preview_argv(&base), preview_argv(&rung0));
}

/// A higher rung overrides `-m` and `--ctx-size` and nothing else (design
/// §4.1): every other typed field (`n_gpu_layers` here) renders unchanged,
/// and a freeform `--ctx-size` sitting in `args` is deduped away by the
/// typed field exactly as it is on the base — `render_llama_args`'s `taken`
/// set claims `ctx-size` before freeform args are ever considered.
#[test]
fn a_higher_rung_overrides_only_gguf_and_ctx_size() {
    let mut m = local("laddered", None, None, false);
    m.params.ctx_size = Some(4096);
    m.params.n_gpu_layers = Some(99);
    m.args = vec!["--ctx-size".into(), "999".into()];
    m.ladder = vec![Rung {
        gguf_path: "top.gguf".into(),
        ctx_size: 131072,
    }];
    let snap = snapshot(vec![m], vec![], vec![]);

    let rung1 = model_runtime_at(&snap, Class::Chat, "laddered", 1).unwrap();
    let args = preview_argv(&rung1);

    let m_idx = args.iter().position(|a| a == "-m").unwrap();
    assert_eq!(args[m_idx + 1], "/models/top.gguf", "{args:?}");

    let ctx_positions: Vec<usize> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--ctx-size")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        ctx_positions.len(),
        1,
        "the freeform --ctx-size must be deduped away, not rendered twice: {args:?}"
    );
    assert_eq!(args[ctx_positions[0] + 1], "131072", "{args:?}");

    let ngl_idx = args.iter().position(|a| a == "--n-gpu-layers").unwrap();
    assert_eq!(
        args[ngl_idx + 1],
        "99",
        "unrelated fields stay the row's: {args:?}"
    );
}

/// `model_runtime_at` is defensive about a rung number a row no longer has
/// (e.g. a stale caller after the ladder shrank): it falls back to the base
/// rather than panicking.
#[test]
fn an_out_of_range_rung_falls_back_to_the_base() {
    let snap = snapshot(vec![local("m1", None, None, false)], vec![], vec![]);
    let base = model_runtime(&snap, Class::Chat, "m1").unwrap();
    let stale = model_runtime_at(&snap, Class::Chat, "m1", 7).unwrap();
    assert_eq!(preview_argv(&base), preview_argv(&stale));
}

// ---------------------------------------------------------------------------
// Audio config rendering (§3.6, "Audio config")
// ---------------------------------------------------------------------------

/// "Single-model" rendering is not a second implementation: it is the
/// existing multi-model renderer called with a one-element slice, so the
/// entry it produces for a model is byte-identical to what that model's
/// entry looks like inside a real multi-model list, siblings and all.
#[test]
fn single_model_config_matches_the_multi_model_renderers_entry_for_the_same_model() {
    let settings = AudioSettings {
        backend: "cuda".into(),
        device: 0,
        threads: 4,
        lazy_load: false,
        ..AudioSettings::default()
    };
    let m = audio("qwen3-tts", None, false);

    let single = audio_cfg::render_single_model_config(&settings, &m);
    let multi_alone = render_server_config(&settings, std::slice::from_ref(&m));
    assert_eq!(
        single, multi_alone,
        "single-model rendering must be exactly the multi-model call with a one-element slice"
    );

    // And the parsed entry survives unchanged when real siblings sit next to
    // it in a genuine multi-model list.
    let sibling = audio("other-model", None, false);
    let multi_with_sibling = render_server_config(&settings, &[m.clone(), sibling]);
    let multi_v: Value = serde_json::from_str(&multi_with_sibling).unwrap();
    let single_v: Value = serde_json::from_str(&single).unwrap();
    assert_eq!(multi_v["models"][0], single_v["models"][0]);
}

#[test]
fn config_dir_follows_the_data_dir_audiocpp_slug_hash_scheme() {
    let data_dir = Path::new("/home/u/.local/share/lmgw");
    let dir = audio_cfg::config_dir(data_dir, "Qwen3-TTS/12Hz");
    assert_eq!(
        dir,
        data_dir.join("audiocpp").join(format!(
            "{}-{}",
            slug("Qwen3-TTS/12Hz"),
            hash6("Qwen3-TTS/12Hz")
        ))
    );
    assert!(
        dir.to_string_lossy()
            .starts_with("/home/u/.local/share/lmgw/audiocpp/qwen3-tts-12hz-"),
        "{dir:?}"
    );
}

/// The config dir carries the same `hash6` the container name does, and for
/// the same reason (§3.3): `slug` is lossy, so two model ids that slug alike
/// would otherwise mount **one** directory into two containers and overwrite
/// each other's `server.json` at every start — with adoption (§3.4) comparing
/// the mounted config, that also means each start evicting the other's
/// container on the next boot.
#[test]
fn two_models_that_slug_alike_get_different_config_dirs() {
    let data_dir = Path::new("/home/u/.local/share/lmgw");
    for (a, b) in [("org/Voice.v2", "org-voice-v2"), ("a/b", "a-b")] {
        assert_eq!(
            slug(a),
            slug(b),
            "the pair has to collide under slug for this to test anything"
        );
        assert_ne!(
            audio_cfg::config_dir(data_dir, a),
            audio_cfg::config_dir(data_dir, b),
            "config dirs collided for {a:?} vs {b:?}"
        );
    }
}

#[test]
fn write_config_creates_the_file_atomically_and_it_parses() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("audiocpp").join("qwen3-tts");
    let json = render_server_config(
        &AudioSettings::default(),
        &[audio("qwen3-tts", None, false)],
    );

    let returned = audio_cfg::write_config(&dir, &json).unwrap();
    assert_eq!(returned, dir, "the dir comes back, not the file path");

    let on_disk = std::fs::read_to_string(dir.join("server.json")).unwrap();
    assert_eq!(on_disk, json);
    let parsed: Value = serde_json::from_str(&on_disk).unwrap();
    assert_eq!(parsed["models"][0]["id"], "qwen3-tts");
    // No leftover tmp file after a successful write.
    assert!(!dir.join(".server.json.tmp").exists());
}

/// End to end through `ModelRuntime::render_spec`: an audio model now
/// produces a complete podman argv (deliverable of §3.6/WP3.5) — same
/// run/labels/port/`/models` mount frame as a llama model, `/config` mounted
/// at the per-model config dir `render_spec` just wrote, and the fixed
/// audio.cpp container command.
#[test]
fn render_spec_for_an_audio_model_writes_its_config_and_produces_the_expected_podman_argv() {
    let m = audio("qwen3-tts", None, false);
    let snap = snapshot(vec![], vec![], vec![m.clone()]);
    let rt = model_runtime(&snap, Class::Audio, "qwen3-tts").unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let spec = rt
        .render_spec("lmgw", 9201, "/srv/audio-models/", tmp.path())
        .expect("an audio runtime always carries the model row + class settings");

    assert_eq!(
        spec.container_name,
        container_name("lmgw", Class::Audio, "qwen3-tts")
    );
    assert_eq!(spec.health_path, "/v1/models");
    let expected_dir = audio_cfg::config_dir(tmp.path(), "qwen3-tts");
    assert_eq!(spec.config_mount, Some(expected_dir.clone()));

    // The config was actually written, and it is the same JSON the
    // multi-model renderer would have produced for this one model.
    let on_disk = std::fs::read_to_string(expected_dir.join("server.json")).unwrap();
    let expected_json = audio_cfg::render_single_model_config(&snap.settings.audio, &m);
    assert_eq!(on_disk, expected_json);

    let args = podman_run_argv(&spec);
    let expected: Vec<String> = [
        "run".to_string(),
        "-d".into(),
        "--replace".into(),
        "--name".into(),
        spec.container_name.clone(),
        "--label".into(),
        "lmgw.instance=lmgw".into(),
        "--label".into(),
        "lmgw.class=audio".into(),
        "--label".into(),
        "lmgw.model=qwen3-tts".into(),
        "--label".into(),
        "lmgw.engine=audio".into(),
        "--audio-class-flag".into(),
        "-p".into(),
        "127.0.0.1:9201:8080".into(),
        "-v".into(),
        "/srv/audio-models:/models:ro".into(),
        "-v".into(),
        format!("{}:/config:ro", expected_dir.display()),
        "class-audio-image".into(),
        "server".into(),
        "--config".into(),
        "/config/server.json".into(),
    ]
    .to_vec();
    assert_eq!(args, expected);
}

// ---------------------------------------------------------------------------
// The image class's render_spec (image-generation design §3)
// ---------------------------------------------------------------------------

/// The fourth class end to end: `--init`, `--entrypoint /sd-server`, the class
/// image/run args it inherited, the capabilities health path — and the two
/// directories created under the models dir on the way, because sd-server's
/// capabilities route throws without them (§12.2).
#[test]
fn render_spec_for_an_image_model_creates_its_dirs_and_produces_the_expected_podman_argv() {
    let models_dir = tempfile::tempdir().unwrap();
    let dir = models_dir.path().display().to_string();
    let mut snap =
        snapshot_with_images(vec![], vec![], vec![], vec![image("z-image", None, false)]);
    snap.settings.image.models_dir = dir.clone();

    let rt = model_runtime(&snap, Class::Image, "z-image").unwrap();
    // `data_dir` is unused for this class — sd-server has no config file — so
    // a path that does not exist proves it is never touched.
    let spec = rt
        .render_spec("lmgw", 9301, &dir, Path::new("/nonexistent/lmgw-data-dir"))
        .expect("the only fallible part is creating the two directories");

    assert_eq!(
        spec.container_name,
        container_name("lmgw", Class::Image, "z-image")
    );
    assert_eq!(spec.health_path, "/sdcpp/v1/capabilities");
    assert_eq!(spec.config_mount, None, "sd-server mounts no config dir");
    assert!(spec.init);
    assert_eq!(spec.entrypoint.as_deref(), Some("/sd-server"));
    assert!(models_dir.path().join("loras").is_dir());
    assert!(models_dir.path().join("upscalers").is_dir());

    let args = podman_run_argv(&spec);
    let expected: Vec<String> = [
        "run".to_string(),
        "-d".into(),
        "--replace".into(),
        "--init".into(),
        "--name".into(),
        spec.container_name.clone(),
        "--label".into(),
        "lmgw.instance=lmgw".into(),
        "--label".into(),
        "lmgw.class=image".into(),
        "--label".into(),
        "lmgw.model=z-image".into(),
        "--label".into(),
        "lmgw.engine=sdcpp".into(),
        "--image-class-flag".into(),
        "-p".into(),
        "127.0.0.1:9301:8080".into(),
        "-v".into(),
        format!("{dir}:/models:ro"),
        "--entrypoint".into(),
        "/sd-server".into(),
        "class-image-image".into(),
        "--listen-ip".into(),
        "0.0.0.0".into(),
        "--listen-port".into(),
        "8080".into(),
        "--eager-load".into(),
        "--lora-model-dir".into(),
        "/models/loras".into(),
        "--hires-upscalers-dir".into(),
        "/models/upscalers".into(),
        "--diffusion-model".into(),
        "/models/leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf".into(),
        "--vae".into(),
        "/models/Comfy-Org/z_image_turbo/ae.safetensors".into(),
        "--cfg-scale".into(),
        "1.0".into(),
        "--diffusion-fa".into(),
    ]
    .to_vec();
    assert_eq!(args, expected);
}

/// The other three classes must keep their podman line exactly as it was: no
/// `--init`, no `--entrypoint`.
#[test]
fn only_the_image_class_asks_for_init_and_an_entrypoint() {
    let snap = snapshot(
        vec![local("c1", None, None, false)],
        vec![aux("a1", None, None, false)],
        vec![audio("d1", None, false)],
    );
    let tmp = tempfile::tempdir().unwrap();
    for (class, id) in [
        (Class::Chat, "c1"),
        (Class::Aux, "a1"),
        (Class::Audio, "d1"),
    ] {
        let rt = model_runtime(&snap, class, id).unwrap();
        let spec = rt
            .render_spec("lmgw", 9000, "/srv/models", tmp.path())
            .unwrap();
        assert!(!spec.init, "{class} asked for --init");
        assert_eq!(spec.entrypoint, None, "{class} asked for an --entrypoint");
        let argv = podman_run_argv(&spec);
        assert!(!argv.iter().any(|a| a == "--init"), "{class}: {argv:?}");
        assert!(
            !argv.iter().any(|a| a == "--entrypoint"),
            "{class}: {argv:?}"
        );
    }
}

/// A row that repoints `lora_model_dir` gets *that* directory created, not
/// the default one — the flag always points somewhere that exists.
#[test]
fn a_repointed_lora_dir_is_the_one_that_gets_created() {
    let models_dir = tempfile::tempdir().unwrap();
    let dir = models_dir.path().display().to_string();
    let mut row = image("z-image", None, false);
    row.files
        .insert("lora_model_dir".into(), "shared/loras".into());
    let mut snap = snapshot_with_images(vec![], vec![], vec![], vec![row]);
    snap.settings.image.models_dir = dir.clone();

    let rt = model_runtime(&snap, Class::Image, "z-image").unwrap();
    let spec = rt
        .render_spec("lmgw", 9301, &dir, Path::new("/nonexistent"))
        .unwrap();
    assert!(models_dir.path().join("shared/loras").is_dir());
    assert!(!models_dir.path().join("loras").exists());
    let argv = podman_run_argv(&spec);
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--lora-model-dir" && w[1] == "/models/shared/loras"),
        "{argv:?}"
    );
}

/// A **read** verb renders the same argv without touching the filesystem.
///
/// `render_spec` creates the two directories sd-server's capabilities route
/// throws without, which a start must do and a preview must not: on a
/// read-only models dir the preview would have answered "could not render the
/// command line: Read-only file system" to a question about flags.
#[test]
fn a_preview_renders_the_command_line_without_creating_anything() {
    let models_dir = tempfile::tempdir().unwrap();
    let dir = models_dir.path().display().to_string();
    let mut snap =
        snapshot_with_images(vec![], vec![], vec![], vec![image("z-image", None, false)]);
    snap.settings.image.models_dir = dir.clone();
    let rt = model_runtime(&snap, Class::Image, "z-image").unwrap();

    // Read-only, so `create_dir_all` would fail rather than quietly succeed.
    let mut perms = std::fs::metadata(models_dir.path()).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(models_dir.path(), perms).unwrap();

    let spec = rt
        .preview_spec("lmgw", 0, &dir, Path::new("/nonexistent"))
        .expect("a preview never depends on the filesystem");
    let argv = podman_run_argv(&spec);
    assert!(argv.iter().any(|a| a == "--eager-load"), "{argv:?}");
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--lora-model-dir" && w[1] == "/models/loras"),
        "{argv:?}"
    );
    assert!(!models_dir.path().join("loras").exists());
    assert!(!models_dir.path().join("upscalers").exists());

    // The start path still creates them, where a failure belongs.
    let mut perms = std::fs::metadata(models_dir.path()).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(models_dir.path(), perms).unwrap();
    rt.render_spec("lmgw", 0, &dir, Path::new("/nonexistent"))
        .unwrap();
    assert!(models_dir.path().join("loras").is_dir());
}

/// The fourth synthetic upstream (§3): id −4, name `sdcpp`, never persisted,
/// and `vram::classify` reads the class back off that id — which is what
/// makes an image route recognisable as local without trusting a name an
/// owner could reuse.
#[test]
fn the_image_class_routes_through_the_synthetic_upstream_minus_four() {
    let mut snap =
        snapshot_with_images(vec![], vec![], vec![], vec![image("z-image", None, false)]);
    snap.settings.image.public_prefix = "image".into();

    let up = snap.image_upstream();
    assert_eq!(up.id, lmgw_core::config::IMAGE_UPSTREAM_ID);
    assert_eq!(up.id, -4);
    assert_eq!(up.name, "sdcpp");
    assert_eq!(up.kind.as_str(), "sd_cpp");
    // The class's own ceiling, not the constant all four used to share.
    assert_eq!(
        up.timeout_ms, 1_800_000,
        "a high-step render at a large size is minutes on one connection"
    );
    snap.settings.image.request_timeout_seconds = 42;
    assert_eq!(
        snap.image_upstream().timeout_ms,
        42_000,
        "reads the setting"
    );
    snap.settings.image.request_timeout_seconds = 0;
    assert_eq!(
        snap.image_upstream().request_timeout(),
        None,
        "0 is the maximum possible, not a zero-length deadline"
    );
    snap.settings.image.request_timeout_seconds = 1800;
    assert!(!snap.upstreams.values().any(|u| u.id == -4), "never stored");
    assert!(snap.is_local_upstream(-4));

    // `image/<id>` resolves onto it, and only for an enabled row.
    let route = snap
        .resolve("image/z-image")
        .expect("the class prefix resolves");
    assert_eq!(route.upstream.id, -4);
    assert_eq!(route.upstream_model, "z-image");
    let target = lmgw_core::vram::classify(&route).expect("a local target");
    assert_eq!(target.class, Class::Image);
    assert_eq!(target.model_id, "z-image");

    assert_eq!(snap.image_public_name("z-image"), "image/z-image");
    snap.image_models[0].enabled = false;
    assert!(snap.resolve("image/z-image").is_err());
}

/// Hold fallback: the image class behaves like aux and audio — it never
/// inherits the global chat alias (a chat model cannot draw a picture), and
/// only an explicit `alias` mode gives a row one.
#[test]
fn an_image_row_never_inherits_the_global_hold_fallback() {
    use lmgw_core::config::HoldFallbackMode;

    let mut snap =
        snapshot_with_images(vec![], vec![], vec![], vec![image("z-image", None, false)]);
    snap.settings.hold.fallback_alias = Some("cloud-chat".into());
    assert_eq!(snap.hold_fallback_for(Class::Image, "z-image"), None);

    snap.image_models[0].hold_fallback_mode = HoldFallbackMode::Alias;
    snap.image_models[0].hold_fallback = Some("openai/gpt-image-1".into());
    assert_eq!(
        snap.hold_fallback_for(Class::Image, "z-image"),
        Some("openai/gpt-image-1".to_string())
    );

    snap.image_models[0].hold_fallback_mode = HoldFallbackMode::None;
    assert_eq!(snap.hold_fallback_for(Class::Image, "z-image"), None);
    // A row this snapshot does not know refuses rather than inherits.
    assert_eq!(snap.hold_fallback_for(Class::Image, "gone"), None);
}
