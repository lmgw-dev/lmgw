//! Decision models as the aux class's third kind (decision-models design
//! §3.2–§3.5): the header key that marks one ahead of the rerank and chat
//! rules, the aux row's five fields through the op and the store, the
//! decision row's refusals, and the projector in the VRAM footprint.
//!
//! The headers are synthetic, shaped after Appendix B. The real Laya file is
//! read by `modelinfo_real_files.rs` when `LMGW_TEST_AUX_DIR` names it.

use lmgw_core::config::{AuxBackend, AuxKind, AuxModel, RouterSettings, Settings};
use lmgw_core::gguf::{self, synth};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::vram::plan::PlanCache;
use lmgw_core::{modelinfo, ops, store};
use serde_json::{json, Value};

const LAYA: &str = "ggml-org/Laya-GGUF/Laya-Q8_0.gguf";
const KEV: &str = "ggml-org/Kev-4B-GGUF/Kev-4B-Q4_K_M.gguf";
const LEV: &str = "ggml-org/lev-GGUF/lev-Q4_K_M.gguf";
const KEV_MMPROJ: &str = "ggml-org/Kev-4B-GGUF/mmproj-Kev-4B-Q8_0.gguf";

/// Laya's shape: a non-causal `modern-bert` with the `cls.*` head the rerank
/// rule reads, `classifier.pooling_type` (not `pooling_type`), and the
/// decision keys.
fn laya(systemone: bool) -> synth::Header {
    let mut h = synth::Header::default();
    h.str("general.architecture", "modern-bert")
        .str("general.name", "synthetic Laya")
        .u32("modern-bert.context_length", 8192)
        .u32("modern-bert.block_count", 4)
        .u32("modern-bert.embedding_length", 64)
        .bool_flag("modern-bert.attention.causal", false)
        .u32("modern-bert.classifier.pooling_type", 1)
        .str("modern-bert.decision.type", "laya")
        .u32("modern-bert.decision.max_head_tokens", 192)
        .tensor("cls.output.weight")
        .tensor("cls.output.bias");
    if systemone {
        h.str("tokenizer.chat_template.systemone", "{{ state }}");
    }
    h
}

/// The causal `qwen35` shape (Kev-4B with `head`, lev without): an ordinary
/// chat template beside the systemone one, no `attention.causal` key.
fn causal(decision_type: &str, head: bool) -> synth::Header {
    let mut h = synth::chat("qwen35", 262_144);
    h.str("qwen35.decision.type", decision_type)
        .str("tokenizer.chat_template.systemone", "{{ state }}");
    if head {
        h.tensor("cls.output.weight");
    }
    h
}

fn projector() -> synth::Header {
    let mut h = synth::embedding("clip", 0, 0);
    h.str("general.type", "mmproj")
        .str("clip.projector_type", "qwen3vl_merger");
    h
}

struct Fixture {
    state: SharedState,
    aux_dir: tempfile::TempDir,
}

impl Fixture {
    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.aux_dir.path().join(rel)
    }

    fn write(&self, rel: &str, h: &synth::Header) {
        h.write_to(&self.path(rel));
    }

    async fn set(&self, patch: Value) -> Result<lmgw_api_types::AuxModelSaved, String> {
        ops::aux_model_set(&self.state, serde_json::from_value(patch).unwrap()).await
    }

    fn row(&self, model_id: &str) -> AuxModel {
        self.state
            .snapshot()
            .aux_models
            .iter()
            .find(|m| m.model_id == model_id)
            .cloned()
            .unwrap()
    }

    async fn detail(&self, model_id: &str) -> lmgw_api_types::AuxModelDetail {
        match ops::local_model_get(&self.state, None, Some(model_id), Some("aux"))
            .await
            .unwrap()
        {
            lmgw_api_types::LocalModelRead::Aux(d) => d,
            other => panic!("not an aux read: {other:?}"),
        }
    }
}

async fn fixture() -> Fixture {
    let state = AppState::init_for_tests().await.unwrap();
    let aux_dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.aux_router.models_dir = aux_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let f = Fixture { state, aux_dir };
    f.write(LAYA, &laya(true));
    f.write(KEV, &causal("kev", true));
    f.write(LEV, &causal("lev", false));
    f.write(KEV_MMPROJ, &projector());
    f
}

async fn kind_of(f: &Fixture, rel: &str) -> (String, Option<String>) {
    let i = modelinfo::model_inspect(&f.state, rel, Some("aux"))
        .await
        .unwrap();
    (i.serve_as, i.aux_kind)
}

