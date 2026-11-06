use super::*;

fn summary(arch: &str) -> ModelSummary {
    ModelSummary {
        architecture: Some(arch.to_string()),
        ..ModelSummary::default()
    }
}

#[test]
fn a_dflash_drafter_drafts_one_token_fewer_than_its_block() {
    for (block, want) in [(8, 7), (16, 15)] {
        let mut d = summary("dflash");
        d.block_size = Some(block);
        let (n, why) = draft_window("draft-dflash", &d);
        assert_eq!(n, Some(want));
        assert!(why.contains(&format!("block size is {block} (dflash.block_size)")));
    }
}

#[test]
fn a_dflash_header_without_a_block_size_leaves_the_window_out() {
    let (n, note) = draft_window("draft-dflash", &summary("dflash"));
    assert_eq!(n, None);
    assert!(note.contains("no dflash.block_size"));
    // A zero block is no block size either.
    let mut d = summary("dflash");
    d.block_size = Some(0);
    assert_eq!(draft_window("draft-dflash", &d).0, None);
}

#[test]
fn other_drafters_keep_the_working_default_window() {
    assert_eq!(draft_window("draft-mtp", &summary("qwen35")).0, Some(4));
    assert_eq!(draft_window("draft-eagle3", &summary("eagle3")).0, Some(4));
}

#[test]
fn model_ids_drop_the_quantization_label() {
    assert_eq!(
        suggest_model_id("unsloth/Muse-Glimmer-30B-GGUF/Muse-Glimmer-30B-UD-Q4_K_XL.gguf"),
        "muse-glimmer-30b"
    );
    assert_eq!(suggest_model_id("Qwen3.6-27B-IQ4_XS.gguf"), "qwen3.6-27b");
    assert_eq!(
        suggest_model_id("translategemma-27b-it-Q4_K_M.gguf"),
        "translategemma-27b-it"
    );
    // Nothing to strip is not an error.
    assert_eq!(suggest_model_id("plainmodel.gguf"), "plainmodel");
}

/// The classification that keeps an embedding model out of the chat
/// table: decided by the header's pooling_type / classifier head, never
/// by the filename or the architecture name a chat model shares.
#[test]
fn encoders_classify_as_aux_by_their_header() {
    // Qwen3-Embedding is architecture `qwen3` — the same as the chat
    // model — and differs only in pooling_type.
    let mut embed = summary("qwen3");
    embed.pooling_type = Some(3);
    assert_eq!(class_for(&embed), ("aux", Some(AuxKind::Embed)));

    let mut rerank = summary("bert");
    rerank.pooling_type = Some(4);
    assert_eq!(class_for(&rerank), ("aux", Some(AuxKind::Rerank)));

    // An older reranker conversion: no pooling_type, but the head.
    let mut old_rerank = summary("bert");
    old_rerank.has_classifier_head = true;
    assert_eq!(class_for(&old_rerank), ("aux", Some(AuxKind::Rerank)));

    // A BERT-family encoder with neither is still an embedder.
    assert_eq!(
        class_for(&summary("nomic-bert")),
        ("aux", Some(AuxKind::Embed))
    );

    // A chat model, and a chat model's companions.
    let mut chat = summary("qwen35");
    chat.has_chat_template = true;
    assert_eq!(class_for(&chat), ("chat", None));
    let mut proj = summary("clip");
    proj.is_mmproj = true;
    proj.pooling_type = Some(1);
    assert_eq!(class_for(&proj), ("chat", None));
    assert_eq!(class_for(&summary("dflash")), ("chat", None));
}

/// A chat row carrying `--embedding` in its extra args (the shape the
/// old workaround took) is probed as an embedder, not by generation.
#[test]
fn a_chat_row_flagged_as_an_embedder_is_probed_for_embeddings() {
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(args_request_embeddings(&args(&[
        "--embedding",
        "--pooling",
        "last"
    ])));
    assert!(args_request_embeddings(&args(&["--embeddings"])));
    assert!(args_request_embeddings(&args(&["--embedding=true"])));
    assert!(!args_request_embeddings(&args(&["--no-warmup"])));
    assert!(!args_request_embeddings(&[]));
}

