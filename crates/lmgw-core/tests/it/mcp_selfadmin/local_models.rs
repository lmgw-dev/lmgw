//! Self-admin tools over `/mcp/admin` — local models: parameter coverage, read-back and advisories.

use super::*;

// ---------------------------------------------------------------------------
// Local models: full parameter coverage and read-back
// ---------------------------------------------------------------------------

/// The gap that motivated the expanded surface: a multimodal, speculatively
/// decoded model could not be expressed through the tool plane at all — the
/// projector and drafter had no fields, so they had to be typed into the
/// freeform args escape hatch with in-container paths.
///
/// Asserts the whole round trip: create with every interesting field, read it
/// back (which was itself impossible before — the list view returns only
/// names), and confirm the rendered preset carries the right keys with the
/// `/models/` prefix applied exactly once.
#[tokio::test]
async fn a_multimodal_speculative_model_round_trips_through_the_tool_plane() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    let created = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create",
            "model_id": "muse-glimmer-30b",
            "gguf_path": "unsloth/Muse-Glimmer-30B-GGUF/Muse-Glimmer-30B-UD-Q4_K_XL.gguf",
            "mmproj_path": "unsloth/Muse-Glimmer-30B-GGUF/mmproj-kquant.gguf",
            "draft_gguf_path": "unsloth/Muse-Glimmer-30B-GGUF/dflash-kquant.gguf",
            "spec_type": "draft-dflash",
            "spec_draft_n_max": 16,
            "ctx_size": 131072,
            "n_gpu_layers": 999,
            "flash_attn": "on",
            "cache_type_k": "q8_0",
            "cache_type_v": "q8_0",
            "jinja": true,
            "temp": 1.0,
            "top_p": 0.95,
            "top_k": 64,
            "reasoning_format": "auto"
        }),
    )
    .await;
    assert!(!is_error(&created), "create failed: {}", text_of(&created));

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "muse-glimmer-30b" }),
        )
        .await,
    );

    // Paths stay relative on the way out — the caller never deals with the
    // container's mount point.
    assert_eq!(
        got["params"]["mmproj_path"],
        "unsloth/Muse-Glimmer-30B-GGUF/mmproj-kquant.gguf"
    );
    assert_eq!(got["params"]["spec_type"], "draft-dflash");
    assert_eq!(got["params"]["spec_draft_n_max"], 16);
    assert_eq!(got["params"]["ctx_size"], 131072);
    assert_eq!(got["params"]["top_p"], 0.95);
    assert!(got["params"]["jinja"].as_bool().unwrap());

    // … and the command line it renders is what its container is started
    // with (§3.6 — the preset section this used to assert died with router
    // mode).
    let cmd = got["command_line"].as_str().unwrap();
    assert!(
        cmd.contains("--mmproj /models/unsloth/Muse-Glimmer-30B-GGUF/mmproj-kquant.gguf"),
        "projector missing or mis-prefixed:\n{cmd}"
    );
    assert!(
        cmd.contains("--model-draft /models/unsloth/Muse-Glimmer-30B-GGUF/dflash-kquant.gguf"),
        "drafter missing or mis-prefixed:\n{cmd}"
    );
    assert!(cmd.contains("--spec-type draft-dflash"), "{cmd}");
    assert!(cmd.contains("--ctx-size 131072"), "{cmd}");
    assert!(
        cmd.contains("--alias muse-glimmer-30b"),
        "direct mode must name the model:\n{cmd}"
    );
    assert!(!cmd.contains("/models//"), "double slash in a path:\n{cmd}");
}

/// `None` means "leave as is", so unsetting needs its own verb. Without it a
/// caller could add a drafter but never remove one.
#[tokio::test]
async fn clear_unsets_fields_that_omission_would_preserve() {
    let state = state_with_mode(SelfAdmin::Full).await;
    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;

    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "create", "model_id": "m", "gguf_path": "m.gguf",
            "spec_type": "draft-mtp", "spec_draft_n_max": 4, "ctx_size": 8192
        }),
    )
    .await;

    // An unrelated update must not disturb the drafter settings …
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "m", "ctx_size": 4096 }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "m" }),
        )
        .await,
    );
    assert_eq!(got["params"]["spec_type"], "draft-mtp");
    assert_eq!(got["params"]["ctx_size"], 4096);

    // … but naming them in `clear` must.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "m", "clear": "spec_type,spec_draft_n_max" }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "m" }),
        )
        .await,
    );
    assert!(got["params"]["spec_type"].is_null(), "{got}");
    assert!(got["params"]["spec_draft_n_max"].is_null(), "{got}");
    assert_eq!(
        got["params"]["ctx_size"], 4096,
        "clear must not touch others"
    );

    let bad = call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "m", "clear": "nonsense_field" }),
    )
    .await;
    assert!(is_error(&bad), "an unknown clear target must be reported");
    assert!(text_of(&bad).contains("nonsense_field"));
}