/// §3.2: `{arch}.decision.type` is tested before the rerank and embed rules.
/// Laya and Kev-4B carry the `cls.output*` head that would make them
/// rerankers, and lev is a causal model with a chat template that would be a
/// chat row; each is a decision model. The same headers without the key are
/// what the later rules make of them.
#[tokio::test]
async fn a_decision_header_is_a_decision_model_before_the_rerank_and_chat_rules() {
    let f = fixture().await;

    let s = gguf::summarize(&f.path(LAYA)).unwrap();
    assert_eq!(s.decision_type.as_deref(), Some("laya"));
    assert_eq!(s.decision_max_head_tokens, Some(192));
    assert_eq!(s.attention_causal, Some(false));
    assert!(s.has_systemone_template);
    assert!(s.has_classifier_head, "the rerank rule's signal is there");
    assert_eq!(
        s.pooling_type, None,
        "classifier.pooling_type is another key"
    );

    let kev = gguf::summarize(&f.path(KEV)).unwrap();
    assert_eq!(kev.decision_type.as_deref(), Some("kev"));
    assert_eq!(kev.attention_causal, None, "absent on a causal file");
    assert!(kev.has_chat_template && kev.has_classifier_head);

    let decision = ("aux".to_string(), Some("decision".to_string()));
    assert_eq!(kind_of(&f, LAYA).await, decision);
    assert_eq!(kind_of(&f, KEV).await, decision);
    assert_eq!(kind_of(&f, LEV).await, decision);

    // Without the key, the same files fall to the rules after it.
    let mut bare_laya = synth::Header::default();
    bare_laya
        .str("general.architecture", "modern-bert")
        .u32("modern-bert.context_length", 8192)
        .tensor("cls.output.weight");
    f.write("bare/laya.gguf", &bare_laya);
    f.write("bare/lev.gguf", &synth::chat("qwen35", 262_144));
    assert_eq!(
        kind_of(&f, "bare/laya.gguf").await,
        ("aux".into(), Some("rerank".into()))
    );
    assert_eq!(kind_of(&f, "bare/lev.gguf").await, ("chat".into(), None));
}

/// The aux plan names the kind and its type, proposes no pooling, and says
/// when the server would refuse the file for want of its systemone template.
#[tokio::test]
async fn the_aux_plan_names_the_decision_kind_and_a_missing_systemone_template() {
    let f = fixture().await;
    let plan = modelinfo::local_model_plan(&f.state, LAYA, Some("aux"))
        .await
        .unwrap();
    assert_eq!(plan["params"]["kind"], "decision", "{plan}");
    assert!(plan["params"]["pooling"].is_null(), "{plan}");
    assert_eq!(plan["params"]["ctx_size"], 8192);
    assert!(
        plan["rationale"]["kind"]
            .as_str()
            .unwrap()
            .contains("'laya'"),
        "{plan}"
    );
    let warnings = plan["warnings"].to_string();
    assert!(!warnings.contains("systemone"), "{plan}");
    assert!(!warnings.contains("rerank"), "{plan}");

    f.write("no-template/laya.gguf", &laya(false));
    let plan = modelinfo::local_model_plan(&f.state, "no-template/laya.gguf", Some("aux"))
        .await
        .unwrap();
    assert!(
        plan["warnings"]
            .to_string()
            .contains("tokenizer.chat_template.systemone"),
        "{plan}"
    );

    // A sibling projector goes into the row's field, not its args.
    let plan = modelinfo::local_model_plan(&f.state, KEV, Some("aux"))
        .await
        .unwrap();
    assert_eq!(plan["params"]["mmproj_path"], KEV_MMPROJ, "{plan}");
    assert!(plan["params"]["extra_args"].is_null(), "{plan}");
}