#[test]
fn draft_architectures_map_to_their_spec_type() {
    assert_eq!(spec_type_for(&summary("dflash")), Some("draft-dflash"));
    assert_eq!(spec_type_for(&summary("dspark")), Some("draft-dspark"));
    assert_eq!(spec_type_for(&summary("eagle3")), Some("draft-eagle3"));
    // A plain model with no MTP heads cannot self-draft.
    assert_eq!(spec_type_for(&summary("qwen35")), None);

    let mut mtp = summary("qwen35");
    mtp.has_mtp_layers = true;
    assert_eq!(spec_type_for(&mtp), Some("draft-mtp"));

    // A projector is never a drafter, whatever else it says.
    let mut proj = summary("clip");
    proj.is_mmproj = true;
    proj.has_mtp_layers = true;
    assert_eq!(spec_type_for(&proj), None);
}

#[test]
fn a_full_model_carrying_mtp_tensors_is_still_weights() {
    // The trap: `-MTP-GGUF` repos ship full models *with* MTP tensors, so
    // tensor presence alone would misfile a 27B model as a drafter.
    let mut big = summary("qwen35");
    big.has_mtp_layers = true;
    big.block_count = Some(64);
    assert_eq!(role_of(&big), "weights");

    let mut module = summary("qwen35");
    module.has_mtp_layers = true;
    module.block_count = Some(1);
    assert_eq!(role_of(&module), "drafter");

    assert_eq!(role_of(&summary("dflash")), "drafter");
    let mut proj = summary("clip");
    proj.is_mmproj = true;
    assert_eq!(role_of(&proj), "mmproj");
    assert_eq!(role_of(&ModelSummary::default()), "unknown");
}

// -- the batch a projector needs -----------------------------------------

fn with_projector() -> LlamaParams {
    LlamaParams {
        mmproj_path: Some("gemma4/mmproj-BF16.gguf".into()),
        ..LlamaParams::default()
    }
}

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// Gemma 4 12B's projector, the crash case.
fn gemma4uv() -> ImageAttention {
    ImageAttention::NonCausal {
        ty: "gemma4uv".into(),
        tokens: ImageTokens::Measured(GEMMA4_IMAGE_UBATCH),
    }
}

fn advise(params: &LlamaParams, args: &[String]) -> Option<String> {
    projector_ubatch_advisory(params, args, &gemma4uv())
}

fn projector(ty: Option<&str>) -> ModelSummary {
    ModelSummary {
        is_mmproj: true,
        has_vision_encoder: Some(true),
        projector_type: ty.map(str::to_string),
        ..ModelSummary::default()
    }
}

/// A Gemma 3 projector as its header states it: 896 px images in 14 px
/// patches, no scale-factor key.
fn gemma3_projector() -> ModelSummary {
    ModelSummary {
        vision_image_size: Some(896),
        vision_patch_size: Some(14),
        ..projector(Some("gemma3"))
    }
}

/// The projector types and widths measured (2026-09-24),
/// against what `mtmd_decode_use_non_causal` in its llama.cpp answers.
#[test]
fn image_attention_follows_llama_cpps_non_causal_list() {
    let measured = |ty: &str| ImageAttention::NonCausal {
        ty: ty.to_string(),
        tokens: ImageTokens::Measured(1280),
    };
    // Gemma 4 12B: always non-causal.
    assert_eq!(
        image_attention(Some(&projector(Some("gemma4uv"))), Some(3840)),
        measured("gemma4uv")
    );
    // gemma4v depends on the text model: 26B-A4B and 31B are non-causal,
    // E2B and E4B causal.
    let g4v = projector(Some("gemma4v"));
    assert_eq!(image_attention(Some(&g4v), Some(2816)), measured("gemma4v"));
    assert_eq!(image_attention(Some(&g4v), Some(5376)), measured("gemma4v"));
    assert_eq!(
        image_attention(Some(&g4v), Some(2560)),
        ImageAttention::Causal
    );
    assert_eq!(
        image_attention(Some(&g4v), Some(1536)),
        ImageAttention::Causal
    );
    // Without the weights' width, the projector's own projection_dim is
    // the same number.
    let e4b = ModelSummary {
        vision_projection_dim: Some(2560),
        ..projector(Some("gemma4v"))
    };
    assert_eq!(image_attention(Some(&e4b), None), ImageAttention::Causal);
    assert!(matches!(
        image_attention(Some(&g4v), None),
        ImageAttention::Unknown(why) if why.contains("gemma4v")
    ));
    // A type with no measured ceiling is still non-causal, and says so.
    assert_eq!(
        image_attention(Some(&projector(Some("deepseek4v"))), None),
        ImageAttention::NonCausal {
            ty: "deepseek4v".into(),
            tokens: ImageTokens::Unmeasured
        }
    );
    // Every other measured type decodes causally.
    for ty in [
        "qwen3vl_merger",
        "qwen2vl_merger",
        "pixtral",
        "muse-glimmer",
    ] {
        assert_eq!(
            image_attention(Some(&projector(Some(ty))), Some(4096)),
            ImageAttention::Causal,
            "{ty}"
        );
    }
    // No images at all: nothing to decode.
    let audio_only = ModelSummary {
        has_vision_encoder: Some(false),
        ..projector(Some("qwen2a"))
    };
    assert_eq!(
        image_attention(Some(&audio_only), None),
        ImageAttention::Causal
    );
}

