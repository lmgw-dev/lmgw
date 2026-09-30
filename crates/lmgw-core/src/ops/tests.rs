use crate::config::{SelfAdmin, Settings, Snapshot};

use super::*;

fn snap_bound(addr: &str) -> Snapshot {
    Snapshot {
        settings: Settings {
            bind_addr: addr.to_string(),
            ..Settings::default()
        },
        ..Snapshot::default()
    }
}

#[test]
fn self_loop_rejects_own_endpoint() {
    let s = snap_bound("127.0.0.1:8001");
    assert!(reject_self_loop(&s, "http://127.0.0.1:8001/mcp").is_err());
    assert!(reject_self_loop(&s, "http://localhost:8001/mcp").is_err());
    // Trailing slash is the same endpoint.
    assert!(reject_self_loop(&s, "http://127.0.0.1:8001/mcp/").is_err());
}

#[test]
fn self_loop_allows_other_servers() {
    let s = snap_bound("127.0.0.1:8001");
    // Different port on the same host is a different server.
    assert!(reject_self_loop(&s, "http://127.0.0.1:9000/mcp").is_ok());
    // Different host entirely.
    assert!(reject_self_loop(&s, "https://api.example.com/mcp").is_ok());
    // Our port, but not our MCP path.
    assert!(reject_self_loop(&s, "http://127.0.0.1:8001/other").is_ok());
    // Our port and a path *ending* in /mcp, which is not the aggregate
    // endpoint: an agent's own container, proxied (container-runtime
    // §3.3). The old "ends with /mcp" rule refused this and would have
    // made `provides.mcp` unregisterable; aggregating it recurses into
    // nothing, because it is one container's tools and not this
    // gateway's whole surface.
    assert!(reject_self_loop(&s, "http://127.0.0.1:8001/agents/board/mcp").is_ok());
    assert!(reject_self_loop(&s, "http://localhost:8001/agents/board/mcp").is_ok());
}

#[test]
fn the_agent_name_prefix_is_reserved() {
    // An owner-created row named this way would be adopted — and deleted —
    // by the agent lifecycle (container-runtime §3.3).
    assert!(reject_agent_name("agent:board").is_err());
    assert!(reject_agent_name("agent:").is_err());
    assert!(reject_agent_name("agents").is_ok());
    assert!(reject_agent_name("my-agent:thing").is_ok());
}

#[test]
fn self_loop_wildcard_bind_matches_any_host() {
    // Bound to every interface: every host on that port reaches us.
    let s = snap_bound("0.0.0.0:8001");
    assert!(reject_self_loop(&s, "http://192.168.1.5:8001/mcp").is_err());
    assert!(reject_self_loop(&s, "http://192.168.1.5:8002/mcp").is_ok());
}

#[test]
fn reserved_prefix_is_rejected() {
    assert!(validate_tool_prefix("gh").is_ok());
    assert!(validate_tool_prefix("").is_ok());
    assert!(validate_tool_prefix(crate::mcp::RESERVED_TOOL_PREFIX).is_err());
    assert!(validate_tool_prefix("bad prefix").is_err());
}

#[test]
fn mode_gate() {
    assert!(check_mode(SelfAdmin::Off, false).is_err());
    assert!(check_mode(SelfAdmin::ReadOnly, false).is_ok());
    assert!(check_mode(SelfAdmin::ReadOnly, true).is_err());
    assert!(check_mode(SelfAdmin::Full, true).is_ok());
}

// -----------------------------------------------------------------
// Unified-KV save-time validation (unified-KV design §3.3, decision D2)
// -----------------------------------------------------------------

#[test]
fn kv_unified_refuses_explicit_shared_pool_without_max_output() {
    // parallel > 1, no n_predict — the spec's own example.
    let err = validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(true),
        parallel: Some(2),
        ..Default::default()
    })
    .unwrap_err();
    assert!(err.contains("n_predict"), "{err}");
    // parallel unset (auto 4 slots) is the same problem — `effective_slots`
    // is what is checked, not the raw field.
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(true),
        ..Default::default()
    })
    .is_err());
    // n_predict = 0 is "no bound", same as unset.
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(true),
        parallel: Some(2),
        n_predict: Some(0),
        ..Default::default()
    })
    .is_err());
}