/// The five fields (§3.3) round-trip through `aux_model_set`, the table and
/// `local_model_get`, render as their flags, and clear back to unset.
#[tokio::test]
async fn the_five_fields_round_trip_through_the_op_and_the_store() {
    let f = fixture().await;
    let saved = f
        .set(json!({
            "action": "create", "model_id": "kev", "gguf_path": KEV, "kind": "decision",
            "ctx_size": 262144, "ubatch_size": 2048, "batch_size": 2048,
            "mmproj_path": format!("/models/{KEV_MMPROJ}"), "backend": "cpu", "threads": 12,
        }))
        .await
        .unwrap();
    assert!(saved.message.contains("/v1/systemone"), "{}", saved.message);
    let warnings = saved.warnings.unwrap_or_default().join("\n");
    assert!(!warnings.contains("projector"), "{warnings}");

    let row = f.row("kev");
    assert_eq!(row.kind, AuxKind::Decision);
    assert_eq!((row.ubatch_size, row.batch_size), (Some(2048), Some(2048)));
    assert_eq!(
        row.mmproj_path.as_deref(),
        Some(KEV_MMPROJ),
        "stored relative"
    );
    assert_eq!(
        (row.backend, row.threads),
        (Some(AuxBackend::Cpu), Some(12))
    );
    let stored = store::get_aux_model(&f.state.db, row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            stored.ubatch_size,
            stored.batch_size,
            stored.backend,
            stored.threads
        ),
        (Some(2048), Some(2048), Some(AuxBackend::Cpu), Some(12))
    );

    let d = f.detail("kev").await;
    assert_eq!(
        (d.kind.as_str(), d.endpoint.as_str()),
        ("decision", "/v1/systemone")
    );
    assert_eq!((d.ubatch_size, d.batch_size), (Some(2048), Some(2048)));
    assert_eq!(d.mmproj_path.as_deref(), Some(KEV_MMPROJ));
    assert_eq!((d.backend.as_deref(), d.threads), (Some("cpu"), Some(12)));
    for flag in [
        "--ubatch-size 2048",
        "--batch-size 2048",
        &format!("--mmproj /models/{KEV_MMPROJ}"),
        "--n-gpu-layers 0",
        "--threads 12",
    ] {
        assert!(d.command_line.contains(flag), "{flag}: {}", d.command_line);
    }
    for flag in ["--embeddings", "--reranking", "--pooling"] {
        assert!(!d.command_line.contains(flag), "{flag}: {}", d.command_line);
    }

    // A sparse update keeps what it does not name; `clear` and blanks unset.
    f.set(json!({"action": "update", "model_id": "kev", "threads": 4}))
        .await
        .unwrap();
    assert_eq!(
        (f.row("kev").threads, f.row("kev").ubatch_size),
        (Some(4), Some(2048))
    );
    f.set(json!({
        "action": "update", "model_id": "kev", "backend": "", "mmproj_path": "",
        "clear": "threads,ubatch_size",
    }))
    .await
    .unwrap();
    let row = f.row("kev");
    assert_eq!(
        (
            row.backend,
            &row.mmproj_path,
            row.threads,
            row.ubatch_size,
            row.batch_size
        ),
        (None, &None, None, None, Some(2048))
    );
}

/// What a row cannot carry is refused by name, before anything is stored.
#[tokio::test]
async fn the_op_refuses_what_an_aux_row_cannot_carry() {
    let f = fixture().await;
    let create = |extra: Value| {
        let mut p = json!({"action": "create", "model_id": "laya", "gguf_path": LAYA,
                           "kind": "decision"});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        p
    };
    for (extra, says) in [
        (json!({"backend": "cuda"}), "only 'cpu'"),
        (json!({"threads": 0}), "threads: 0"),
        (json!({"ubatch_size": -512}), "ubatch_size"),
        (json!({"batch_size": 0}), "batch_size"),
        (json!({"mmproj_path": "../elsewhere.gguf"}), "mmproj_path"),
        (json!({"pooling": "cls"}), "no pooling"),
        (json!({"extra_args": ["--reranking"]}), "--reranking"),
        (json!({"extra_args": ["--embeddings"]}), "--embeddings"),
        (json!({"extra_args": ["--pooling=cls"]}), "--pooling=cls"),
        (json!({"kind": "classify"}), "embed|rerank|decision"),
    ] {
        let e = f.set(create(extra.clone())).await.unwrap_err();
        assert!(e.contains(says), "{extra}: {e}");
    }
    assert!(f.state.snapshot().aux_models.is_empty());
}

