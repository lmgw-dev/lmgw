//! Per-model container argv rendering + naming (design §3.3, §3.6).
//!
//! Mirrors what `tests/router_preset.rs` covers for the INI renderer, minus
//! everything that renderer does not have to do (env-var-style bare args,
//! newline injection into an INI file — argv tokens are `execve` arguments,
//! not text re-parsed by a loader).

use std::path::PathBuf;

use lmgw_core::config::{AuxKind, LlamaParams};
use lmgw_core::runtime::argv::{
    podman_run_argv, render_engine_args, render_image_args, render_llama_args, EngineArgs,
    ImageArgs, LlamaArgs, RenderSpec,
};
use lmgw_core::runtime::{container_name, hash6, slug, Class};
use lmgw_core::sdcpp_caps::SdcppCaps;
use serde_json::{json, Value};

fn chat_spec(model_id: &str, gguf: &str, params: LlamaParams, args: &[&str]) -> RenderSpec {
    RenderSpec {
        class: Class::Chat,
        model_id: model_id.into(),
        image: "ghcr.io/ggml-org/llama.cpp:server-cuda".into(),
        container_name: "lmgw-chat-x-abcdef".into(),
        container_prefix: "lmgw".into(),
        host_port: 9001,
        models_dir: "/srv/models".into(),
        config_mount: None,
        extra_run_args: vec![],
        init: false,
        entrypoint: None,
        engine: EngineArgs::Llama(LlamaArgs::Chat {
            gguf_path: gguf.into(),
            params,
            args: args.iter().map(|s| s.to_string()).collect(),
        }),
        health_path: "/health",
    }
}

fn aux_spec(
    model_id: &str,
    gguf: &str,
    kind: AuxKind,
    pooling: Option<&str>,
    ctx_size: Option<i64>,
    args: &[&str],
) -> RenderSpec {
    RenderSpec {
        class: Class::Aux,
        model_id: model_id.into(),
        image: "ghcr.io/ggml-org/llama.cpp:server-cuda".into(),
        container_name: "lmgw-aux-x-abcdef".into(),
        container_prefix: "lmgw".into(),
        host_port: 9002,
        models_dir: "/srv/aux-models".into(),
        config_mount: None,
        extra_run_args: vec![],
        init: false,
        entrypoint: None,
        engine: EngineArgs::Llama(LlamaArgs::Aux {
            gguf_path: gguf.into(),
            kind,
            pooling: pooling.map(String::from),
            ctx_size,
            args: args.iter().map(|s| s.to_string()).collect(),
        }),
        health_path: "/health",
    }
}

/// A pair of adjacent tokens appears somewhere in the output.
fn has_pair(args: &[String], a: &str, b: &str) -> bool {
    args.windows(2).any(|w| w[0] == a && w[1] == b)
}

// ---------------------------------------------------------------------------
// Always-emitted -m / --alias / --host / --port
// ---------------------------------------------------------------------------

#[test]
fn always_emits_model_alias_and_host_port() {
    let spec = chat_spec(
        "my-model",
        "weights/model.gguf",
        LlamaParams::default(),
        &[],
    );
    let args = render_llama_args(&spec);
    assert!(
        has_pair(&args, "-m", "/models/weights/model.gguf"),
        "{args:?}"
    );
    assert!(has_pair(&args, "--alias", "my-model"), "{args:?}");
    assert!(has_pair(&args, "--host", "0.0.0.0"), "{args:?}");
    assert!(has_pair(&args, "--port", "8080"), "{args:?}");
}

/// Direct mode names the model after the request, not the router — so
/// `--alias` must reflect `model_id` even when it differs wildly from the
/// GGUF filename, and a freeform `-m`/`--alias`/`--host`/`--port` must not be
/// able to override any of the four.
#[test]
fn always_emitted_keys_cannot_be_overridden_by_freeform_args() {
    let spec = chat_spec(
        "my-model",
        "weights/model.gguf",
        LlamaParams::default(),
        &[
            "-m",
            "/models/other.gguf",
            "--alias",
            "someone-else",
            "--host",
            "127.0.0.1",
            "--port",
            "9999",
        ],
    );
    let args = render_llama_args(&spec);
    assert!(
        has_pair(&args, "-m", "/models/weights/model.gguf"),
        "{args:?}"
    );
    assert!(has_pair(&args, "--alias", "my-model"), "{args:?}");
    assert!(has_pair(&args, "--host", "0.0.0.0"), "{args:?}");
    assert!(has_pair(&args, "--port", "8080"), "{args:?}");
    assert!(!args.contains(&"other.gguf".to_string()), "{args:?}");
    assert!(!args.contains(&"someone-else".to_string()), "{args:?}");
    assert!(!args.contains(&"127.0.0.1".to_string()), "{args:?}");
    assert!(!args.contains(&"9999".to_string()), "{args:?}");
}