/// Gemma 3 resizes every image to one square, so its token count is a
/// property of the header: (896 / 14)^2 / 4^2 = 256. That fits llama.cpp's
/// default ubatch of 512, so there is nothing to plan or warn about.
#[test]
fn gemma3_images_are_a_fixed_256_tokens_and_fit_the_default() {
    let attention = image_attention(Some(&gemma3_projector()), None);
    assert_eq!(
        attention,
        ImageAttention::NonCausal {
            ty: "gemma3".into(),
            tokens: ImageTokens::Fixed(256)
        }
    );
    let (floor, basis) = projector_ubatch_floor(&attention, &[]).unwrap();
    assert_eq!(floor, 256);
    assert!(basis.contains("exactly 256 tokens"), "{basis}");
    assert!(floor <= LLAMA_DEFAULT_UBATCH);
    assert_eq!(
        projector_ubatch_advisory(&with_projector(), &[], &attention),
        None
    );
    // A fixed-size projector ignores --image-max-tokens.
    assert_eq!(
        projector_ubatch_floor(&attention, &argv(&["--image-max-tokens", "4096"]))
            .unwrap()
            .0,
        256
    );
    // Below 256 it would still abort, and says so.
    let tiny = LlamaParams {
        ubatch_size: Some(128),
        ..with_projector()
    };
    let w = projector_ubatch_advisory(&tiny, &[], &attention).expect("must advise");
    assert!(w.contains("aborts llama-server"), "{w}");
    assert!(w.contains("at least 256"), "{w}");
    // The header's own scale factor, where it states one, is used.
    let scaled = ModelSummary {
        vision_scale_factor: Some(2),
        ..gemma3_projector()
    };
    assert!(matches!(
        image_attention(Some(&scaled), None),
        ImageAttention::NonCausal {
            tokens: ImageTokens::Fixed(1024),
            ..
        }
    ));
}