#[test]
fn kv_unified_accepts_with_max_output_set() {
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(true),
        parallel: Some(2),
        n_predict: Some(4096),
        // Review finding 2: a known pool size is now also required —
        // this fixture is otherwise about `n_predict` alone, so it names
        // one to keep that the case under test.
        ctx_size: Some(16_384),
        ..Default::default()
    })
    .is_ok());
    // Auto default (kv_unified unset) is left alone regardless of
    // n_predict — never refused.
    assert!(validate_kv_unified(&crate::config::LlamaParams::default()).is_ok());
    // Explicit split never needs n_predict for this rule — but does need
    // `parallel` set to something real (the next test), which this
    // fixture already has.
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(false),
        parallel: Some(2),
        ..Default::default()
    })
    .is_ok());
}

/// Review finding 3: llama-server ignores `--no-kv-unified` and forces
/// unified (and 4 slots) whenever `parallel` is left on auto, so
/// `kv_unified: false` needs `parallel` fixed to a real slot count or the
/// save is refused — it would otherwise silently do nothing.
#[test]
fn kv_unified_false_needs_parallel_set_to_a_real_slot_count() {
    let err = validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(false),
        ..Default::default()
    })
    .unwrap_err();
    assert!(err.contains("parallel"), "{err}");
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(false),
        parallel: Some(0),
        ..Default::default()
    })
    .is_err());
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(false),
        parallel: Some(1),
        ..Default::default()
    })
    .is_ok());
}

/// Review finding 2: an explicitly shared pool with more than one slot
/// needs the ledger to know its size — `--fit` can shrink an unset
/// `ctx_size` to whatever memory allows, so it is refused until either
/// `ctx_size` or `kv_unified_per_slot` is set.
#[test]
fn kv_unified_true_needs_a_known_pool_size() {
    let err = validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(true),
        parallel: Some(2),
        n_predict: Some(4096),
        ..Default::default()
    })
    .unwrap_err();
    assert!(err.contains("ctx_size"), "{err}");
    assert!(err.contains("kv_unified_per_slot"), "{err}");
    // `ctx_size` of `0` reads as unset (llama-server's own sentinel),
    // same refusal.
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(true),
        parallel: Some(2),
        n_predict: Some(4096),
        ctx_size: Some(0),
        ..Default::default()
    })
    .is_err());
    // Either field on its own is enough.
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(true),
        parallel: Some(2),
        n_predict: Some(4096),
        ctx_size: Some(16_384),
        ..Default::default()
    })
    .is_ok());
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(true),
        parallel: Some(2),
        n_predict: Some(4096),
        kv_unified_per_slot: Some(4096),
        ..Default::default()
    })
    .is_ok());
    // A single effective slot never needs a known pool size for this
    // rule (there is nothing to share).
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(true),
        parallel: Some(1),
        n_predict: Some(4096),
        ..Default::default()
    })
    .is_ok());
}

#[test]
fn kv_unified_per_slot_must_be_positive_and_only_on_a_unified_row() {
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified_per_slot: Some(0),
        ..Default::default()
    })
    .is_err());
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified_per_slot: Some(-1),
        ..Default::default()
    })
    .is_err());
    // Split row: the cap would do nothing, refused rather than ignored.
    // `parallel` is set so this hits the per-slot-cap refusal below
    // rather than the "kv_unified=false needs parallel" one above.
    let err = validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(false),
        parallel: Some(2),
        kv_unified_per_slot: Some(4096),
        ..Default::default()
    })
    .unwrap_err();
    assert!(err.contains("kv_unified_per_slot"), "{err}");
    // Unified (explicit or auto default): accepted.
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified: Some(true),
        kv_unified_per_slot: Some(4096),
        parallel: Some(1),
        ..Default::default()
    })
    .is_ok());
    assert!(validate_kv_unified(&crate::config::LlamaParams {
        kv_unified_per_slot: Some(4096),
        ..Default::default()
    })
    .is_ok());
}

