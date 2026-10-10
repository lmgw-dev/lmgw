//! `local_model_plan` against real GGUFs on disk.
//!
//! The unit tests in `modelinfo` cover the classification rules with synthetic
//! summaries. This covers the part that only real files exercise: finding the
//! companions sitting next to the weights, and turning three files in one
//! directory into a configuration that would actually load.
//!
//! Gated on the files being present, so it skips cleanly in CI and on any box
//! that has not downloaded them. `LMGW_TEST_MODELS_DIR` points at the chat
//! models directory and `LMGW_TEST_AUX_DIR` at the embedding models directory.

use lmgw_core::config::Settings;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::{modelinfo, store};

/// A repo directory shipping weights + a vision projector + a dflash drafter —
/// the combination that was impossible to configure through the tool plane.
const REPO: &str = "unsloth/Muse-Glimmer-30B-GGUF";
const WEIGHTS: &str = "unsloth/Muse-Glimmer-30B-GGUF/Muse-Glimmer-30B-UD-Q4_K_XL.gguf";
/// Directory named by the env var `var`, if set and non-empty.
fn dir_from_env(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.is_empty())
}

fn models_dir() -> Option<String> {
    dir_from_env("LMGW_TEST_MODELS_DIR")
}

fn aux_dir() -> Option<String> {
    dir_from_env("LMGW_TEST_AUX_DIR")
}

/// Whether `file` exists under the models directory (false when unset).
fn have_model(file: &str) -> bool {
    models_dir().is_some_and(|d| std::path::Path::new(&d).join(file).is_file())
}

/// Whether `file` exists under the aux directory (false when unset).
fn have_aux(file: &str) -> bool {
    aux_dir().is_some_and(|d| std::path::Path::new(&d).join(file).is_file())
}

async fn state_pointed_at_real_models() -> Option<SharedState> {
    let Some(models_dir) = models_dir().filter(|_| have_model(WEIGHTS)) else {
        eprintln!("skipping: set LMGW_TEST_MODELS_DIR to a directory containing {WEIGHTS}");
        return None;
    };
    let state = AppState::init_for_tests().await.ok()?;
    let mut settings = Settings::default();
    settings.router.models_dir = models_dir;
    // Deliberately an image nothing can pull: the probe runs a throwaway
    // container (§3.6) and must degrade to "not checked" rather than fail the
    // whole call. (`init_for_tests` installs a runtime that refuses every
    // podman verb anyway — this makes the intent explicit at the call site.)
    settings.router.image = "localhost/lmgw-test-no-such-image:none".into();
    store::save_settings(&state.db, &settings).await.ok()?;
    state.reload_snapshot().await.ok()?;
    Some(state)
}

#[tokio::test]
async fn planning_a_multimodal_model_finds_its_projector_and_drafter() {
    let Some(state) = state_pointed_at_real_models().await else {
        return;
    };
    let plan = modelinfo::local_model_plan(&state, WEIGHTS, false, None)
        .await
        .unwrap();
    let p = &plan["params"];

    assert_eq!(plan["model_id"], "muse-glimmer-30b");

    // Read out of the GGUF header, not guessed: the model's real trained
    // context. A plan that quietly proposed something smaller would be the
    // exact hidden cap this surface is supposed to stop producing.
    assert_eq!(p["ctx_size"], 131072);
    assert_eq!(p["jinja"], true);

    // The two companions, discovered by inspecting every GGUF in the same
    // directory rather than by matching filenames.
    assert_eq!(p["mmproj_path"], format!("{REPO}/mmproj-kquant.gguf"));
    assert_eq!(p["draft_gguf_path"], format!("{REPO}/dflash-kquant.gguf"));

    // The value a published model card got wrong. The drafter's architecture
    // is `dflash`, so nothing else can be correct here.
    assert_eq!(p["spec_type"], "draft-dflash");
    assert_eq!(p["spec_draft_n_max"], 16);

    // Every proposed value carries its reason — a caller overriding one needs
    // to know which came from the file and which are defaults.
    for field in ["ctx_size", "spec_type", "mmproj_path", "draft_gguf_path"] {
        assert!(
            plan["rationale"][field]
                .as_str()
                .is_some_and(|r| !r.is_empty()),
            "no rationale for {field}: {plan}"
        );
    }

    // No container to ask, so the runtime verdict must be absent rather than
    // invented.
    assert_eq!(plan["runtime"]["checked"], false);
}