/// The static findings read the header and the fields: a decision file
/// served as another kind, a projector on a non-causal decision model, a
/// ubatch above the batch llama.cpp lowers it to, a CPU row whose own run
/// args still pass the GPU, and a flag in the args that a set field renders.
#[tokio::test]
async fn the_row_findings_read_the_decision_header_and_the_fields() {
    let f = fixture().await;
    f.set(json!({"action": "create", "model_id": "laya-rr", "gguf_path": LAYA, "kind": "rerank"}))
        .await
        .unwrap();
    let problems = f.detail("laya-rr").await.problems.join("\n");
    assert!(problems.contains("set kind=decision"), "{problems}");

    let saved = f
        .set(json!({
            "action": "create", "model_id": "laya", "gguf_path": LAYA, "kind": "decision",
            "mmproj_path": KEV_MMPROJ, "ubatch_size": 8192, "backend": "cpu",
            "extra_run_args": ["--device", "nvidia.com/gpu=all"],
            "extra_args": ["--threads", "4"], "threads": 16,
        }))
        .await
        .unwrap();
    let warnings = saved.warnings.unwrap_or_default().join("\n");
    assert!(warnings.contains("non-causal decision model"), "{warnings}");
    assert!(
        warnings.contains("set batch_size to at least 8192"),
        "{warnings}"
    );
    assert!(warnings.contains("still pass the GPU"), "{warnings}");
    assert!(
        warnings.contains("`--threads` is rendered from the row's threads field"),
        "{warnings}"
    );
    let cmd = f.detail("laya").await.command_line;
    assert!(
        cmd.contains("--threads 16") && !cmd.contains("--threads 4"),
        "{cmd}"
    );

    // A CPU row with no threads field and a count in its args: that count
    // renders (no default replaces it), and the finding says so. The
    // ubatch is checked against the batch the args still render.
    let saved = f
        .set(json!({
            "action": "create", "model_id": "kev-cpu", "gguf_path": KEV, "kind": "decision",
            "backend": "cpu", "ubatch_size": 4096,
            "extra_args": ["--threads", "8", "-b", "1024"],
        }))
        .await
        .unwrap();
    let warnings = saved.warnings.unwrap_or_default().join("\n");
    assert!(
        warnings.contains("`--threads` in the extra args still renders"),
        "{warnings}"
    );
    assert!(
        warnings.contains("the ubatch (4096) exceeds the batch (1024, from the extra args)"),
        "{warnings}"
    );
    let cmd = f.detail("kev-cpu").await.command_line;
    assert!(cmd.ends_with("--threads 8 -b 1024"), "{cmd}");
    assert_eq!(cmd.matches("--threads").count(), 1, "{cmd}");
}

/// The footprint charges a projector beside the weights — from the field,
/// and from a `--mmproj` still riding in the args — and a CPU row nothing.
#[tokio::test]
async fn the_footprint_charges_the_projector_and_a_cpu_row_nothing() {
    let f = fixture().await;
    let class = RouterSettings {
        models_dir: f.aux_dir.path().display().to_string(),
        ..RouterSettings::default_aux()
    };
    let size = |rel: &str| std::fs::metadata(f.path(rel)).unwrap().len();
    let (weights, mmproj) = (size(KEV), size(KEV_MMPROJ));
    let row = |mmproj_path: Option<&str>, args: &[&str], backend: Option<AuxBackend>| AuxModel {
        id: 1,
        model_id: "kev".into(),
        gguf_path: KEV.into(),
        kind: AuxKind::Decision,
        pooling: None,
        ctx_size: Some(4096),
        ubatch_size: None,
        batch_size: None,
        mmproj_path: mmproj_path.map(String::from),
        backend,
        threads: None,
        args: args.iter().map(|s| s.to_string()).collect(),
        idle_seconds: 0,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    };
    let plans = PlanCache::default();

    let bare = plans.aux(&class, &row(None, &[], None)).await;
    assert_eq!(bare.weights_bytes, weights);
    let field = plans.aux(&class, &row(Some(KEV_MMPROJ), &[], None)).await;
    assert_eq!(field.weights_bytes, weights + mmproj, "{field:?}");
    let in_args = format!("/models/{KEV_MMPROJ}");
    let args = plans
        .aux(&class, &row(None, &["--mmproj", &in_args], None))
        .await;
    assert_eq!(args.weights_bytes, weights + mmproj, "{args:?}");

    let cpu = plans
        .aux(&class, &row(Some(KEV_MMPROJ), &[], Some(AuxBackend::Cpu)))
        .await;
    assert_eq!(cpu.total_bytes, 0, "{cpu:?}");
}

/// `ctx_size` in `clear` takes llama-server's default, as the five row
/// fields do, so the editor's blank context field can unset a saved one; an
/// update that names no context leaves it as it was.
#[tokio::test]
async fn ctx_size_in_clear_unsets_a_saved_context() {
    let f = fixture().await;
    f.set(
        json!({"action": "create", "model_id": "laya", "gguf_path": LAYA,
                 "kind": "decision", "ctx_size": 8192}),
    )
    .await
    .unwrap();
    f.set(json!({"action": "update", "model_id": "laya", "idle_seconds": 60}))
        .await
        .unwrap();
    assert_eq!(f.row("laya").ctx_size, Some(8192), "absent leaves it");
    f.set(
        json!({"action": "update", "model_id": "laya", "ctx_size": null,
                 "clear": "ctx_size,extra_run_args"}),
    )
    .await
    .unwrap();
    assert_eq!(f.row("laya").ctx_size, None);
    assert!(!f.detail("laya").await.command_line.contains("--ctx-size"));
    f.set(json!({"action": "update", "model_id": "laya", "ctx_size": 4096}))
        .await
        .unwrap();
    assert_eq!(f.row("laya").ctx_size, Some(4096));
}