/// A projector llama.cpp decodes non-causally, under its default ubatch
/// (512), loads fine and then aborts llama-server on the first full-page image
/// (seen with Gemma 4's projector). Every read of the row has to say so, naming the
/// assert and the fix, as an advisory: the model starts, so none of them may
/// refuse the row or call it broken. A causal projector with the same batch
/// sizes is healthy and must stay quiet.
#[tokio::test]
async fn a_projector_row_below_the_ubatch_it_needs_gets_an_advisory_everywhere() {
    let state = state_with_mode(SelfAdmin::Full).await;

    // Headers that read as Gemma 4 12B's projector and a Qwen3.5 one: the
    // type is what decides, so a synthetic file is as good as a real one.
    let dir = tempfile::tempdir().unwrap();
    synth::chat("gemma4", 131072).write_to(&dir.path().join("gemma4/w.gguf"));
    let mut gemma = synth::Header::default();
    gemma
        .str("general.architecture", "clip")
        .str("general.type", "mmproj")
        .str("clip.vision.projector_type", "gemma4uv")
        .str("clip.audio.projector_type", "gemma4ua");
    gemma.write_to(&dir.path().join("gemma4/mmproj.gguf"));
    synth::chat("qwen35", 262144).write_to(&dir.path().join("qwen/w.gguf"));
    let mut qwen = synth::Header::default();
    qwen.str("general.architecture", "clip")
        .str("general.type", "mmproj")
        .str("clip.projector_type", "qwen3vl_merger")
        .u32("clip.vision.block_count", 27);
    qwen.write_to(&dir.path().join("qwen/mmproj.gguf"));
    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;
    let names_the_assert = |v: &Value| {
        v.as_array().unwrap().iter().any(|p| {
            let p = p.as_str().unwrap_or("");
            p.contains("non-causal attention requires n_ubatch >= n_tokens")
                && p.contains("Set ubatch_size to at least 1280")
        })
    };
    let check = |id: &'static str| {
        let (base, sid) = (base.clone(), sid.clone());
        async move {
            payload(
                &call(
                    &base,
                    &sid,
                    "lmgw__local_model_check",
                    json!({ "model_id": id }),
                )
                .await,
            )
        }
    };

    // 1. The set path: saved, with the advisory beside the warnings, not in
    //    them.
    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "gemma4", "gguf_path": "gemma4/w.gguf",
                "mmproj_path": "gemma4/mmproj.gguf",
            }),
        )
        .await,
    );
    assert_eq!(created["ok"], true, "an advisory must not block the save");
    assert!(names_the_assert(&created["advisories"]), "{created}");
    assert!(!names_the_assert(&created["warnings"]), "{created}");

    // 2. The read: an advisory, stating the type, and no problem.
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "gemma4" }),
        )
        .await,
    );
    assert!(names_the_assert(&got["advisories"]), "{got}");
    assert!(!names_the_assert(&got["problems"]), "{got}");
    let advisory = got["advisories"][0].as_str().unwrap();
    assert!(
        advisory.contains("(gemma4uv projector) with non-causal attention"),
        "a known type is stated, not hedged: {advisory}"
    );

    // 3. The check: reported, and the row is still ok — the model starts.
    let checked = check("gemma4").await;
    let row = &checked["models"][0];
    assert!(names_the_assert(&row["advisories"]), "{checked}");
    assert_eq!(
        row["ok"], true,
        "an advisory alone must not mark a row broken"
    );
    assert_eq!(row["problems"].as_array().unwrap().len(), 0, "{checked}");
    assert_eq!(checked["broken"], 0, "{checked}");
    assert_eq!(checked["with_advisories"], 1, "{checked}");

    // 4. At the floor the advisory is gone.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "gemma4", "ubatch_size": 1280 }),
    )
    .await;
    let checked = check("gemma4").await;
    assert_eq!(
        checked["models"][0]["advisories"].as_array().unwrap().len(),
        0,
        "{checked}"
    );

    // 5. A Qwen3.5-style shape: a causal projector, batch_size 1024 and the
    //    default ubatch. Healthy, so nothing to report.
    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "qwen", "gguf_path": "qwen/w.gguf",
                "mmproj_path": "qwen/mmproj.gguf", "batch_size": 1024,
            }),
        )
        .await,
    );
    assert_eq!(created["advisories"], json!([]), "{created}");
    let checked = check("qwen").await;
    assert_eq!(checked["models"][0]["ok"], true, "{checked}");
    assert_eq!(checked["models"][0]["advisories"], json!([]), "{checked}");

    // 6. A projector whose header cannot be read still gets the advisory,
    //    worded as what it is: a question lmgw could not answer.
    std::fs::create_dir_all(dir.path().join("broken")).unwrap();
    std::fs::write(dir.path().join("broken/mmproj.gguf"), b"not a gguf").unwrap();
    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "mystery", "gguf_path": "qwen/w.gguf",
                "mmproj_path": "broken/mmproj.gguf",
            }),
        )
        .await,
    );
    assert!(names_the_assert(&created["advisories"]), "{created}");
    assert!(
        created["advisories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("could not tell whether")),
        "{created}"
    );

    // 7. A projector file that has gone missing after the save is the
    //    problem; there is no batch advice to give about a file that cannot
    //    load.
    std::fs::remove_file(dir.path().join("broken/mmproj.gguf")).unwrap();
    let checked = check("mystery").await;
    let row = &checked["models"][0];
    assert_eq!(row["ok"], false, "{checked}");
    assert!(
        row["problems"][0]
            .as_str()
            .unwrap()
            .contains("mmproj_path 'broken/mmproj.gguf' is missing"),
        "{checked}"
    );
    assert_eq!(row["advisories"], json!([]), "{checked}");

    // 8. Group apply reports the advisory apart from the problems, too.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({ "action": "update", "model_id": "gemma4", "clear": "ubatch_size" }),
    )
    .await;
    let applied = call(
        &base,
        &sid,
        "lmgw__container",
        json!({ "target": "chat", "action": "apply" }),
    )
    .await;
    let applied = payload(&applied);
    assert_eq!(applied["models_with_advisories"], 1, "{applied}");
    assert_eq!(applied["advisories"][0]["model_id"], "gemma4", "{applied}");
    assert!(
        applied["problems"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["model_id"] != "gemma4"),
        "{applied}"
    );
}