// ---------------------------------------------------------------------------
// switch vs value vs no-twin — one per flag class
// ---------------------------------------------------------------------------

#[test]
fn boolean_flag_renders_as_a_bare_switch() {
    let params = LlamaParams {
        jinja: true,
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec("m", "m.gguf", params, &[]));
    assert!(args.contains(&"--jinja".to_string()), "{args:?}");
    // Never `--jinja true`.
    assert!(!has_pair(&args, "--jinja", "true"), "{args:?}");
}

#[test]
fn value_taking_option_keeps_its_value() {
    let params = LlamaParams {
        flash_attn: Some("on".into()),
        ctx_size: Some(32768),
        n_predict: Some(4096),
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec("m", "m.gguf", params, &[]));
    assert!(has_pair(&args, "--flash-attn", "on"), "{args:?}");
    assert!(has_pair(&args, "--ctx-size", "32768"), "{args:?}");
    assert!(has_pair(&args, "--n-predict", "4096"), "{args:?}");
}

/// `reasoning_preserve` is the one default-on tri-state: `Some(false)` must
/// render the `--no-*` twin as a bare switch, not `--reasoning-preserve
/// false`, and the positive spelling must not also appear.
#[test]
fn false_on_default_on_option_renders_the_no_twin() {
    let params = LlamaParams {
        reasoning_preserve: Some(false),
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec("m", "m.gguf", params, &[]));
    assert!(
        args.contains(&"--no-reasoning-preserve".to_string()),
        "{args:?}"
    );
    assert!(
        !args.contains(&"--reasoning-preserve".to_string()),
        "{args:?}"
    );

    let params = LlamaParams {
        reasoning_preserve: Some(true),
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec("m", "m.gguf", params, &[]));
    assert!(
        args.contains(&"--reasoning-preserve".to_string()),
        "{args:?}"
    );
    assert!(
        !args.contains(&"--no-reasoning-preserve".to_string()),
        "{args:?}"
    );

    // Unset stays silent — the template's own default applies.
    let args = render_llama_args(&chat_spec("m", "m.gguf", LlamaParams::default(), &[]));
    assert!(
        !args.iter().any(|a| a.contains("reasoning-preserve")),
        "{args:?}"
    );
}

/// `kv_unified` is a tri-state exactly like `reasoning_preserve` above, but
/// with no "default-on" twist: `Some(true)`/`Some(false)` render their own
/// bare switch, and `None` stays silent (unified-KV design §3.1).
/// `kv_unified_per_slot` is an ordinary value flag, independent of the
/// tri-state.
#[test]
fn kv_unified_renders_the_tri_state_and_the_per_slot_cap() {
    let params = LlamaParams {
        kv_unified: Some(true),
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec("m", "m.gguf", params, &[]));
    assert!(args.contains(&"--kv-unified".to_string()), "{args:?}");
    assert!(!args.contains(&"--no-kv-unified".to_string()), "{args:?}");

    let params = LlamaParams {
        kv_unified: Some(false),
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec("m", "m.gguf", params, &[]));
    assert!(args.contains(&"--no-kv-unified".to_string()), "{args:?}");
    assert!(!args.contains(&"--kv-unified".to_string()), "{args:?}");

    // Unset stays silent — llama-server's own default applies.
    let args = render_llama_args(&chat_spec("m", "m.gguf", LlamaParams::default(), &[]));
    assert!(!args.iter().any(|a| a.contains("kv-unified")), "{args:?}");

    let params = LlamaParams {
        kv_unified_per_slot: Some(8192),
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec("m", "m.gguf", params, &[]));
    assert!(has_pair(&args, "--kv-unified-per-slot", "8192"), "{args:?}");
}

// ---------------------------------------------------------------------------
// mmproj / draft path rewrite
// ---------------------------------------------------------------------------

#[test]
fn mmproj_and_draft_paths_rewrite_onto_the_models_mount() {
    let params = LlamaParams {
        mmproj_path: Some("proj/mmproj.gguf".into()),
        draft_gguf_path: Some("/MTP/gemma-4-31B-it-Q8_0-MTP.gguf".into()),
        spec_type: Some("draft-mtp".into()),
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec("m", "gemma.gguf", params, &[]));
    assert!(
        has_pair(&args, "--mmproj", "/models/proj/mmproj.gguf"),
        "{args:?}"
    );
    assert!(
        has_pair(
            &args,
            "--model-draft",
            "/models/MTP/gemma-4-31B-it-Q8_0-MTP.gguf"
        ),
        "{args:?}"
    );
}

