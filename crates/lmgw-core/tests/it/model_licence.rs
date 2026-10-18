//! The licence a row's GGUF states, on the owner-side views (decision-models
//! design §3.9): the models list, the row's own read and `model_inspect`,
//! for chat and aux rows. A header without the keys, or a file that is not
//! there, leaves it null.

use lmgw_api_types as dto;
use lmgw_core::config::{AuxKind, HoldFallbackMode, LlamaParams, Settings};
use lmgw_core::gguf::synth;
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewAuxModel, NewLocalModel};

use crate::common;
use crate::models_full_wire::read;

fn chat_row(model_id: &str) -> NewLocalModel {
    NewLocalModel {
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        params: LlamaParams::default(),
        args: vec![],
        idle_seconds: 300,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: HoldFallbackMode::Inherit,
        hold_fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

fn aux_row(model_id: &str, kind: AuxKind) -> NewAuxModel {
    NewAuxModel {
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        kind,
        pooling: None,
        ctx_size: None,
        ubatch_size: None,
        batch_size: None,
        mmproj_path: None,
        backend: None,
        threads: None,
        args: vec![],
        idle_seconds: 120,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: HoldFallbackMode::Inherit,
        hold_fallback: None,
    }
}

#[tokio::test]
async fn rows_of_every_local_gguf_class_carry_the_header_licence() {
    let state = AppState::init_for_tests().await.unwrap();
    let chat_dir = tempfile::tempdir().unwrap();
    let aux_dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.router.models_dir = chat_dir.path().display().to_string();
    settings.aux_router.models_dir = aux_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();

    // Chat: id, name and link. A second chat file states nothing; a third row
    // points at a file that is not there.
    let mut named = synth::chat("llama", 4096);
    named
        .str("general.license", "other")
        .str("general.license.name", "Gemma Terms of Use")
        .str("general.license.link", "https://ai.google.dev/gemma/terms");
    named.write_to(&chat_dir.path().join("named.gguf"));
    synth::chat("llama", 4096).write_to(&chat_dir.path().join("silent.gguf"));
    // Aux: an id alone on an embedder and on a decision model.
    let mut emb = synth::embedding("bert", 1, 512);
    emb.str("general.license", "apache-2.0");
    emb.write_to(&aux_dir.path().join("emb.gguf"));
    let mut dec = synth::embedding("modern-bert", 1, 512);
    dec.str("general.license", "mit")
        .str("modern-bert.decision.type", "laya")
        .str("tokenizer.chat_template.systemone", "{{ state }}");
    dec.write_to(&aux_dir.path().join("dec.gguf"));

    for id in ["named", "silent", "missing"] {
        store::insert_local_model(&state.db, &chat_row(id))
            .await
            .unwrap();
    }
    store::insert_aux_model(&state.db, &aux_row("emb", AuxKind::Embed))
        .await
        .unwrap();
    store::insert_aux_model(&state.db, &aux_row("dec", AuxKind::Decision))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = common::serve(state.clone()).await;

    let full: dto::ModelsFull = common::round_trips(
        "models-full, licences",
        &read(&gw, "/api/models/full").await,
    );
    let chat = |id: &str| {
        full.local
            .iter()
            .find(|v| v.model.model_id == id)
            .unwrap()
            .licence
            .clone()
    };
    assert_eq!(
        chat("named"),
        Some(dto::ModelLicence {
            id: Some("other".into()),
            name: Some("Gemma Terms of Use".into()),
            link: Some("https://ai.google.dev/gemma/terms".into()),
        })
    );
    assert_eq!(chat("silent"), None);
    assert_eq!(chat("missing"), None, "an unreadable file says nothing");
    let aux = |id: &str| {
        full.aux
            .iter()
            .find(|v| v.model.model_id == id)
            .unwrap()
            .licence
            .as_ref()
            .and_then(|l| l.id.clone())
    };
    assert_eq!(aux("emb").as_deref(), Some("apache-2.0"));
    assert_eq!(aux("dec").as_deref(), Some("mit"));

    // The row's own read and the inspect tool agree with the list.
    let live = read(&gw, "/api/local-model?model_id=named").await;
    let dto::LocalModelRead::Chat(d) = common::round_trips("local-model, chat licence", &live)
    else {
        panic!("not a chat read: {live}");
    };
    assert_eq!(d.licence, chat("named"));
    let live = read(&gw, "/api/local-model?target=aux&model_id=dec").await;
    let dto::LocalModelRead::Aux(d) = common::round_trips("local-model, aux licence", &live) else {
        panic!("not an aux read: {live}");
    };
    assert_eq!(d.licence.and_then(|l| l.id).as_deref(), Some("mit"));
    let inspect = lmgw_core::modelinfo::model_inspect(&state, "named.gguf", None)
        .await
        .unwrap();
    assert_eq!(inspect.licence, chat("named"));
    let inspect = lmgw_core::modelinfo::model_inspect(&state, "silent.gguf", None)
        .await
        .unwrap();
    assert_eq!(inspect.licence, None);
}

#[test]
fn a_chip_says_the_name_else_the_id() {
    let l = |id: Option<&str>, name: Option<&str>| dto::ModelLicence {
        id: id.map(Into::into),
        name: name.map(Into::into),
        link: None,
    };
    assert_eq!(
        l(Some("mit"), Some("MIT License")).label(),
        Some("MIT License")
    );
    assert_eq!(l(Some("mit"), None).label(), Some("mit"));
    assert_eq!(l(None, None).label(), None);
}