/// A pool-guarded row (`LlamaParams::pool_guarded`: unified KV explicitly on,
/// more than one slot, `n_predict` set — unified-KV design §12) reserves
/// against a per-image token bound before the gate admits an image request,
/// via `gate::count::image_token_bound`. A causal projector with no measured
/// ceiling and no `--image-max-tokens` gives that function nothing to bound,
/// so the row should carry an advisory saying image requests to it are
/// refused until the flag is set — the model still loads and serves text
/// fine, so this must never become a problem. The identical row *without*
/// the guard (kv_unified/n_predict both unset) stays quiet: an auto-slot row
/// is deliberately left unguarded, so there is nothing for the gate to
/// refuse.
#[tokio::test]
async fn a_pool_guarded_row_with_no_image_bound_gets_an_advisory() {
    let state = state_with_mode(SelfAdmin::Full).await;

    // A causal projector (qwen3vl_merger, as in the ubatch test above):
    // healthy under any batch size, but `image_token_bound` still refuses it
    // absent a configured `--image-max-tokens`, because a causal projector's
    // image cost is not fixed.
    let dir = tempfile::tempdir().unwrap();
    synth::chat("qwen35", 262144).write_to(&dir.path().join("qwen/w.gguf"));
    let mut qwen = synth::Header::default();
    qwen.str("general.architecture", "clip")
        .str("general.type", "mmproj")
        .str("clip.projector_type", "qwen3vl_merger")
        .u32("clip.vision.block_count", 27);
    qwen.write_to(&dir.path().join("qwen/mmproj.gguf"));
    let mut settings = store::load_settings(&state.db).await.unwrap();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();

    let base = serve_with(state.clone()).await;
    let sid = initialize(&base).await;
    let names_the_gate_advisory = |v: &Value| {
        v.as_array().unwrap().iter().any(|p| {
            let p = p.as_str().unwrap_or("");
            p.contains("pool-guarded") && p.contains("--image-max-tokens is set in the row's args")
        })
    };

    // 1. Guarded: kv_unified on, n_predict set — the advisory fires.
    let created = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "guarded", "gguf_path": "qwen/w.gguf",
                "mmproj_path": "qwen/mmproj.gguf", "kv_unified": true, "n_predict": 512,
                // The pool ledger needs a known size for an explicitly shared
                // pool (review finding 2) — this row has no `parallel`, so
                // it defaults to 4 auto slots (> 1) and needs one.
                "kv_unified_per_slot": 4096,
            }),
        )
        .await,
    );
    assert_eq!(created["ok"], true, "an advisory must not block the save");
    assert!(names_the_gate_advisory(&created["advisories"]), "{created}");

    // 2. The unguarded twin: same projector, same missing bound, no guard —
    //    nothing to report.
    let unguarded = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_set",
            json!({
                "action": "create", "model_id": "unguarded", "gguf_path": "qwen/w.gguf",
                "mmproj_path": "qwen/mmproj.gguf",
            }),
        )
        .await,
    );
    assert_eq!(unguarded["advisories"], json!([]), "{unguarded}");

    // 3. Giving the guarded row a bound clears the advisory, same shape as
    //    the ubatch advisory's own floor case.
    call(
        &base,
        &sid,
        "lmgw__local_model_set",
        json!({
            "action": "update", "model_id": "guarded",
            "extra_args": ["--image-max-tokens", "1280"],
        }),
    )
    .await;
    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__local_model_get",
            json!({ "model_id": "guarded" }),
        )
        .await,
    );
    assert!(!names_the_gate_advisory(&got["advisories"]), "{got}");
}