#[test]
fn mmproj_and_no_mmproj_are_mutually_exclusive() {
    let params = LlamaParams {
        no_mmproj: true,
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec(
        "m",
        "m.gguf",
        params,
        &["--mmproj=/models/p.gguf"],
    ));
    assert!(args.contains(&"--no-mmproj".to_string()), "{args:?}");
    assert!(
        !args
            .iter()
            .any(|a| a.contains("mmproj.gguf") || a == "p.gguf"),
        "{args:?}"
    );

    let params = LlamaParams {
        mmproj_path: Some("p.gguf".into()),
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec("m", "m.gguf", params, &["--no-mmproj"]));
    assert!(has_pair(&args, "--mmproj", "/models/p.gguf"), "{args:?}");
    assert!(!args.contains(&"--no-mmproj".to_string()), "{args:?}");
}

// ---------------------------------------------------------------------------
// Dedup against freeform args
// ---------------------------------------------------------------------------

/// `-ngl` is the short alias of `--n-gpu-layers`; a stale copy in the
/// freeform args must lose to the structured field silently, not render
/// alongside it.
#[test]
fn short_alias_collision_loses_to_the_structured_field() {
    let params = LlamaParams {
        n_gpu_layers: Some(999),
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec("m", "m.gguf", params, &["-ngl", "40"]));
    assert!(has_pair(&args, "--n-gpu-layers", "999"), "{args:?}");
    assert!(!args.iter().any(|a| a == "-ngl"), "{args:?}");
    assert!(!args.contains(&"40".to_string()), "{args:?}");
}

/// `-n` and `--predict` are both llama-server aliases of `--n-predict`; a
/// stale copy of either in the freeform args must lose to the structured
/// field silently, not render alongside it.
#[test]
fn n_predict_short_and_long_aliases_lose_to_the_structured_field() {
    let params = LlamaParams {
        n_predict: Some(4096),
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec("m", "m.gguf", params.clone(), &["-n", "100"]));
    assert!(has_pair(&args, "--n-predict", "4096"), "{args:?}");
    assert!(!args.iter().any(|a| a == "-n"), "{args:?}");
    assert!(!args.contains(&"100".to_string()), "{args:?}");

    let args = render_llama_args(&chat_spec("m", "m.gguf", params, &["--predict", "100"]));
    assert!(has_pair(&args, "--n-predict", "4096"), "{args:?}");
    assert!(!args.iter().any(|a| a == "--predict"), "{args:?}");
    assert!(!args.contains(&"100".to_string()), "{args:?}");
}

/// `--n-gpu-layers=40` (equals form) must be recognized as the same key too.
#[test]
fn equals_form_collision_also_loses() {
    let params = LlamaParams {
        n_gpu_layers: Some(999),
        ..LlamaParams::default()
    };
    let args = render_llama_args(&chat_spec("m", "m.gguf", params, &["--n-gpu-layers=40"]));
    assert!(has_pair(&args, "--n-gpu-layers", "999"), "{args:?}");
    assert!(!args.iter().any(|a| a.contains('=')), "{args:?}");
}

/// A freeform arg whose key is *not* claimed by any structured field must
/// still pass through untouched — the dedup guard only removes genuine
/// collisions.
#[test]
fn non_colliding_freeform_args_pass_through() {
    let args = render_llama_args(&chat_spec(
        "m",
        "m.gguf",
        LlamaParams::default(),
        &["--top-k", "5", "--some-future-flag"],
    ));
    assert!(has_pair(&args, "--top-k", "5"), "{args:?}");
    assert!(args.contains(&"--some-future-flag".to_string()), "{args:?}");
}

/// `--rerank` is llama-server's short spelling of `--reranking`; a stale copy
/// must not duplicate the flag the aux kind already rendered.
#[test]
fn rerank_short_alias_cannot_duplicate_the_reranking_flag() {
    let args = render_llama_args(&aux_spec(
        "reranker",
        "reranker.gguf",
        AuxKind::Rerank,
        None,
        None,
        &["--rerank"],
    ));
    let reranking_count = args.iter().filter(|a| a.as_str() == "--reranking").count();
    assert_eq!(reranking_count, 1, "{args:?}");
    assert!(!args.contains(&"--rerank".to_string()), "{args:?}");
}

// ---------------------------------------------------------------------------
// Embed vs rerank aux flags
// ---------------------------------------------------------------------------