/// Not knowing is not the same as knowing it is safe.
#[test]
fn an_unreadable_untyped_or_unknown_projector_is_unknown() {
    for (p, says) in [
        (None, "could not be read"),
        (Some(projector(None)), "names no projector type"),
        (Some(projector(Some("brand-new-vl"))), "'brand-new-vl'"),
    ] {
        match image_attention(p.as_ref(), Some(4096)) {
            ImageAttention::Unknown(why) => assert!(why.contains(says), "{why}"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }
}

/// The crash case: a Gemma 4 row with a projector and no ubatch_size, so
/// llama.cpp's 512 was in force and a full-page image aborted the server.
#[test]
fn a_projector_at_the_default_ubatch_is_advised_about_by_the_assert_it_trips() {
    let w = advise(&with_projector(), &[]).expect("must advise");
    assert!(
        w.contains("(gemma4uv projector) with non-causal attention"),
        "{w}"
    );
    assert!(
        w.contains(
            "ubatch_size is unset, so llama.cpp's default of 512 applies: an image of more \
                 than 512 tokens aborts llama-server"
        ),
        "{w}"
    );
    assert!(
        w.contains("GGML_ASSERT \"non-causal attention requires n_ubatch >= n_tokens\""),
        "{w}"
    );
    assert!(
        w.contains("Set ubatch_size to at least 1280 (Gemma 4 images stop"),
        "{w}"
    );
    // The default batch (2048) is not the problem, so it is not named.
    assert!(!w.contains("and batch_size"), "{w}");
    assert!(
        w.contains("still starts"),
        "an advisory, not a refusal: {w}"
    );
}

/// A Qwen3.5 row: a qwen3vl_merger projector, batch_size
/// 1024 and llama.cpp's default ubatch. Causal decoding splits an image
/// across physical batches, so there is nothing to advise.
#[test]
fn a_causal_projector_gets_no_advisory_whatever_its_batch() {
    let qwen = LlamaParams {
        mmproj_path: Some("unsloth/Qwen3.5-9B-GGUF/mmproj-BF16.gguf".into()),
        batch_size: Some(1024),
        ..LlamaParams::default()
    };
    let attention = image_attention(Some(&projector(Some("qwen3vl_merger"))), Some(4096));
    assert_eq!(projector_ubatch_floor(&attention, &[]), None);
    assert_eq!(projector_ubatch_advisory(&qwen, &[], &attention), None);
}

/// When lmgw cannot tell, it still advises, says it could not tell, and
/// says where the number it uses comes from.
#[test]
fn unknown_and_unmeasured_projectors_say_what_they_do_not_know() {
    let w = projector_ubatch_advisory(
        &with_projector(),
        &[],
        &ImageAttention::Unknown("its projector's header could not be read".into()),
    )
    .expect("must advise");
    assert!(
        w.contains(
            "lmgw could not tell whether llama.cpp decodes its images with non-causal \
                 attention: its projector's header could not be read. If it does"
        ),
        "{w}"
    );
    assert!(
        w.contains("at least 1280 (with the projector unknown"),
        "{w}"
    );

    let deepseek = ImageAttention::NonCausal {
        ty: "deepseek4v".into(),
        tokens: ImageTokens::Unmeasured,
    };
    let w = projector_ubatch_advisory(&with_projector(), &[], &deepseek).expect("must advise");
    assert!(
        w.contains("no image size has been measured for deepseek4v"),
        "{w}"
    );
}

#[test]
fn a_text_only_row_or_large_enough_batches_get_no_advisory() {
    // Nothing to split without a projector.
    assert_eq!(advise(&LlamaParams::default(), &[]), None);
    // `--no-mmproj` claims the flag, so an extra-args projector never loads.
    let text_only = LlamaParams {
        no_mmproj: true,
        ..LlamaParams::default()
    };
    assert_eq!(
        advise(&text_only, &argv(&["--mmproj", "/models/x.gguf"])),
        None
    );
    // The floor exactly, with the batch left at llama.cpp's 2048.
    let at_floor = LlamaParams {
        ubatch_size: Some(GEMMA4_IMAGE_UBATCH),
        ..with_projector()
    };
    assert_eq!(advise(&at_floor, &[]), None);
    // A gemma4 row at 2048/2048.
    let live = LlamaParams {
        ubatch_size: Some(2048),
        batch_size: Some(2048),
        ..with_projector()
    };
    assert_eq!(advise(&live, &[]), None);
    // An extra arg reaches llama-server while the field is unset, so it
    // counts, in either spelling.
    for args in [&["-ub", "1280"][..], &["--ubatch-size=4096"][..]] {
        assert_eq!(advise(&with_projector(), &argv(args)), None, "{args:?}");
    }
}

/// mtmd hands an image over in pieces of at most batch_size tokens and
/// llama.cpp clamps ubatch to batch. A ubatch at or above a too-small
/// batch therefore never aborts: it splits the image, which degrades it.
#[test]
fn a_batch_too_small_for_the_image_splits_it_rather_than_aborting() {
    let clamped = LlamaParams {
        ubatch_size: Some(2048),
        batch_size: Some(1024),
        ..with_projector()
    };
    let w = advise(&clamped, &[]).expect("must advise");
    assert!(
        w.contains(
            "batch_size is 1024: an image of more than 1024 tokens is split across \
                 separate decodes. Nothing aborts"
        ),
        "{w}"
    );
    assert!(w.contains("degrades"), "{w}");
    assert!(!w.contains("aborts llama-server"), "{w}");
    // Only the field that is short is named.
    assert!(w.contains("Set batch_size to at least 1280"), "{w}");

    let equal = LlamaParams {
        ubatch_size: Some(1024),
        batch_size: Some(1024),
        ..with_projector()
    };
    let w = advise(&equal, &[]).expect("must advise");
    assert!(w.contains("split across separate decodes"), "{w}");
    assert!(
        w.contains("Set ubatch_size and batch_size to at least 1280"),
        "{w}"
    );
}

/// A ubatch below the batch aborts on any piece larger than the ubatch,
/// whatever the batch.
#[test]
fn a_ubatch_below_the_batch_aborts_and_names_every_short_field() {
    let small_batch = LlamaParams {
        batch_size: Some(1024),
        ..with_projector()
    };
    let w = advise(&small_batch, &[]).expect("must advise");
    assert!(
        w.contains("an image of more than 512 tokens aborts llama-server"),
        "{w}"
    );
    assert!(
        w.contains("Set ubatch_size and batch_size to at least 1280"),
        "{w}"
    );

    // A value that came from the extra args is not described as unset.
    let w = advise(&with_projector(), &argv(&["-ub", "1024"])).expect("must advise");
    assert!(
        w.contains("ubatch_size is 1024: an image of more than 1024"),
        "{w}"
    );
}

#[test]
fn a_raised_image_budget_raises_the_floor() {
    // The owner raised the image budget past the constant: that is the
    // ceiling now, and the constant must not hide it.
    let p = LlamaParams {
        ubatch_size: Some(2048),
        ..with_projector()
    };
    let w = advise(&p, &argv(&["--image-max-tokens", "4096"])).expect("must advise");
    assert!(
        w.contains(
            "Set ubatch_size and batch_size to at least 4096 (the row's own \
                 --image-max-tokens is 4096)"
        ),
        "{w}"
    );
    // A smaller budget does not lower the measured floor.
    assert_eq!(
        projector_ubatch_floor(&gemma4uv(), &argv(&["--image-max-tokens=256"]))
            .unwrap()
            .0,
        1280
    );
}

/// A projector outside the models dir loads, and its header cannot be
/// read from here, so it is unknown. One that is missing from the models
/// dir cannot load at all: the missing file is the problem, and there is
/// no batch advice to give about it.
#[tokio::test]
async fn an_outside_projector_is_unknown_and_a_missing_one_is_skipped() {
    let outside = argv(&["-mm", "/srv/p.gguf"]);
    let attention = row_image_attention("/models-dir", &LlamaParams::default(), &outside, None)
        .await
        .expect("an outside projector is still checked");
    assert!(
        matches!(&attention, ImageAttention::Unknown(why) if why.contains("outside")),
        "{attention:?}"
    );
    let w = projector_ubatch_advisory(&LlamaParams::default(), &outside, &attention);
    assert!(w.is_some_and(|w| w.contains("could not tell")));

    let dir = tempfile::tempdir().unwrap();
    let missing = row_image_attention(
        &dir.path().display().to_string(),
        &with_projector(),
        &[],
        None,
    )
    .await;
    assert_eq!(missing, None);
}

fn row_with(params: Value, args: &[&str]) -> LocalModel {
    serde_json::from_value(json!({
        "id": 1, "model_id": "gemma4", "gguf_path": "gemma4/w.gguf",
        "params": params, "args": args, "idle_seconds": 0,
        "enabled": true, "public": false, "warm_start": false,
    }))
    .unwrap()
}

/// What `local_model_plan` proposes for weights with a sibling projector,
/// reduced to the fields that matter here.
fn projector_plan() -> (
    serde_json::Map<String, Value>,
    serde_json::Map<String, Value>,
) {
    let params = json!({
        "mmproj_path": "gemma4/mmproj-BF16.gguf",
        "ubatch_size": GEMMA4_IMAGE_UBATCH,
    });
    let why = json!({ "mmproj_path": "sibling", "ubatch_size": "the assert" });
    (
        params.as_object().unwrap().clone(),
        why.as_object().unwrap().clone(),
    )
}

fn changed_fields(d: &Value) -> Vec<&str> {
    typed(d);
    d["change"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["field"].as_str().unwrap())
        .collect()
}

/// `d` read into the api-types `ConfiguredAs` the editors fill from,
/// which must hold it whole.
fn typed(d: &Value) -> dto::ConfiguredAs {
    let t: dto::ConfiguredAs =
        serde_json::from_value(d.clone()).unwrap_or_else(|e| panic!("{e}: {d}"));
    assert_eq!(serde_json::to_value(&t).unwrap(), *d, "ConfiguredAs drifts");
    t
}

/// A chat row's comparison, `unset` and `apply.clear` included, is the
/// typed `ConfiguredAs`: speculation its files cannot support is cleared.
#[test]
fn a_chat_row_s_comparison_reads_as_the_typed_configured_as() {
    let none = serde_json::Map::new();
    let row = row_with(
        json!({ "spec_type": "draft-mtp", "spec_draft_n_max": 3 }),
        &[],
    );
    let t = typed(&plan_diff(&row, &none, &none, false, None));
    assert!(!t.in_sync);
    let unset = t.unset.unwrap();
    assert_eq!(
        unset.iter().map(|u| u.field.as_str()).collect::<Vec<_>>(),
        ["spec_type", "spec_draft_n_max"]
    );
    let clear = t.apply.unwrap()["clear"].as_str().unwrap().to_string();
    assert_eq!(clear, "spec_type,spec_draft_n_max");
    let synced = typed(&plan_diff(
        &row_with(json!({}), &[]),
        &none,
        &none,
        false,
        None,
    ));
    assert!(synced.in_sync && synced.apply.is_none() && synced.change.is_empty());
}

#[test]
fn the_plan_diff_raises_a_small_ubatch_and_never_lowers_a_large_one() {
    let (params, why) = projector_plan();
    let mm = "gemma4/mmproj-BF16.gguf";
    let image = gemma4uv();
    let diff = |row: &LocalModel| plan_diff(row, &params, &why, false, Some(&image));

    // The crashing row: ubatch unset, batch at llama.cpp's default. Only
    // the ubatch needs to change; the patch applies as-is.
    let d = diff(&row_with(json!({ "mmproj_path": mm }), &[]));
    assert_eq!(changed_fields(&d), ["ubatch_size"], "{d}");
    assert_eq!(d["apply"]["ubatch_size"], 1280);
    assert!(d["apply"]["batch_size"].is_null(), "{d}");
    assert_eq!(d["change"][0]["why"], "the assert");
    assert!(d["change"][0]["current"].is_null(), "{d}");

    // A deliberately larger pair is in sync, not a proposal to shrink it.
    let big = json!({ "mmproj_path": mm, "ubatch_size": 2048, "batch_size": 2048 });
    assert_eq!(diff(&row_with(big, &[]))["in_sync"], true);

    // Nor is a large enough ubatch passed through the extra args.
    let d = diff(&row_with(json!({ "mmproj_path": mm }), &["-ub", "1536"]));
    assert_eq!(d["in_sync"], true, "{d}");

    // A too-small one there is what is in force, and `current` says so
    // rather than showing the empty field.
    let d = diff(&row_with(
        json!({ "mmproj_path": mm }),
        &["-ub", "1024", "-b", "1024"],
    ));
    assert_eq!(changed_fields(&d), ["ubatch_size", "batch_size"], "{d}");
    assert_eq!(d["change"][0]["current"], 1024, "{d}");
    assert_eq!(d["change"][1]["current"], 1024, "{d}");

    // A batch below the floor would split the image or clamp the new
    // ubatch, so it is raised with it, as for a Qwen3.5 row at batch_size 1024.
    let d = diff(&row_with(
        json!({ "mmproj_path": mm, "batch_size": 1024 }),
        &[],
    ));
    assert_eq!(changed_fields(&d), ["ubatch_size", "batch_size"], "{d}");
    assert_eq!(d["apply"]["batch_size"], 1280);
    assert!(d["change"][1]["why"].as_str().unwrap().contains("clamps"));

    // The row's own raised image budget is the floor it is diffed against.
    let d = diff(&row_with(
        json!({ "mmproj_path": mm }),
        &["--image-max-tokens", "4096"],
    ));
    assert_eq!(d["apply"]["ubatch_size"], 4096, "{d}");
    assert_eq!(d["apply"]["batch_size"], 4096, "{d}");
}

// -- against the real files, when LMGW_TEST_MODELS_DIR has them ---------

const MUSE: &str = "unsloth/Muse-Glimmer-30B-GGUF";
const QWEN9B: &str = "unsloth/Qwen3.5-9B-GGUF/Qwen3.5-9B-UD-Q4_K_XL.gguf";

/// The models root (`LMGW_TEST_MODELS_DIR`), or `None` with a skip note.
fn models_root() -> Option<std::path::PathBuf> {
    let root = std::env::var_os("LMGW_TEST_MODELS_DIR").map(std::path::PathBuf::from);
    if root.is_none() {
        eprintln!("skipping: LMGW_TEST_MODELS_DIR is not set");
    }
    root
}

/// Summary of the file at `rel` below the models root; `None` (skip) when
/// `LMGW_TEST_MODELS_DIR` is unset or the file is missing.
fn real(rel: &str) -> Option<ModelSummary> {
    let p = models_root()?.join(rel);
    p.is_file().then(|| gguf::summarize(&p).ok()).flatten()
}

/// Sixteen real projectors (2026-09-24), typed through the
/// header reader and classified against llama.cpp's non-causal list. The
/// Gemma 4 files name their type only under `clip.vision.projector_type`.
#[test]
fn real_projectors_decode_the_way_llama_cpp_says() {
    let nc = |ty: &str| ImageAttention::NonCausal {
        ty: ty.to_string(),
        tokens: ImageTokens::Measured(GEMMA4_IMAGE_UBATCH),
    };
    let gemma = |size: &str| {
        (
            format!("unsloth/gemma-4-{size}-it-qat-GGUF/mmproj-BF16.gguf"),
            Some(format!(
                "unsloth/gemma-4-{size}-it-qat-GGUF/gemma-4-{size}-it-qat-UD-Q4_K_XL.gguf"
            )),
        )
    };
    let causal = |path: &str| (path.to_string(), None);
    // (mmproj, the weights beside it when the width matters), type, answer.
    type Case = ((String, Option<String>), &'static str, ImageAttention);
    let cases: Vec<Case> = vec![
        (gemma("12B"), "gemma4uv", nc("gemma4uv")),
        (gemma("26B-A4B"), "gemma4v", nc("gemma4v")),
        (gemma("31B"), "gemma4v", nc("gemma4v")),
        (gemma("E4B"), "gemma4v", ImageAttention::Causal),
        (
            causal("bartowski/UI-TARS-7B-DPO-GGUF/mmproj-UI-TARS-7B-DPO-f16.gguf"),
            "qwen2vl_merger",
            ImageAttention::Causal,
        ),
        (
            causal("noctrex/Shieldstral-1.0-3B-GGUF/mmproj-BF16.gguf"),
            "pixtral",
            ImageAttention::Causal,
        ),
        (
            causal("unsloth/Muse-Glimmer-30B-GGUF/mmproj-kquant.gguf"),
            "muse-glimmer",
            ImageAttention::Causal,
        ),
        (
            causal("Qwen/Qwen3-VL-4B-Instruct-GGUF/mmproj-Qwen3VL-4B-Instruct-Q8_0.gguf"),
            "qwen3vl_merger",
            ImageAttention::Causal,
        ),
        (
            causal("example/Qwen3.6-27B-Finetune-NEO-MTP-GGUF/mmproj-BF16.gguf"),
            "qwen3vl_merger",
            ImageAttention::Causal,
        ),
    ];
    let qwen: Vec<_> = [
        "Qwen3.5-0.8B",
        "Qwen3.5-2B",
        "Qwen3.5-4B",
        "Qwen3.5-9B",
        "Qwen3.6-27B-MTP",
        "Qwen3.6-35B-A3B-MTP",
        "Qwen3.8-27B",
    ]
    .into_iter()
    .map(|m| {
        (
            causal(&format!("unsloth/{m}-GGUF/mmproj-BF16.gguf")),
            "qwen3vl_merger",
            ImageAttention::Causal,
        )
    })
    .collect();
    for ((mmproj, weights), ty, want) in cases.into_iter().chain(qwen) {
        let Some(p) = real(&mmproj) else {
            continue;
        };
        assert_eq!(p.projector_type.as_deref(), Some(ty), "{mmproj}");
        let width = weights
            .and_then(|w| real(&w))
            .and_then(|w| w.embedding_length);
        assert_eq!(image_attention(Some(&p), width), want, "{mmproj}");
    }
}

#[test]
fn real_files_classify_correctly() {
    if let Some(s) = real(&format!("{MUSE}/dflash-kquant.gguf")) {
        assert_eq!(role_of(&s), "drafter");
        assert_eq!(spec_type_for(&s), Some("draft-dflash"));
    }
    if let Some(s) = real(&format!("{MUSE}/mmproj-kquant.gguf")) {
        assert_eq!(role_of(&s), "mmproj");
        assert_eq!(spec_type_for(&s), None);
    }
    if let Some(s) = real(&format!("{MUSE}/Muse-Glimmer-30B-UD-Q4_K_XL.gguf")) {
        assert_eq!(role_of(&s), "weights");
        assert_eq!(s.context_length, Some(131072));
    }
    // A model with no MTP layers: spec_type=draft-mtp against it can never
    // load.
    if let Some(s) = real(QWEN9B) {
        assert!(!s.has_mtp_layers);
        assert_eq!(spec_type_for(&s), None);
    }
}
