//! A chat row on an embedding, rerank or decision GGUF: `local_model_check`
//! names the aux class and the kind its header says serve it
//! (decision-models design §3.2), so lev, OpenJev and Clef misfiled as chat
//! rows are not silently "fine". The finding is an advisory while the row
//! still loads and serves (the old `--embedding` workaround, a label-logit
//! decision type that still generates), and a problem when it cannot.

use lmgw_core::config::Settings;
use lmgw_core::gguf::synth;
use lmgw_core::ops;
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewLocalModel};
use serde_json::Value;

fn chat_row(model_id: &str, gguf: &str, args: &[&str]) -> NewLocalModel {
    NewLocalModel {
        model_id: model_id.into(),
        gguf_path: gguf.into(),
        params: Default::default(),
        args: args.iter().map(|a| a.to_string()).collect(),
        idle_seconds: 300,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

/// The row's `problems` and `advisories`, each as one string.
fn findings(out: &Value, id: &str) -> (String, String) {
    out["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model_id"] == id)
        .map(|m| (m["problems"].to_string(), m["advisories"].to_string()))
        .unwrap_or_default()
}

#[tokio::test]
async fn a_chat_row_on_an_aux_gguf_is_flagged_with_its_kind_by_whether_it_still_serves() {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.router.models_dir = dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();

    // lev: a causal label-logit decision model with an ordinary chat
    // template too, which is why the header's decision.type must decide.
    let mut lev = synth::chat("qwen35", 262_144);
    lev.str("qwen35.decision.type", "lev")
        .str("tokenizer.chat_template.systemone", "{{ state }}");
    lev.write_to(&dir.path().join("lev.gguf"));
    // Kev: the same, with a decision head, so its server runs in embedding
    // mode and cannot generate.
    let mut kev = synth::chat("qwen35", 262_144);
    kev.str("qwen35.decision.type", "kev")
        .str("tokenizer.chat_template.systemone", "{{ state }}")
        .tensor("cls.output.weight");
    kev.write_to(&dir.path().join("kev.gguf"));
    synth::embedding("bert", 2, 512).write_to(&dir.path().join("bge.gguf"));
    synth::chat("qwen3", 32_768).write_to(&dir.path().join("chat.gguf"));
    for (id, file, args) in [
        ("lev", "lev.gguf", &[][..]),
        ("lev-rerank", "lev.gguf", &["--reranking"][..]),
        ("kev", "kev.gguf", &[][..]),
        ("bge", "bge.gguf", &[][..]),
        ("bge-embedding", "bge.gguf", &["--embedding"][..]),
        ("chat", "chat.gguf", &[][..]),
    ] {
        store::insert_local_model(&state.db, &chat_row(id, file, args))
            .await
            .unwrap();
    }
    state.reload_snapshot().await.unwrap();

    let out = crate::common::check_wire(ops::local_model_check(&state, None, None).await.unwrap());
    let named = |s: &str, kind: &str| s.contains("the aux class serves it") && s.contains(kind);

    // Still serves: an advisory, not a problem.
    let (problems, advisories) = findings(&out, "lev");
    assert!(
        advisories.contains("gguf_path 'lev.gguf' declares decision.type 'lev'")
            && named(&advisories, "kind=decision"),
        "{out}"
    );
    assert!(
        !problems.contains("aux class"),
        "lev still generates: {out}"
    );
    let (problems, advisories) = findings(&out, "bge-embedding");
    assert!(named(&advisories, "kind=embed"), "{out}");
    assert!(
        !problems.contains("aux class"),
        "the --embedding workaround serves: {out}"
    );

    // Cannot serve: a problem.
    for (id, kind) in [
        ("lev-rerank", "kind=decision"),
        ("kev", "kind=decision"),
        ("bge", "kind=embed"),
    ] {
        let (problems, advisories) = findings(&out, id);
        assert!(named(&problems, kind), "{id}: {out}");
        assert!(!advisories.contains("aux class"), "{id}: {out}");
    }
    let (problems, _) = findings(&out, "bge");
    assert!(problems.contains("pooling_type 'cls'"), "{out}");

    let (problems, advisories) = findings(&out, "chat");
    assert!(
        !problems.contains("aux class") && !advisories.contains("aux class"),
        "a chat GGUF is not flagged: {out}"
    );
}