#[test]
fn embed_renders_embeddings_and_optional_pooling() {
    let args = render_llama_args(&aux_spec(
        "bge-m3",
        "bge-m3.gguf",
        AuxKind::Embed,
        Some("mean"),
        Some(8192),
        &[],
    ));
    assert!(args.contains(&"--embeddings".to_string()), "{args:?}");
    assert!(!args.contains(&"--reranking".to_string()), "{args:?}");
    assert!(has_pair(&args, "--pooling", "mean"), "{args:?}");
    assert!(has_pair(&args, "--ctx-size", "8192"), "{args:?}");
}

/// Even if a rerank row somehow carries a `pooling` value (nothing in the
/// type system forbids it), the renderer must never emit `--pooling` for a
/// reranker — the llama-server quirk where a second pooling source breaks
/// reranking outright.
#[test]
fn rerank_never_emits_pooling_even_when_set() {
    let args = render_llama_args(&aux_spec(
        "reranker",
        "reranker.gguf",
        AuxKind::Rerank,
        Some("rank"),
        None,
        &[],
    ));
    assert!(args.contains(&"--reranking".to_string()), "{args:?}");
    assert!(
        !args.iter().any(|a| a == "--pooling" || a == "rank"),
        "{args:?}"
    );

    // A freeform `--pooling` cannot sneak it back in either.
    let args = render_llama_args(&aux_spec(
        "reranker",
        "reranker.gguf",
        AuxKind::Rerank,
        None,
        None,
        &["--pooling", "rank"],
    ));
    assert!(
        !args.iter().any(|a| a == "--pooling" || a == "rank"),
        "{args:?}"
    );
}

// ---------------------------------------------------------------------------
// Full podman argv golden test
// ---------------------------------------------------------------------------