#[test]
fn pairs_redact_values_but_keep_keys() {
    let env = vec![
        ("GITHUB_TOKEN".to_string(), "ghp_secret".to_string()),
        ("DEBUG".to_string(), "1".to_string()),
    ];
    let out = redact_pairs(&env, "=");
    assert_eq!(out, "GITHUB_TOKEN=<set>\nDEBUG=<set>");
    assert!(!out.contains("ghp_secret"));
}

#[test]
fn patches_are_sparse() {
    let p: UpstreamPatch = patch_from_args(Some(
        serde_json::from_str(r#"{"action":"enable","id":3}"#).unwrap(),
    ))
    .unwrap();
    assert_eq!(p.action, "enable");
    assert_eq!(p.id, Some(3));
    assert!(p.name.is_none() && p.api_key.is_none() && p.timeout_ms.is_none());
}

// -----------------------------------------------------------------
// Hugging Face repo classification
// -----------------------------------------------------------------

/// The three files of a real repo (`unsloth/Muse-Glimmer-30B-GGUF`) that
/// look alike and are configured completely differently. Getting this
/// wrong is what makes a caller pass a projector as `gguf_path`.
#[test]
fn repo_files_are_classified_by_role() {
    let chat = |p: &str| classify_repo_file(p, "chat");
    assert_eq!(chat("Muse-Glimmer-30B-UD-Q4_K_XL.gguf"), "weights");
    assert_eq!(chat("mmproj-kquant.gguf"), "mmproj");
    assert_eq!(chat("dflash-kquant.gguf"), "drafter");
    assert_eq!(chat("MTP/mtp-gemma-4-E4B-it-Q4_0.gguf"), "drafter");
    assert_eq!(chat("Qwen3.6-27B-IQ4_XS.gguf"), "weights");
    assert_eq!(
        chat("sglang-EAGLE3-LLaMA3.1-Instruct-8B-Q8_0.gguf"),
        "drafter"
    );
    assert_eq!(chat("Qwen3-8B-DFlash-b16-Q8_0.gguf"), "drafter");
    assert_eq!(chat("qwen3-0.6b-draft-Q8_0.gguf"), "drafter");
    // A full model that carries its own MTP heads (a typical community
    // file name), and a model family that happens to be called Eagle:
    // both are weights, only a *leading* mtp/eagle names a drafter.
    assert_eq!(
        chat(
            "example/Qwen3.6-27B-Finetune-NEO-MTP-GGUF/\
                 Qwen3.6-27B-Finetune-NEO-MTP-IQ4_XS.gguf"
        ),
        "weights"
    );
    assert_eq!(chat("model-neo-mtp-iq4_xs.gguf"), "weights");
    assert_eq!(chat("Eagle-7B-Instruct-Q4_K_M.gguf"), "weights");
    assert_eq!(chat("README.md"), "other");
    // Path-qualified names classify on the basename, not the directory.
    assert_eq!(
        chat("some/dir/gemma-4-31B-it-qat-UD-Q4_K_XL.gguf"),
        "weights"
    );
    // A `.safetensors` is not a chat-class file at all, whatever it is
    // named — the GGUF-only rule of the three older classes is unchanged.
    assert_eq!(chat("model.safetensors"), "other");
    for t in ["aux", "audio"] {
        assert_eq!(classify_repo_file("m-Q4_K_M.gguf", t), "weights");
        assert_eq!(classify_repo_file("vae/ae.safetensors", t), "other");
    }
}

/// The image ladder (§7.1), over the files of the six shipped recipes.
/// Every one of these paths is a real hub path, so a regression here is a
/// regression against what an owner actually sees.
#[test]
fn image_repo_files_are_classified_by_pipeline_role() {
    let at = |repo: &str, path: &str| classify_image_file(path, Some(repo));
    // Directory evidence wins.
    assert_eq!(
        at("Comfy-Org/z_image_turbo", "split_files/vae/ae.safetensors"),
        "vae"
    );
    assert_eq!(
        at(
            "Comfy-Org/z_image_turbo",
            "split_files/text_encoders/qwen_3_4b.safetensors"
        ),
        "text_encoder"
    );
    assert_eq!(
        at(
            "Comfy-Org/z_image_turbo",
            "split_files/diffusion_models/z_image_turbo_bf16.safetensors"
        ),
        "diffusion"
    );
    assert_eq!(
        at(
            "QuantStack/Qwen-Image-GGUF",
            "VAE/Qwen_Image-VAE.safetensors"
        ),
        "vae"
    );
    // Filename markers.
    assert_eq!(
        at("comfyanonymous/flux_text_encoders", "clip_l.safetensors"),
        "text_encoder"
    );
    assert_eq!(
        at(
            "comfyanonymous/flux_text_encoders",
            "t5xxl_fp16.safetensors"
        ),
        "text_encoder"
    );
    assert_eq!(
        at("madebyollin/sdxl-vae-fp16-fix", "sdxl_vae.safetensors"),
        "vae"
    );
    // The family-name tiebreak: same vendor word, opposite roles.
    assert_eq!(
        at("QuantStack/Qwen-Image-GGUF", "Qwen_Image-Q4_K_M.gguf"),
        "diffusion"
    );
    assert_eq!(
        at(
            "unsloth/Qwen3-4B-Instruct-2507-GGUF",
            "Qwen3-4B-Instruct-2507-Q4_K_M.gguf"
        ),
        "text_encoder"
    );
    assert_eq!(
        at("unsloth/Qwen2.5-VL-7B-Instruct-GGUF", "mmproj-F16.gguf"),
        "text_encoder"
    );
    assert_eq!(
        at("leejet/Z-Image-Turbo-GGUF", "z_image_turbo-Q4_K.gguf"),
        "diffusion"
    );
    assert_eq!(
        at("leejet/FLUX.1-schnell-gguf", "flux1-schnell-q8_0.gguf"),
        "diffusion"
    );
    assert_eq!(
        at("city96/FLUX.1-dev-gguf", "flux1-dev-Q4_K_S.gguf"),
        "diffusion"
    );
    // An all-in-one checkpoint at the repo root.
    assert_eq!(
        at(
            "stabilityai/stable-diffusion-xl-base-1.0",
            "sd_xl_base_1.0.safetensors"
        ),
        "checkpoint"
    );
    assert_eq!(
        at(
            "stabilityai/stable-diffusion-xl-base-1.0",
            "sd_xl_offset_example-lora_1.0.safetensors"
        ),
        "lora"
    );
    // Anything that is not a weights file at all.
    assert_eq!(at("leejet/Z-Image-Turbo-GGUF", "README.md"), "other");
    // Without an explicit repo the downloader's own `<owner>/<repo>/<file>`
    // layout is read back off the path — this is `gguf_files target=image`.
    assert_eq!(
        classify_repo_file("leejet/Z-Image-Turbo-GGUF/z_image_turbo-Q4_K.gguf", "image"),
        "diffusion"
    );
    assert_eq!(
        classify_repo_file(
            "Comfy-Org/z_image_turbo/split_files/vae/ae.safetensors",
            "image"
        ),
        "vae"
    );
    assert_eq!(
        classify_repo_file(
            "unsloth/Qwen3-4B-Instruct-2507-GGUF/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
            "image"
        ),
        "text_encoder"
    );
}

#[test]
fn quant_labels_come_out_of_the_filename() {
    assert_eq!(
        quant_label("Muse-Glimmer-30B-UD-Q4_K_XL.gguf").as_deref(),
        Some("UD-Q4_K_XL")
    );
    assert_eq!(
        quant_label("Qwen3.6-27B-IQ4_XS.gguf").as_deref(),
        Some("IQ4_XS")
    );
    assert_eq!(
        quant_label("translategemma-27b-it-Q4_K_M.gguf").as_deref(),
        Some("Q4_K_M")
    );
    // A split GGUF is labelled by its base name, not by the part suffix.
    assert_eq!(
        quant_label("big-model-Q8_0-00001-of-00003.gguf").as_deref(),
        Some("Q8_0")
    );
    assert_eq!(quant_label("mmproj-kquant.gguf"), None);
}

// -----------------------------------------------------------------
// Ladder validation (ladder design §4.3)
// -----------------------------------------------------------------

fn ladder_params(ctx: i64, parallel: i64, n_predict: i64) -> crate::config::LlamaParams {
    crate::config::LlamaParams {
        ctx_size: Some(ctx),
        parallel: Some(parallel),
        n_predict: Some(n_predict),
        ..Default::default()
    }
}

fn rung(gguf: &str, ctx: i64) -> crate::ladder::Rung {
    crate::ladder::Rung {
        gguf_path: gguf.into(),
        ctx_size: ctx,
    }
}

#[tokio::test]
async fn an_empty_ladder_is_never_checked() {
    // Not even the models dir is read — "a row without a ladder must
    // validate exactly as before" (§8 WP1).
    let params = crate::config::LlamaParams::default();
    assert!(
        validate_ladder("/does/not/exist", "m", "base.gguf", &params, &[], &[])
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn rule1_refuses_a_missing_or_zero_max_output() {
    let ladder = vec![rung("top.gguf", 65536)];
    for n_predict in [0, -1] {
        let params = ladder_params(32768, 2, n_predict);
        let err = validate_ladder("/does/not/exist", "m", "base.gguf", &params, &[], &ladder)
            .await
            .unwrap_err();
        assert!(err.contains("Max output"), "{err}");
    }
}

#[tokio::test]
async fn rule2_refuses_an_effectively_unified_kv_cache() {
    let ladder = vec![rung("top.gguf", 65536)];

    let mut explicit = ladder_params(32768, 2, 512);
    explicit.kv_unified = Some(true);
    let err = validate_ladder("/does/not/exist", "m", "base.gguf", &explicit, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("split"), "{err}");

    // `parallel` left on auto unifies regardless of `kv_unified` (fact 4).
    let mut auto = ladder_params(32768, 2, 512);
    auto.parallel = None;
    let err = validate_ladder("/does/not/exist", "m", "base.gguf", &auto, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("split"), "{err}");
}

#[tokio::test]
async fn rule3_refuses_an_unset_base_ctx_size() {
    let params = crate::config::LlamaParams {
        parallel: Some(2),
        n_predict: Some(512),
        ..Default::default()
    };
    let ladder = vec![rung("top.gguf", 65536)];
    let err = validate_ladder("/does/not/exist", "m", "base.gguf", &params, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("ctx_size"), "{err}");
}

#[tokio::test]
async fn rule3_refuses_a_rung_that_does_not_grow_the_per_slot_context() {
    // Base per-slot is 32768/2 = 16384; a rung of the same total ctx_size
    // gives the same per-slot number, which is not *strictly* larger.
    let params = ladder_params(32768, 2, 512);
    let ladder = vec![rung("top.gguf", 32768)];
    let err = validate_ladder("/does/not/exist", "m", "base.gguf", &params, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("rung 2"), "{err}");
    assert!(
        err.contains("strictly larger") || err.contains("does not exceed"),
        "{err}"
    );
}

#[tokio::test]
async fn rule4_refuses_a_rung_with_no_room_for_a_prompt() {
    // Base per-slot is 2048/2 = 1024, equal to n_predict — no room left.
    let params = ladder_params(2048, 2, 1024);
    let ladder = vec![rung("top.gguf", 8192)];
    let err = validate_ladder("/does/not/exist", "m", "base.gguf", &params, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("rung 1"), "{err}");
}

#[tokio::test]
async fn rule3b_refuses_a_rung_past_its_trained_context_base_included() {
    // Review finding 1: llama-server caps every slot at the trained
    // context, so a rung configured past it would promise what it cannot
    // hold. Both numbers are in the refusal.
    let dir = tempfile::tempdir().unwrap();
    crate::gguf::synth::chat("qwen3", 4096).write_to(&dir.path().join("base.gguf"));
    crate::gguf::synth::chat("qwen3", 4096).write_to(&dir.path().join("top.gguf"));
    let models_dir = dir.path().display().to_string();

    let err = validate_ladder(
        &models_dir,
        "m",
        "base.gguf",
        &ladder_params(2048, 1, 16),
        &[],
        &[rung("top.gguf", 8192)],
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("rung 2") && err.contains("trained context (4096)") && err.contains("8192"),
        "{err}"
    );

    let err = validate_ladder(
        &models_dir,
        "m",
        "base.gguf",
        &ladder_params(8192, 1, 16),
        &[],
        &[rung("top.gguf", 16384)],
    )
    .await
    .unwrap_err();
    assert!(err.contains("rung 1") && err.contains("(4096)"), "{err}");

    // Two slots of 4096 each fit a 4096-token trained context.
    assert!(validate_ladder(
        &models_dir,
        "m",
        "base.gguf",
        &ladder_params(4096, 2, 16),
        &[],
        &[rung("top.gguf", 8192)],
    )
    .await
    .is_ok());
}

#[tokio::test]
async fn the_rung_plan_reports_the_trained_context_the_slot_is_capped_at() {
    let dir = tempfile::tempdir().unwrap();
    crate::gguf::synth::chat("qwen3", 4096).write_to(&dir.path().join("top.gguf"));
    let state = crate::state::AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.router.models_dir = dir.path().display().to_string();
    crate::store::save_settings(&state.db, &settings)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let plan = ladder_rung_plan(
        &state,
        RungPlanInput {
            gguf_path: "top.gguf",
            ctx_size: 8192,
            cache_type_k: None,
            cache_type_v: None,
            mmproj_path: None,
            draft_gguf_path: None,
            n_gpu_layers: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(plan["trained_context"], 4096, "{plan}");
}

#[tokio::test]
async fn rule5_refuses_a_missing_rung_gguf() {
    let dir = tempfile::tempdir().unwrap();
    crate::gguf::synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    let models_dir = dir.path().display().to_string();
    let params = ladder_params(32768, 2, 512);
    let ladder = vec![rung("missing.gguf", 65536)];
    let err = validate_ladder(&models_dir, "m", "base.gguf", &params, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("rung 2"), "{err}");
}

#[tokio::test]
async fn rule5_refuses_a_rung_that_is_a_projector_not_weights() {
    let dir = tempfile::tempdir().unwrap();
    crate::gguf::synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    let mut proj = crate::gguf::synth::Header::default();
    proj.str("general.type", "mmproj")
        .str("clip.projector_type", "gemma3");
    proj.write_to(&dir.path().join("proj.gguf"));
    let models_dir = dir.path().display().to_string();
    let params = ladder_params(32768, 2, 512);
    let ladder = vec![rung("proj.gguf", 65536)];
    let err = validate_ladder(&models_dir, "m", "base.gguf", &params, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("not weights"), "{err}");
}

#[tokio::test]
async fn rule5_refuses_an_architecture_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    crate::gguf::synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    crate::gguf::synth::chat("llama", 65536).write_to(&dir.path().join("top.gguf"));
    let models_dir = dir.path().display().to_string();
    let params = ladder_params(32768, 2, 512);
    let ladder = vec![rung("top.gguf", 65536)];
    let err = validate_ladder(&models_dir, "m", "base.gguf", &params, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("architecture"), "{err}");
}

#[tokio::test]
async fn rule5_refuses_a_tokenizer_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let mut base = crate::gguf::synth::Header::default();
    base.str("general.architecture", "qwen3")
        .u32("qwen3.context_length", 32768)
        .u32("qwen3.block_count", 4)
        .str("tokenizer.ggml.model", "gpt2")
        .str("tokenizer.ggml.pre", "qwen2");
    base.write_to(&dir.path().join("base.gguf"));
    let mut top = crate::gguf::synth::Header::default();
    top.str("general.architecture", "qwen3")
        .u32("qwen3.context_length", 65536)
        .u32("qwen3.block_count", 4)
        .str("tokenizer.ggml.model", "gpt2")
        .str("tokenizer.ggml.pre", "llama3"); // different from the base
    top.write_to(&dir.path().join("top.gguf"));
    let models_dir = dir.path().display().to_string();
    let params = ladder_params(32768, 2, 512);
    let ladder = vec![rung("top.gguf", 65536)];
    let err = validate_ladder(&models_dir, "m", "base.gguf", &params, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("tokenize identically"), "{err}");
}

#[tokio::test]
async fn rule5_refuses_a_chat_template_mismatch_when_unpinned() {
    let dir = tempfile::tempdir().unwrap();
    crate::gguf::synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    let mut top = crate::gguf::synth::Header::default();
    top.str("general.architecture", "qwen3")
        .u32("qwen3.context_length", 65536)
        .u32("qwen3.block_count", 4)
        .str("tokenizer.chat_template", "a different template entirely");
    top.write_to(&dir.path().join("top.gguf"));
    let models_dir = dir.path().display().to_string();
    let params = ladder_params(32768, 2, 512); // chat_template_file unset
    let ladder = vec![rung("top.gguf", 65536)];
    let err = validate_ladder(&models_dir, "m", "base.gguf", &params, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("chat template"), "{err}");

    // Pinning the template with chat_template_file makes the embedded
    // one irrelevant — llama-server renders the pinned file on every
    // rung, so a mismatched *embedded* template is no longer a problem.
    let mut pinned = params;
    pinned.chat_template_file = Some("template.jinja".into());
    assert!(
        validate_ladder(&models_dir, "m", "base.gguf", &pinned, &[], &ladder)
            .await
            .is_ok()
    );
}

/// Review finding 11: `common_chat_templates_init` picks a named
/// variant (`tokenizer.chat_template.tool_use`) when a request carries
/// tools, so two rungs that agree on the bare template but differ on a
/// named one still render tool calls differently — comparing only the
/// bare key would have missed this.
#[tokio::test]
async fn rule5_refuses_a_named_chat_template_mismatch_when_unpinned() {
    let dir = tempfile::tempdir().unwrap();
    crate::gguf::synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    let mut top = crate::gguf::synth::Header::default();
    top.str("general.architecture", "qwen3")
        .u32("qwen3.context_length", 65536)
        .u32("qwen3.block_count", 4)
        // Same bare template as the base's `synth::chat`...
        .str("tokenizer.chat_template", "{{ messages }}")
        // ...but a named variant this rung alone carries.
        .str("tokenizer.chat_template.tool_use", "{{ tools }}");
    top.write_to(&dir.path().join("top.gguf"));
    let models_dir = dir.path().display().to_string();
    let params = ladder_params(32768, 2, 512);
    let ladder = vec![rung("top.gguf", 65536)];
    let err = validate_ladder(&models_dir, "m", "base.gguf", &params, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("chat template"), "{err}");
}

/// Review finding 12: a rung of another embedding width passes rule 5
/// (same architecture, same tokenizer — neither reads
/// `embedding_length`), and only fails once a projector is actually
/// paired with it at load time. This is the live check's 0.8B → 2B
/// slider, if it had an `mmproj`.
#[tokio::test]
async fn rule5_refuses_a_projector_incompatible_rung_embedding_width() {
    let dir = tempfile::tempdir().unwrap();
    let mut base = crate::gguf::synth::Header::default();
    base.str("general.architecture", "qwen3")
        .u32("qwen3.context_length", 32768)
        .u32("qwen3.block_count", 4)
        .u32("qwen3.embedding_length", 2048);
    base.write_to(&dir.path().join("base.gguf"));
    let mut top = crate::gguf::synth::Header::default();
    top.str("general.architecture", "qwen3")
        .u32("qwen3.context_length", 65536)
        .u32("qwen3.block_count", 4)
        .u32("qwen3.embedding_length", 4096); // a different width
    top.write_to(&dir.path().join("top.gguf"));
    let mut proj = crate::gguf::synth::Header::default();
    proj.str("clip.projector_type", "gemma3")
        .u32("clip.vision.image_size", 896)
        .u32("clip.vision.patch_size", 14)
        .u32("clip.vision.projector.scale_factor", 4);
    proj.write_to(&dir.path().join("proj.gguf"));
    let models_dir = dir.path().display().to_string();
    let mut params = ladder_params(32768, 2, 512);
    params.mmproj_path = Some("proj.gguf".into());
    let ladder = vec![rung("top.gguf", 65536)];
    let err = validate_ladder(&models_dir, "m", "base.gguf", &params, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("embedding_length"), "{err}");
}

#[tokio::test]
async fn rule6_refuses_draft_mtp_without_a_draft_file_or_mtp_layers() {
    let dir = tempfile::tempdir().unwrap();
    crate::gguf::synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    crate::gguf::synth::chat("qwen3", 65536).write_to(&dir.path().join("top.gguf"));
    let models_dir = dir.path().display().to_string();
    let mut params = ladder_params(32768, 2, 512);
    params.spec_type = Some("draft-mtp".into());
    let ladder = vec![rung("top.gguf", 65536)];
    let err = validate_ladder(&models_dir, "m", "base.gguf", &params, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("MTP"), "{err}");
    assert!(err.contains("rung 2"), "{err}");
}

#[tokio::test]
async fn rule6_passes_when_the_rung_itself_carries_mtp_layers() {
    let dir = tempfile::tempdir().unwrap();
    crate::gguf::synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    // A full model with its own MTP heads: enough layers that
    // `role_of` still calls it weights (`has_mtp_layers && block_count
    // <= 8` is the drafter heuristic), plus a tensor name that trips
    // `has_mtp_layers`.
    let mut top = crate::gguf::synth::Header::default();
    top.str("general.architecture", "qwen3")
        .u32("qwen3.context_length", 65536)
        .u32("qwen3.block_count", 30)
        // Same embedded template as the base's `synth::chat` — rule 5's
        // template check is not what this test is about.
        .str("tokenizer.chat_template", "{{ messages }}")
        .tensor("blk.0.nextn.embed_tokens");
    top.write_to(&dir.path().join("top.gguf"));
    let models_dir = dir.path().display().to_string();
    let mut params = ladder_params(32768, 2, 512);
    params.spec_type = Some("draft-mtp".into());
    let ladder = vec![rung("top.gguf", 65536)];
    assert!(
        validate_ladder(&models_dir, "m", "base.gguf", &params, &[], &ladder)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn rule7_refuses_a_projector_with_no_known_per_image_bound() {
    let dir = tempfile::tempdir().unwrap();
    crate::gguf::synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    crate::gguf::synth::chat("qwen3", 65536).write_to(&dir.path().join("top.gguf"));
    // Non-causal (NON_CAUSAL_PROJECTORS) but not one of the special-cased
    // types `image_token_bound` has a formula for — Unmeasured.
    let mut proj = crate::gguf::synth::Header::default();
    proj.str("clip.projector_type", "deepseek4v");
    proj.write_to(&dir.path().join("proj.gguf"));
    let models_dir = dir.path().display().to_string();
    let mut params = ladder_params(32768, 2, 512);
    params.mmproj_path = Some("proj.gguf".into());
    let ladder = vec![rung("top.gguf", 65536)];
    let err = validate_ladder(&models_dir, "m", "base.gguf", &params, &[], &ladder)
        .await
        .unwrap_err();
    assert!(err.contains("image-max-tokens"), "{err}");

    // The row's own flag lifts the refusal.
    let args = vec!["--image-max-tokens".to_string(), "900".to_string()];
    assert!(
        validate_ladder(&models_dir, "m", "base.gguf", &params, &args, &ladder)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_valid_three_rung_ladder_passes_every_rule() {
    let dir = tempfile::tempdir().unwrap();
    crate::gguf::synth::chat("qwen3", 32768).write_to(&dir.path().join("base.gguf"));
    crate::gguf::synth::chat("qwen3", 65536).write_to(&dir.path().join("mid.gguf"));
    crate::gguf::synth::chat("qwen3", 131072).write_to(&dir.path().join("top.gguf"));
    let models_dir = dir.path().display().to_string();
    let params = ladder_params(32768, 2, 512);
    let ladder = vec![rung("mid.gguf", 65536), rung("top.gguf", 131072)];
    assert!(
        validate_ladder(&models_dir, "m", "base.gguf", &params, &[], &ladder)
            .await
            .is_ok()
    );
}