/// The live misconfiguration this check exists for: four rows in the author's
/// own gateway set `spec_type = draft-mtp` against GGUFs with no MTP layers,
/// and every one fails to load with `context type MTP requested but model
/// doesn't contain MTP layers`.
#[tokio::test]
async fn a_model_without_mtp_layers_is_not_given_mtp_speculation() {
    let Some(state) = state_pointed_at_real_models().await else {
        return;
    };
    const QWEN: &str = "unsloth/Qwen3.5-9B-GGUF/Qwen3.5-9B-UD-Q4_K_XL.gguf";
    if !have_model(QWEN) {
        return;
    }
    let plan = modelinfo::local_model_plan(&state, QWEN, false, None)
        .await
        .unwrap();
    assert!(
        plan["params"]["spec_type"].is_null(),
        "must not propose speculation for a model that cannot do it: {plan}"
    );
    assert!(plan["params"]["draft_gguf_path"].is_null());
}

/// Planning a GGUF nothing is configured against is an *addition*, and must
/// still read that way — an empty comparison, not a silent one.
#[tokio::test]
async fn planning_an_unconfigured_gguf_proposes_creating_it() {
    let Some(state) = state_pointed_at_real_models().await else {
        return;
    };
    let plan = modelinfo::local_model_plan(&state, WEIGHTS, false, None)
        .await
        .unwrap();
    assert_eq!(plan["configured_as"].as_array().unwrap().len(), 0);
    assert!(
        plan["next_step"]
            .as_str()
            .unwrap()
            .contains("action=create"),
        "{plan}"
    );
}

/// The repair case. A row already serving the GGUF turns the plan into a diff:
/// the caller gets the two fields that are wrong and a patch it can send back
/// verbatim, instead of a full parameter set to eyeball against
/// `local_model_get`.
#[tokio::test]
async fn planning_a_configured_gguf_diffs_against_the_row() {
    let Some(state) = state_pointed_at_real_models().await else {
        return;
    };
    const QWEN: &str = "unsloth/Qwen3.5-9B-GGUF/Qwen3.5-9B-UD-Q4_K_XL.gguf";
    if !have_model(QWEN) {
        return;
    }

    // Exactly the live misconfiguration: MTP speculation against weights with
    // no MTP heads, a context short of the trained ceiling, and the sibling
    // projector left unconfigured.
    let patch = serde_json::from_value(serde_json::json!({
        "action": "create", "model_id": "broken", "gguf_path": QWEN,
        "spec_type": "draft-mtp", "spec_draft_n_max": 4,
        "ctx_size": 64000, "temp": 0.6, "seed": 3407,
    }))
    .unwrap();
    lmgw_core::ops::local_model_set(&state, patch)
        .await
        .unwrap();

    let plan = modelinfo::local_model_plan(&state, QWEN, false, None)
        .await
        .unwrap();
    let d = &plan["configured_as"][0];
    assert_eq!(d["model_id"], "broken");
    assert_eq!(d["in_sync"], false);

    // The unloadable speculation is proposed for removal, with the drafter
    // window that is meaningless without it.
    let cleared = d["apply"]["clear"].as_str().unwrap();
    assert!(cleared.contains("spec_type"), "{d}");
    assert!(cleared.contains("spec_draft_n_max"), "{d}");
    assert_eq!(d["apply"]["ctx_size"], 262144);
    assert_eq!(
        d["apply"]["mmproj_path"],
        "unsloth/Qwen3.5-9B-GGUF/mmproj-BF16.gguf"
    );

    // Sampling is a preference the plan never forms one about. Proposing to
    // revert it would make the diff untrustworthy for exactly the deliberate
    // tuning it must leave alone.
    assert!(d["apply"]["temp"].is_null(), "{d}");
    assert!(d["apply"]["seed"].is_null(), "{d}");
    assert!(
        !cleared.contains("temp") && !cleared.contains("seed"),
        "{d}"
    );

    // The patch is an argument set for local_model_set, not a report about
    // one — sending it back unedited must be the whole repair.
    assert_eq!(d["apply"]["action"], "update");
    assert_eq!(d["apply"]["model_id"], "broken");
    let repair = serde_json::from_value(d["apply"].clone()).unwrap();
    lmgw_core::ops::local_model_set(&state, repair)
        .await
        .unwrap();

    // And having applied it, the same plan now reports nothing left to do.
    let plan = modelinfo::local_model_plan(&state, QWEN, false, None)
        .await
        .unwrap();
    let d = &plan["configured_as"][0];
    assert_eq!(d["in_sync"], true, "{plan}");
    assert!(d["apply"].is_null(), "an in-sync row needs no patch: {d}");
    assert!(
        plan["next_step"]
            .as_str()
            .unwrap()
            .contains("nothing to do"),
        "{plan}"
    );
}