#[test]
fn podman_run_argv_matches_the_expected_shape() {
    let mut spec = chat_spec(
        "gemma4-12b",
        "gemma-4-12B-it-qat-UD-Q4_K_XL.gguf",
        LlamaParams {
            n_gpu_layers: Some(999),
            ..LlamaParams::default()
        },
        &[],
    );
    spec.container_name = "lmgw-chat-gemma4-12b-a1b2c3".into();
    spec.container_prefix = "lmgw".into();
    spec.host_port = 9101;
    spec.models_dir = "/srv/models/".into();
    spec.config_mount = Some(PathBuf::from("/home/u/.local/share/lmgw/audiocpp/gemma"));
    spec.extra_run_args = vec![
        "--device".into(),
        "nvidia.com/gpu=all".into(),
        "--security-opt".into(),
        "label=disable".into(),
    ];

    let args = podman_run_argv(&spec);
    let expected: Vec<String> = [
        "run",
        "-d",
        "--replace",
        "--name",
        "lmgw-chat-gemma4-12b-a1b2c3",
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
        "--security-opt",
        "label=disable",
        "-p",
        "127.0.0.1:9101:8080",
        "-v",
        "/srv/models:/models:ro",
        "-v",
        "/home/u/.local/share/lmgw/audiocpp/gemma:/config:ro",
        "ghcr.io/ggml-org/llama.cpp:server-cuda",
        "-m",
        "/models/gemma-4-12B-it-qat-UD-Q4_K_XL.gguf",
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

#[test]
fn podman_run_argv_omits_config_mount_when_none() {
    let spec = chat_spec("m", "m.gguf", LlamaParams::default(), &[]);
    let args = podman_run_argv(&spec);
    assert!(!args.iter().any(|a| a.contains(":/config:ro")), "{args:?}");
}

// ---------------------------------------------------------------------------
// Audio engine argv (§3.6)
// ---------------------------------------------------------------------------

/// The full podman argv for an audio model's container: same run/labels/port/
/// `/models` mount frame as a llama model, `/config` mounted at the per-model
/// config dir, and a container command that is fixed regardless of which
/// model it is — audio.cpp reads the mounted `server.json` for that (mirrors
/// `audio::podman_run_args`'s tail exactly: `server --config
/// /config/server.json`).
#[test]
fn podman_run_argv_for_an_audio_model_matches_the_expected_shape() {
    let spec = RenderSpec {
        class: Class::Audio,
        model_id: "qwen3-tts".into(),
        image: "ghcr.io/0xshug0/audio.cpp:full-cuda12".into(),
        container_name: "lmgw-audio-qwen3-tts-a1b2c3".into(),
        container_prefix: "lmgw".into(),
        host_port: 9201,
        models_dir: "/srv/audio-models/".into(),
        config_mount: Some("/home/u/.local/share/lmgw/audiocpp/qwen3-tts".into()),
        extra_run_args: vec!["--device".into(), "nvidia.com/gpu=all".into()],
        init: false,
        entrypoint: None,
        engine: EngineArgs::Audio,
        health_path: "/v1/models",
    };

    let args = podman_run_argv(&spec);
    let expected: Vec<String> = [
        "run",
        "-d",
        "--replace",
        "--name",
        "lmgw-audio-qwen3-tts-a1b2c3",
        "--label",
        "lmgw.instance=lmgw",
        "--label",
        "lmgw.class=audio",
        "--label",
        "lmgw.model=qwen3-tts",
        "--label",
        "lmgw.engine=audio",
        "--device",
        "nvidia.com/gpu=all",
        "-p",
        "127.0.0.1:9201:8080",
        "-v",
        "/srv/audio-models:/models:ro",
        "-v",
        "/home/u/.local/share/lmgw/audiocpp/qwen3-tts:/config:ro",
        "ghcr.io/0xshug0/audio.cpp:full-cuda12",
        "server",
        "--config",
        "/config/server.json",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(args, expected);
}

/// `render_llama_args` is llama-specific; called on an audio spec it must
/// come back empty rather than panic — [`render_engine_args`]'s dispatch (not
/// this function) is what a caller should reach for when the engine is
/// unknown, but this is the failure mode if something calls the wrong one.
#[test]
fn render_llama_args_is_empty_for_an_audio_spec() {
    let spec = RenderSpec {
        class: Class::Audio,
        model_id: "voice1".into(),
        image: "img".into(),
        container_name: "c".into(),
        container_prefix: "lmgw".into(),
        host_port: 1,
        models_dir: "/m".into(),
        config_mount: None,
        extra_run_args: vec![],
        init: false,
        entrypoint: None,
        engine: EngineArgs::Audio,
        health_path: "/v1/models",
    };
    assert!(render_llama_args(&spec).is_empty());
}

// ---------------------------------------------------------------------------
// Naming: slug edge cases + injectivity
// ---------------------------------------------------------------------------

#[test]
fn slug_edge_cases() {
    assert_eq!(slug("Qwen3.6-35B-A3B"), "qwen3-6-35b-a3b");
    assert_eq!(slug("org/Model-Name"), "org-model-name");
    assert_eq!(slug("UPPER_CASE"), "upper-case");
    assert_eq!(slug("日本語モデル"), "");

    let long = format!("{}-tail", "x".repeat(60));
    let slugged = slug(&long);
    assert!(slugged.len() <= 40, "{slugged:?}");
    assert!(!slugged.ends_with('-'), "{slugged:?}");
    assert!(!slugged.starts_with('-'), "{slugged:?}");
}

#[test]
fn hash6_is_six_hex_chars_and_stable() {
    let h = hash6("some-model-id");
    assert_eq!(h.len(), 6);
    assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(h, hash6("some-model-id"));
    assert_ne!(h, hash6("some-other-model-id"));
}

#[test]
fn container_name_is_injective_over_adversarial_pairs() {
    let pairs = [
        ("a/b", "a-b"),
        ("Model.Name", "model-name"),
        ("qwen3.6-35b", "qwen3-6-35b"),
        ("日本語", "!!!"),
        ("foo--bar", "foo-bar"),
        ("UPPER", "upper"),
    ];
    for (a, b) in pairs {
        assert_ne!(
            container_name("lmgw", Class::Chat, a),
            container_name("lmgw", Class::Chat, b),
            "collision for {a:?} vs {b:?}"
        );
    }
}

#[test]
fn container_name_differs_across_classes_for_the_same_model_id() {
    let names: Vec<String> = [Class::Chat, Class::Aux, Class::Audio]
        .iter()
        .map(|c| container_name("lmgw", *c, "shared-model-id"))
        .collect();
    assert_ne!(names[0], names[1]);
    assert_ne!(names[0], names[2]);
    assert_ne!(names[1], names[2]);
}

// ---------------------------------------------------------------------------
// Image engine argv (image-generation design §3)
// ---------------------------------------------------------------------------

/// A spec for one sd-server pipeline, spelled against the vocabulary lmgw
/// ships with (the committed `sd-server --help` of `c678dfe`) — the same
/// vocabulary a start falls back to when the image cannot be probed.
fn image_spec(files: Value, args: Value) -> RenderSpec {
    RenderSpec {
        class: Class::Image,
        model_id: "z-image-turbo".into(),
        image: "ghcr.io/leejet/stable-diffusion.cpp:master-cuda".into(),
        container_name: "lmgw-image-z-image-turbo-a1b2c3".into(),
        container_prefix: "lmgw".into(),
        host_port: 9301,
        models_dir: "/srv/image-models/".into(),
        config_mount: None,
        extra_run_args: vec!["--device".into(), "nvidia.com/gpu=all".into()],
        init: true,
        entrypoint: Some("/sd-server".into()),
        engine: EngineArgs::Image(ImageArgs {
            files: files.as_object().cloned().unwrap_or_default(),
            args: args.as_object().cloned().unwrap_or_default(),
            caps: SdcppCaps::embedded(),
        }),
        health_path: "/sdcpp/v1/capabilities",
    }
}

/// The Z-Image-Turbo pipeline the spike measured (§12), rendered end to end:
/// `--init` between `--replace` and `--name`, `--entrypoint /sd-server`
/// immediately before the image, the five unconditional server flags, and
/// every path on the `/models` mount.
#[test]
fn podman_run_argv_for_an_image_model_matches_the_expected_shape() {
    let spec = image_spec(
        json!({
            "diffusion_model": "leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf",
            "vae": "Comfy-Org/z_image_turbo/ae.safetensors",
            "llm": "unsloth/Qwen3-4B-Instruct-2507-GGUF/Qwen3-4B-Instruct-2507-Q4_K_M.gguf"
        }),
        json!({"cfg_scale": 1.0, "steps": 8, "diffusion_fa": true}),
    );

    let args = podman_run_argv(&spec);
    let expected: Vec<String> = [
        "run",
        "-d",
        "--replace",
        "--init",
        "--name",
        "lmgw-image-z-image-turbo-a1b2c3",
        "--label",
        "lmgw.instance=lmgw",
        "--label",
        "lmgw.class=image",
        "--label",
        "lmgw.model=z-image-turbo",
        "--label",
        "lmgw.engine=sdcpp",
        "--device",
        "nvidia.com/gpu=all",
        "-p",
        "127.0.0.1:9301:8080",
        "-v",
        "/srv/image-models:/models:ro",
        "--entrypoint",
        "/sd-server",
        "ghcr.io/leejet/stable-diffusion.cpp:master-cuda",
        "--listen-ip",
        "0.0.0.0",
        "--listen-port",
        "8080",
        "--eager-load",
        "--lora-model-dir",
        "/models/loras",
        "--hires-upscalers-dir",
        "/models/upscalers",
        // `files` renders in the map's own (sorted) order.
        "--diffusion-model",
        "/models/leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf",
        "--llm",
        "/models/unsloth/Qwen3-4B-Instruct-2507-GGUF/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
        "--vae",
        "/models/Comfy-Org/z_image_turbo/ae.safetensors",
        // then `args`, likewise sorted.
        "--cfg-scale",
        "1.0",
        "--diffusion-fa",
        "--steps",
        "8",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(args, expected);
}

/// The trap this class exists to avoid: sd-server's long flags mix separators,
/// so a canonical key has to be looked up in the image's vocabulary rather
/// than transformed. `clip_l` is **not** `--clip-l`, and `diffusion_model` is
/// **not** `--diffusion_model`.
#[test]
fn a_key_is_spelled_by_the_images_vocabulary_not_by_substitution() {
    let spec = image_spec(
        json!({
            "diffusion_model": "flux/flux1-schnell-q8_0.gguf",
            "clip_l": "encoders/clip_l.safetensors",
            "clip_g": "encoders/clip_g.safetensors",
            "t5xxl": "encoders/t5xxl_fp16.safetensors",
            "llm_vision": "encoders/vit.gguf",
            "control_net": "extras/controlnet.safetensors",
            "high_noise_diffusion_model": "wan/high-noise.gguf"
        }),
        json!({"offload_to_cpu": true, "vae_tiling": true}),
    );
    let args = render_image_args(&spec).unwrap();
    for (flag, value) in [
        ("--clip_l", "/models/encoders/clip_l.safetensors"),
        ("--clip_g", "/models/encoders/clip_g.safetensors"),
        ("--t5xxl", "/models/encoders/t5xxl_fp16.safetensors"),
        ("--llm_vision", "/models/encoders/vit.gguf"),
        ("--control-net", "/models/extras/controlnet.safetensors"),
        (
            "--high-noise-diffusion-model",
            "/models/wan/high-noise.gguf",
        ),
        ("--diffusion-model", "/models/flux/flux1-schnell-q8_0.gguf"),
    ] {
        assert!(has_pair(&args, flag, value), "{flag} missing from {args:?}");
    }
    assert!(args.contains(&"--offload-to-cpu".to_string()), "{args:?}");
    assert!(args.contains(&"--vae-tiling".to_string()), "{args:?}");
    // The spellings a substitution rule would have produced.
    for wrong in ["--clip-l", "--clip-g", "--diffusion_model", "--control_net"] {
        assert!(!args.iter().any(|a| a == wrong), "{wrong} in {args:?}");
    }
}

/// The five flags §12 made unconditional are claimed the way
/// `render_llama_args` claims `model`/`alias`/`host`/`port`: a row may not
/// take lmgw off `0.0.0.0:8080`, may not go back to lazy loading, and may not
/// drop the two directory flags whose absence makes the capabilities route
/// throw.
#[test]
fn args_cannot_override_the_five_unconditional_flags() {
    let spec = image_spec(
        json!({"model": "sdxl/sd_xl_base_1.0.safetensors"}),
        json!({
            "listen_ip": "127.0.0.1",
            "listen_port": 1234,
            "eager_load": false,
            "lora_model_dir": "/somewhere/else",
            "hires_upscalers_dir": "/somewhere/else"
        }),
    );
    let args = render_image_args(&spec).unwrap();
    assert!(has_pair(&args, "--listen-ip", "0.0.0.0"), "{args:?}");
    assert!(has_pair(&args, "--listen-port", "8080"), "{args:?}");
    assert!(!args.iter().any(|a| a == "127.0.0.1"), "{args:?}");
    assert!(!args.iter().any(|a| a == "1234"), "{args:?}");
    assert_eq!(
        args.iter().filter(|a| *a == "--eager-load").count(),
        1,
        "{args:?}"
    );
    assert!(
        has_pair(&args, "--lora-model-dir", "/models/loras"),
        "{args:?}"
    );
    assert!(
        has_pair(&args, "--hires-upscalers-dir", "/models/upscalers"),
        "{args:?}"
    );
    assert!(!args.iter().any(|a| a == "/somewhere/else"), "{args:?}");
}

/// The same claim, under every **alias** the build declares for those flags.
///
/// `flag_for` resolves `l` → `--listen-ip` and `m` → `--model`, so a guard
/// that tested the stored spelling let an alias walk straight past it and
/// render a second copy of a flag lmgw had already spelled — the image twin of
/// `short_alias_collision_loses_to_the_structured_field`.
#[test]
fn an_alias_of_a_claimed_flag_is_dropped_like_the_canonical_key() {
    for spelling in ["l", "-l", "--l", "listen-ip", "listen_ip"] {
        let spec = image_spec(
            json!({"model": "sdxl/sd_xl_base_1.0.safetensors"}),
            json!({ spelling: "127.0.0.1" }),
        );
        let args = render_image_args(&spec).unwrap();
        assert_eq!(
            args.iter().filter(|a| *a == "--listen-ip").count(),
            1,
            "{spelling}: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "127.0.0.1"),
            "{spelling}: {args:?}"
        );
        assert!(has_pair(&args, "--listen-ip", "0.0.0.0"), "{args:?}");
    }
}

/// `-m` is `--model`, which makes it the other half of the `model` /
/// `diffusion_model` exclusivity: a row spelling its checkpoint `m` beside a
/// `diffusion_model` names both ways to load a pipeline, and the renderer has
/// to see that as the pair it is rather than as two unrelated keys.
#[test]
fn the_model_alias_counts_against_the_diffusion_model_exclusivity() {
    let both = image_spec(
        json!({"m": "sdxl/sd_xl_base_1.0.safetensors",
               "diffusion_model": "z/dit.gguf"}),
        json!({}),
    );
    let e = render_image_args(&both).unwrap_err();
    assert!(e.contains("alternatives, not a pair"), "{e}");

    // And on its own it *is* the checkpoint, rendered under the build's own
    // spelling of the flag.
    let alone = image_spec(json!({"m": "sdxl/sd_xl_base_1.0.safetensors"}), json!({}));
    let args = render_image_args(&alone).unwrap();
    assert!(
        has_pair(&args, "--model", "/models/sdxl/sd_xl_base_1.0.safetensors"),
        "{args:?}"
    );
    assert!(!args.iter().any(|a| a == "--m"), "{args:?}");
}

/// A row *may* repoint the two directories — through `files`, where paths
/// live — and the flag itself still goes out, rewritten onto the mount.
#[test]
fn a_row_repoints_the_directory_flags_without_removing_them() {
    let spec = image_spec(
        json!({
            "model": "sdxl/sd_xl_base_1.0.safetensors",
            "lora_model_dir": "shared/loras",
            "hires_upscalers_dir": "/shared/upscalers"
        }),
        json!({}),
    );
    let args = render_image_args(&spec).unwrap();
    assert!(
        has_pair(&args, "--lora-model-dir", "/models/shared/loras"),
        "{args:?}"
    );
    assert!(
        has_pair(&args, "--hires-upscalers-dir", "/models/shared/upscalers"),
        "{args:?}"
    );
    // Each exactly once: the directory keys are claimed, so the `files` loop
    // does not render them a second time.
    assert_eq!(
        args.iter().filter(|a| *a == "--lora-model-dir").count(),
        1,
        "{args:?}"
    );
    assert_eq!(
        args.iter()
            .filter(|a| *a == "--hires-upscalers-dir")
            .count(),
        1,
        "{args:?}"
    );
}

/// Switch-vs-value comes from the stored JSON type, which is what the editor
/// and the tools write: `true` is the flag on its own, `false`/`null` say
/// nothing at all (sd-server has no `--no-*` twins), and a list repeats the
/// flag — what "can be used multiple times" options want.
#[test]
fn args_render_by_their_json_type() {
    let spec = image_spec(
        json!({"model": "sdxl/model.safetensors"}),
        json!({
            "diffusion_fa": true,
            "vae_tiling": false,
            "clip_skip": null,
            "type": "q8_0",
            "steps": 8,
            "cfg_scale": 1.5,
            "ref_image": ["a.png", "b.png"]
        }),
    );
    let args = render_image_args(&spec).unwrap();
    assert!(args.contains(&"--diffusion-fa".to_string()), "{args:?}");
    assert!(!args.iter().any(|a| a == "--vae-tiling"), "{args:?}");
    assert!(!args.iter().any(|a| a == "--clip-skip"), "{args:?}");
    assert!(has_pair(&args, "--type", "q8_0"), "{args:?}");
    assert!(has_pair(&args, "--steps", "8"), "{args:?}");
    assert!(has_pair(&args, "--cfg-scale", "1.5"), "{args:?}");
    assert!(has_pair(&args, "--ref-image", "a.png"), "{args:?}");
    assert!(has_pair(&args, "--ref-image", "b.png"), "{args:?}");
    assert_eq!(
        args.iter().filter(|a| *a == "--ref-image").count(),
        2,
        "{args:?}"
    );
}

/// `-m` and `--diffusion-model` are the two ways to name a pipeline (§2.6),
/// and they are alternatives: sd-server loads one or the other.
#[test]
fn exactly_one_of_model_and_diffusion_model_is_required() {
    let both = image_spec(
        json!({"model": "a.safetensors", "diffusion_model": "b.gguf"}),
        json!({}),
    );
    let err = render_image_args(&both).unwrap_err();
    assert!(err.contains("both"), "{err}");

    let neither = image_spec(json!({"vae": "ae.safetensors"}), json!({}));
    let err = render_image_args(&neither).unwrap_err();
    assert!(err.contains("neither"), "{err}");

    let checkpoint = image_spec(json!({"model": "sdxl/model.safetensors"}), json!({}));
    let args = render_image_args(&checkpoint).unwrap();
    assert!(
        has_pair(&args, "--model", "/models/sdxl/model.safetensors"),
        "{args:?}"
    );
}

/// A key the image has no flag for is refused by name rather than passed
/// through: sd-server answers an unknown flag with a usage dump and exit 1,
/// which says nothing about which key was wrong.
#[test]
fn an_unknown_key_is_refused_and_named() {
    let bad_file = image_spec(
        json!({"diffusion_model": "a.gguf", "clip-l-encoder": "c.safetensors"}),
        json!({}),
    );
    let err = render_image_args(&bad_file).unwrap_err();
    assert!(err.contains("clip-l-encoder"), "{err}");

    let bad_arg = image_spec(
        json!({"diffusion_model": "a.gguf"}),
        json!({"diffusion_mode": "turbo"}),
    );
    let err = render_image_args(&bad_arg).unwrap_err();
    assert!(
        err.contains("diffusion_mode") && err.contains("--diffusion-model"),
        "{err}"
    );
}

/// The two renderers stay in their lanes: `render_llama_args` on an image spec
/// is empty, and `render_engine_args` dispatches to the right one.
#[test]
fn render_llama_args_is_empty_for_an_image_spec() {
    let spec = image_spec(json!({"model": "m.safetensors"}), json!({}));
    assert!(render_llama_args(&spec).is_empty());
    assert_eq!(
        render_engine_args(&spec),
        render_image_args(&spec).unwrap(),
        "the dispatcher renders what the image renderer does"
    );
}

/// A row the renderer refuses comes back empty from the *dispatcher*, which
/// is what boot adoption compares against a running container's `Cmd` — and
/// an empty tail never equals a real one, so such a container is replaced
/// rather than silently adopted.
#[test]
fn render_engine_args_is_empty_for_an_image_row_that_cannot_be_rendered() {
    let spec = image_spec(json!({"vae": "ae.safetensors"}), json!({}));
    assert!(render_engine_args(&spec).is_empty());
}

#[test]
fn container_name_and_labels_carry_the_fourth_class() {
    let name = container_name("lmgw", Class::Image, "z-image-turbo");
    assert_eq!(
        name,
        format!("lmgw-image-z-image-turbo-{}", hash6("z-image-turbo"))
    );
    assert_eq!(Class::Image.engine(), "sdcpp");
}