/// Two rows can deliberately share one GGUF — a GPU model and its CPU-only
/// twin. Both have to be reported, or the caller repairs one and silently
/// leaves the other broken.
#[tokio::test]
async fn every_row_sharing_a_gguf_is_diffed() {
    let Some(state) = state_pointed_at_real_models().await else {
        return;
    };
    for id in ["gpu", "cpu"] {
        let patch = serde_json::from_value(serde_json::json!({
            "action": "create", "model_id": id, "gguf_path": WEIGHTS,
            "spec_type": "draft-mtp",
        }))
        .unwrap();
        lmgw_core::ops::local_model_set(&state, patch)
            .await
            .unwrap();
    }
    let plan = modelinfo::local_model_plan(&state, WEIGHTS, false, None)
        .await
        .unwrap();
    let rows = plan["configured_as"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{plan}");

    // This repo ships a real dflash drafter, so the wrong spec_type is a value
    // to correct — not configuration to strip. Clearing it would throw away
    // speculation the files fully support.
    for r in rows {
        assert_eq!(r["apply"]["spec_type"], "draft-dflash", "{r}");
        assert!(r["apply"]["clear"].is_null(), "{r}");
    }
}

/// The live crash of 2026-09-24: Gemma 4 12B with its projector and no
/// ubatch_size, so llama.cpp's 512 was in force and a full-page image aborted
/// the server on `non-causal attention requires n_ubatch >= n_tokens`. Its
/// projector is `gemma4uv`, which llama.cpp always decodes non-causally. The
/// plan has to propose the fix, and the static checks have to name the
/// problem on a row that lacks it.
#[tokio::test]
async fn a_projector_plan_carries_the_ubatch_its_images_need() {
    let Some(state) = state_pointed_at_real_models().await else {
        return;
    };
    const GEMMA: &str = "unsloth/gemma-4-12B-it-qat-GGUF/gemma-4-12B-it-qat-UD-Q4_K_XL.gguf";
    const MMPROJ: &str = "unsloth/gemma-4-12B-it-qat-GGUF/mmproj-BF16.gguf";
    if !have_model(GEMMA) {
        return;
    }
    let plan = modelinfo::local_model_plan(&state, GEMMA, false, None)
        .await
        .unwrap();
    assert_eq!(plan["params"]["mmproj_path"], MMPROJ, "{plan}");
    assert_eq!(plan["params"]["ubatch_size"], 1280, "{plan}");
    // llama.cpp's default batch (2048) already covers it; the plan does not
    // pin a value it has no reason for.
    assert!(plan["params"]["batch_size"].is_null(), "{plan}");
    let why = plan["rationale"]["ubatch_size"].as_str().unwrap();
    assert!(why.contains("gemma4uv"), "{why}");
    assert!(why.contains("non-causal attention requires"), "{why}");
    // The type is read from `clip.vision.projector_type`, the only key this
    // two-modality file has.
    assert!(
        plan["rationale"]["mmproj_path"]
            .as_str()
            .unwrap()
            .contains("gemma4uv"),
        "{plan}"
    );

    // The crashing configuration, as it was live.
    let patch = serde_json::from_value(serde_json::json!({
        "action": "create", "model_id": "gemma4-12b", "gguf_path": GEMMA,
        "mmproj_path": MMPROJ, "ctx_size": 131072,
    }))
    .unwrap();
    let created = lmgw_core::ops::local_model_set(&state, patch)
        .await
        .unwrap();
    // Saved all the same, with the advisory attached: it does not block.
    assert_eq!(created["ok"], true, "{created}");
    assert!(
        created["advisories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("n_ubatch >= n_tokens")),
        "{created}"
    );
    let checked = crate::common::check_wire(
        lmgw_core::ops::local_model_check(&state, Some("gemma4-12b"), None)
            .await
            .unwrap(),
    );
    let row = &checked["models"][0];
    assert!(
        row["advisories"].as_array().unwrap().iter().any(|p| {
            let p = p.as_str().unwrap_or("");
            p.contains("(gemma4uv projector)")
                && p.contains("GGML_ASSERT")
                && p.contains("Set ubatch_size to at least 1280")
        }),
        "{checked}"
    );
    // The model starts, so it is not broken.
    assert_eq!(row["ok"], true, "{checked}");

    // The plan's repair patch, sent back unedited, is the whole fix.
    let plan = modelinfo::local_model_plan(&state, GEMMA, false, None)
        .await
        .unwrap();
    let d = &plan["configured_as"][0];
    assert_eq!(d["apply"]["ubatch_size"], 1280, "{d}");
    let repair = serde_json::from_value(d["apply"].clone()).unwrap();
    lmgw_core::ops::local_model_set(&state, repair)
        .await
        .unwrap();
    let checked = crate::common::check_wire(
        lmgw_core::ops::local_model_check(&state, Some("gemma4-12b"), None)
            .await
            .unwrap(),
    );
    assert_eq!(checked["broken"], 0, "{checked}");
    assert_eq!(checked["with_advisories"], 0, "{checked}");
}

/// Projectors llama.cpp decodes causally split an image across physical
/// batches, so their plans carry no ubatch floor and their rows no warning.
/// Qwen3.5 (`qwen3vl_merger`) is the live case with batch_size 1024; Gemma 4
/// E4B's `gemma4v` is causal because its text model is 2560 wide.
#[tokio::test]
async fn a_causal_projector_gets_no_ubatch_floor_and_no_warning() {
    let Some(state) = state_pointed_at_real_models().await else {
        return;
    };
    for (weights, id) in [
        (
            "unsloth/Qwen3.5-9B-GGUF/Qwen3.5-9B-UD-Q4_K_XL.gguf",
            "qwen3.5-9b",
        ),
        (
            "unsloth/gemma-4-E4B-it-qat-GGUF/gemma-4-E4B-it-qat-UD-Q4_K_XL.gguf",
            "gemma4-e4b",
        ),
    ] {
        if !have_model(weights) {
            continue;
        }
        let plan = modelinfo::local_model_plan(&state, weights, false, None)
            .await
            .unwrap();
        assert!(plan["params"]["mmproj_path"].is_string(), "{plan}");
        assert!(plan["params"]["ubatch_size"].is_null(), "{plan}");

        let mmproj = plan["params"]["mmproj_path"].clone();
        let patch = serde_json::from_value(serde_json::json!({
            "action": "create", "model_id": id, "gguf_path": weights,
            "mmproj_path": mmproj, "batch_size": 1024,
        }))
        .unwrap();
        lmgw_core::ops::local_model_set(&state, patch)
            .await
            .unwrap();
        let checked = crate::common::check_wire(
            lmgw_core::ops::local_model_check(&state, Some(id), None)
                .await
                .unwrap(),
        );
        assert_eq!(checked["broken"], 0, "{checked}");
        assert_eq!(checked["with_advisories"], 0, "{checked}");
    }
}

/// A projector is not weights. Planning against one should say so instead of
/// producing a config that tries to load it as a model.
#[tokio::test]
async fn planning_against_a_projector_warns_rather_than_pretending() {
    let Some(state) = state_pointed_at_real_models().await else {
        return;
    };
    let plan =
        modelinfo::local_model_plan(&state, &format!("{REPO}/mmproj-kquant.gguf"), false, None)
            .await
            .unwrap();
    let warnings = plan["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("mmproj")),
        "expected a role warning, got {warnings:?}"
    );
}

/// `model_inspect` names what each of the three files is, which is the thing a
/// caller cannot tell from the filenames.
#[tokio::test]
async fn inspect_classifies_each_file_in_the_repo() {
    let Some(state) = state_pointed_at_real_models().await else {
        return;
    };
    for (file, role) in [
        ("Muse-Glimmer-30B-UD-Q4_K_XL.gguf", "weights"),
        ("mmproj-kquant.gguf", "mmproj"),
        ("dflash-kquant.gguf", "drafter"),
    ] {
        let got = crate::common::inspect_wire(
            modelinfo::model_inspect(&state, &format!("{REPO}/{file}"), false, None)
                .await
                .unwrap(),
        );
        assert_eq!(got["role"], role, "{file} misclassified: {got}");
    }

    // KV cost is reported for the real context. Muse-Glimmer is 3:1 sliding
    // window, so the honest figure is far below layers x context x heads —
    // which is precisely why reporting it beats making the caller guess.
    let w = crate::common::inspect_wire(
        modelinfo::model_inspect(&state, WEIGHTS, false, None)
            .await
            .unwrap(),
    );
    let kv = w["vram_estimate"]["kv_cache_bytes_at_full_ctx_q8_0"]
        .as_u64()
        .expect("kv estimate");
    assert!(
        (100_000_000..4_000_000_000).contains(&kv),
        "implausible KV estimate at 128k ctx: {kv}"
    );
}

/// Path handling is reachable from a remote MCP client.
#[tokio::test]
async fn traversal_and_missing_paths_are_refused() {
    let Some(state) = state_pointed_at_real_models().await else {
        return;
    };
    for bad in ["../../etc/passwd", "unsloth/../../etc/passwd"] {
        let err = modelinfo::model_inspect(&state, bad, false, None)
            .await
            .unwrap_err();
        assert!(err.contains("plain path"), "{bad} gave: {err}");
    }
    let err = modelinfo::model_inspect(&state, "nope/absent.gguf", false, None)
        .await
        .unwrap_err();
    assert!(err.contains("lmgw__gguf_files"), "unhelpful error: {err}");

    // The in-container spelling is what a rendered preset shows, so accept it.
    let ok = modelinfo::model_inspect(&state, &format!("/models/{WEIGHTS}"), false, None).await;
    assert!(
        ok.is_ok(),
        "the /models/ prefix should be tolerated: {ok:?}"
    );
}

/// The whole stack over the real wire: JSON-RPC `tools/call` on `/mcp/admin`
/// → `selfadmin` dispatch → `modelinfo` → GGUF headers on disk.
///
/// The other tests here call the ops functions directly, and the tests in
/// `mcp_selfadmin.rs` drive the wire against a temp directory with no real
/// GGUFs. Neither would catch a dispatch arm wired to the wrong argument, so
/// this one goes end to end.
#[tokio::test]
async fn the_add_a_model_path_works_over_the_mcp_wire() {
    use lmgw_core::config::SelfAdmin;
    use lmgw_core::server::build_router;
    use serde_json::json;

    let Some(state) = state_pointed_at_real_models().await else {
        return;
    };
    // The plane needs an owner credential; give it one and turn self-admin on.
    let mut settings = state.snapshot().settings.clone();
    settings.self_admin = SelfAdmin::Full;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    lmgw_core::agents::token::set_owner_key(
        &state,
        lmgw_core::agents::token::OWNER_SELF_ADMIN,
        "t",
        true,
    )
    .await
    .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = build_router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let http = reqwest::Client::new();
    let rpc = |method: &'static str, params: serde_json::Value, sid: Option<String>| {
        let (http, base) = (http.clone(), base.clone());
        async move {
            let mut req = http
                .post(format!("{base}/mcp/admin"))
                .header("accept", "application/json, text/event-stream")
                .header("authorization", "Bearer t")
                .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
            if let Some(s) = sid {
                req = req.header("mcp-session-id", s);
            }
            let resp = req.send().await.unwrap();
            let sid = resp
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let body: serde_json::Value =
                serde_json::from_str(&resp.text().await.unwrap()).unwrap();
            (sid, body)
        }
    };

    let (sid, _) = rpc(
        "initialize",
        json!({ "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": { "name": "e2e", "version": "0" } }),
        None,
    )
    .await;
    let sid = sid.expect("session id");

    // Unwrap a tool result's JSON payload out of its text content block.
    let payload = |body: &serde_json::Value| -> serde_json::Value {
        let r = &body["result"];
        assert!(
            r["isError"] != serde_json::Value::Bool(true),
            "tool errored: {}",
            r["content"][0]["text"]
        );
        serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap()
    };

    // 1. Discover what is on disk.
    let (_, body) = rpc(
        "tools/call",
        json!({ "name": "lmgw__gguf_files", "arguments": { "search": "muse-glimmer" } }),
        Some(sid.clone()),
    )
    .await;
    let files = payload(&body);
    assert!(
        files["count"].as_u64().unwrap() >= 3,
        "expected the repo's three GGUFs: {files}"
    );

    // 2. Plan from the weights file.
    let (_, body) = rpc(
        "tools/call",
        json!({ "name": "lmgw__local_model_plan", "arguments": { "gguf_path": WEIGHTS } }),
        Some(sid.clone()),
    )
    .await;
    let plan = payload(&body);
    assert_eq!(plan["params"]["spec_type"], "draft-dflash");

    // 3. Create from exactly what the planner returned, unmodified — the
    //    contract the planner's `next_step` promises. Anything it emits that
    //    local_model_set does not accept fails here.
    let mut args = plan["params"].as_object().unwrap().clone();
    args.insert("action".into(), json!("create"));
    args.insert("model_id".into(), plan["model_id"].clone());
    let (_, body) = rpc(
        "tools/call",
        json!({ "name": "lmgw__local_model_set", "arguments": args }),
        Some(sid.clone()),
    )
    .await;
    let created = payload(&body);
    assert_eq!(created["ok"], true, "create rejected the plan: {created}");

    // 4. Read it back and confirm the command line carries the projector and
    //    drafter.
    let (_, body) = rpc(
        "tools/call",
        json!({ "name": "lmgw__local_model_get",
                "arguments": { "model_id": plan["model_id"].clone() } }),
        Some(sid.clone()),
    )
    .await;
    let got = payload(&body);
    let cmd = got["command_line"].as_str().unwrap();
    assert!(cmd.contains("--mmproj /models/"), "{cmd}");
    assert!(cmd.contains("--spec-type draft-dflash"), "{cmd}");
    assert!(cmd.contains("--ctx-size 131072"), "{cmd}");
    assert_eq!(got["gguf_present"], true);
    assert_eq!(got["mmproj_present"], true);
    assert_eq!(got["draft_present"], true);
    assert_eq!(got["source"], "manual", "not downloaded in this test");
}

// ---------------------------------------------------------------------------
// The aux class against the real embedding files, when this box has them
// ---------------------------------------------------------------------------

const QWEN_EMBED: &str = "Qwen/Qwen3-Embedding-0.6B-GGUF/Qwen3-Embedding-0.6B-Q8_0.gguf";
const GEMMA_EMBED: &str = "ggml-org/embeddinggemma-300M-GGUF/embeddinggemma-300M-Q8_0.gguf";

async fn state_pointed_at_real_embedders() -> Option<SharedState> {
    let (Some(models_dir), Some(aux_dir)) =
        (models_dir(), aux_dir().filter(|_| have_aux(QWEN_EMBED)))
    else {
        eprintln!(
            "skipping: set LMGW_TEST_MODELS_DIR and LMGW_TEST_AUX_DIR (containing {QWEN_EMBED})"
        );
        return None;
    };
    let state = AppState::init_for_tests().await.ok()?;
    let mut settings = Settings::default();
    settings.router.models_dir = models_dir;
    settings.aux_router.models_dir = aux_dir;
    settings.aux_router.image = "localhost/lmgw-test-no-such-image:none".into();
    store::save_settings(&state.db, &settings).await.ok()?;
    state.reload_snapshot().await.ok()?;
    Some(state)
}

/// The two embedders that started this: `Qwen3-Embedding` is architecture
/// `qwen3` like the chat model and is told apart only by its header's
/// `pooling_type` (last); EmbeddingGemma declares mean pooling. Both plan as
/// aux, with the pooling the converter recorded — never guessed from a name.
#[tokio::test]
async fn real_embedders_plan_as_aux_with_their_declared_pooling() {
    let Some(state) = state_pointed_at_real_embedders().await else {
        return;
    };
    let plan = modelinfo::local_model_plan(&state, QWEN_EMBED, false, Some("aux"))
        .await
        .unwrap();
    assert_eq!(plan["class"], "aux", "{plan}");
    assert_eq!(plan["apply_ready"], true);
    assert_eq!(plan["params"]["kind"], "embed");
    assert_eq!(plan["params"]["pooling"], "last");
    assert_eq!(plan["params"]["ctx_size"], 32768);

    if have_aux(GEMMA_EMBED) {
        let plan = modelinfo::local_model_plan(&state, GEMMA_EMBED, false, Some("aux"))
            .await
            .unwrap();
        assert_eq!(plan["class"], "aux", "{plan}");
        assert_eq!(plan["params"]["pooling"], "mean");
        assert_eq!(plan["params"]["ctx_size"], 2048);
    }

    let inspect = crate::common::inspect_wire(
        modelinfo::model_inspect(&state, QWEN_EMBED, false, Some("aux"))
            .await
            .unwrap(),
    );
    assert_eq!(inspect["serve_as"], "aux");
    assert_eq!(inspect["aux_kind"], "embed");
    assert_eq!(inspect["pooling_type"], "last");

    // And from the chat dir's point of view the file is simply not there —
    // the honest answer, naming the target that would find it.
    let err = modelinfo::model_inspect(&state, QWEN_EMBED, false, None)
        .await
        .unwrap_err();
    assert!(err.contains("target=aux"), "{err}");
}
