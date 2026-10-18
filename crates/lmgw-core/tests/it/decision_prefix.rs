//! A decision row's public name and its fallback (decision-models design
//! §3.10, §2.4): one name per row under the kind's prefix (`decide/<id>`
//! for decision rows, `embed/<id>` for the others), the candidate-alias
//! collisions on it, the setting that moves it, and the save-time check that
//! a decision row's fallback can answer System One.
//!
//! Rows are created through `aux_model_set`, so its checks run. The cloud
//! upstream is a wiremock catalog: `jev` outputs decisions, `gpt` text, and
//! `plain` states nothing.

use lmgw_core::config::{AuxKind, HoldFallbackMode, Protocol, Settings, UpstreamKind};
use lmgw_core::gguf::synth;
use lmgw_core::ops::{self, CandidateAliasPatch};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewCandidateAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::serve;

struct Fixture {
    state: SharedState,
    _aux_dir: tempfile::TempDir,
    _cloud: MockServer,
}

impl Fixture {
    async fn set(&self, patch: Value) -> Result<lmgw_api_types::AuxModelSaved, String> {
        ops::aux_model_set(&self.state, serde_json::from_value(patch).unwrap()).await
    }

    async fn settings(&self, edit: impl FnOnce(&mut Settings)) {
        let mut s = self.state.snapshot().settings.clone();
        edit(&mut s);
        store::save_settings(&self.state.db, &s).await.unwrap();
        self.state.reload_snapshot().await.unwrap();
    }

    /// The upstream model `name` resolves to, or `None` when it does not.
    fn resolves(&self, name: &str) -> Option<String> {
        self.state
            .snapshot()
            .resolve(name)
            .ok()
            .map(|r| r.upstream_model)
    }
}

/// Laya's shape (Appendix B): non-causal, the `cls.*` head, the decision keys.
fn laya() -> synth::Header {
    let mut h = synth::Header::default();
    h.str("general.architecture", "modern-bert")
        .u32("modern-bert.context_length", 8192)
        .bool_flag("modern-bert.attention.causal", false)
        .str("modern-bert.decision.type", "laya")
        .str("tokenizer.chat_template.systemone", "{{ state }}")
        .tensor("cls.output.weight");
    h
}

async fn fixture() -> Fixture {
    let state = AppState::init_for_tests().await.unwrap();
    let aux_dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.aux_router.models_dir = aux_dir.path().display().to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    laya().write_to(&aux_dir.path().join("laya.gguf"));
    synth::embedding("bert", 1, 512).write_to(&aux_dir.path().join("bge.gguf"));

    let cloud = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": [
                {"id": "jev", "architecture": {"output_modalities": ["decisions"]}},
                {"id": "gpt", "architecture": {"output_modalities": ["text"]}},
                {"id": "plain"},
            ]})),
        )
        .mount(&cloud)
        .await;
    let upstream_id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "cloud".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", cloud.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    for (alias, model) in [("decider", "jev"), ("chatter", "gpt"), ("silent", "plain")] {
        store::insert_alias(
            &state.db,
            &NewAlias {
                alias: alias.into(),
                upstream_id,
                upstream_model_id: model.into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: None,
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    Fixture {
        state,
        _aux_dir: aux_dir,
        _cloud: cloud,
    }
}

async fn with_rows() -> Fixture {
    let f = fixture().await;
    let saved = f
        .set(
            json!({"action": "create", "model_id": "laya", "gguf_path": "laya.gguf",
                     "kind": "decision"}),
        )
        .await
        .unwrap();
    assert_eq!(saved.public_name.as_deref(), Some("decide/laya"));
    assert!(saved.message.contains("'decide/laya'"), "{}", saved.message);
    f.set(
        json!({"action": "create", "model_id": "bge", "gguf_path": "bge.gguf",
                 "kind": "embed"}),
    )
    .await
    .unwrap();
    f
}

/// §3.10: a decision row answers to `decide/<id>` and nothing else, an
/// embedder keeps `embed/<id>`, and every surface that names a row says so.
#[tokio::test]
async fn a_decision_row_answers_to_decide_and_embed_does_not_reach_it() {
    let f = with_rows().await;
    assert_eq!(f.resolves("decide/laya").as_deref(), Some("laya"));
    assert_eq!(f.resolves("embed/laya"), None, "one public name per row");
    assert_eq!(f.resolves("embed/bge").as_deref(), Some("bge"));
    assert_eq!(f.resolves("decide/bge"), None);

    let snap = f.state.snapshot();
    let name = |id: &str| {
        let row = snap.aux_models.iter().find(|m| m.model_id == id).unwrap();
        snap.aux_public_name(row)
    };
    assert_eq!(
        (name("laya"), name("bge")),
        ("decide/laya".into(), "embed/bge".into())
    );
    assert_eq!(snap.aux_name_for(AuxKind::Decision, "kev"), "decide/kev");

    let detail = match ops::local_model_get(&f.state, None, Some("laya"), Some("aux"))
        .await
        .unwrap()
    {
        lmgw_api_types::LocalModelRead::Aux(d) => d,
        other => panic!("not an aux read: {other:?}"),
    };
    assert_eq!(detail.public_name, "decide/laya");

    // `/v1/models` (acceptance step 8): the name, the task, the route and
    // the output word.
    let gw = serve(f.state.clone()).await;
    let list: Value = gw
        .client()
        .get(format!("{gw}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = list["data"].as_array().unwrap();
    let laya = entries
        .iter()
        .find(|m| m["id"] == "decide/laya")
        .unwrap_or_else(|| panic!("decide/laya not listed: {list}"));
    assert!(!entries.iter().any(|m| m["id"] == "embed/laya"), "{list}");
    let caps = &laya["capabilities"];
    assert_eq!(caps["task"], "decision", "{laya}");
    assert_eq!(caps["endpoints"], json!(["/v1/systemone"]));
    assert_eq!(caps["output_modalities"], json!(["decisions"]));
}

/// The setting moves the name (§3.10): another prefix, none (bare ids), or
/// the embedders' own, which puts both kinds in one namespace. The settings
/// read and the dashboard's patch carry it.
#[tokio::test]
async fn the_decision_prefix_is_a_setting_of_the_aux_class() {
    let f = with_rows().await;
    let read = ops::settings(&f.state).await.unwrap();
    assert_eq!(read.aux_router.decision_prefix, "decide");
    assert_eq!(read.aux_router.base.public_prefix, "embed");

    f.settings(|s| s.aux_router.decision_prefix = "ask".into())
        .await;
    assert_eq!(f.resolves("ask/laya").as_deref(), Some("laya"));
    assert_eq!(f.resolves("decide/laya"), None);

    f.settings(|s| s.aux_router.decision_prefix.clear()).await;
    assert_eq!(f.resolves("laya").as_deref(), Some("laya"), "bare ids");

    f.settings(|s| s.aux_router.decision_prefix = "embed".into())
        .await;
    assert_eq!(f.resolves("embed/laya").as_deref(), Some("laya"));
    assert_eq!(f.resolves("embed/bge").as_deref(), Some("bge"));

    // The dashboard's patch, and the full read it is edited from.
    let gw = serve(f.state.clone()).await;
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/settings_set_full"))
        .json(&json!({"aux_router": {"decision_prefix": "/decide/"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    // The aux patch is its own type, and still refuses a key it does not
    // know rather than dropping it.
    for unknown in [
        json!({"aux_router": {"decision_prefixes": "ask"}}),
        json!({"aux_router": {"auto_start": true}}),
    ] {
        let resp = gw
            .client()
            .post(format!("{gw}/api/op/settings_set_full"))
            .json(&unknown)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{unknown} was accepted");
    }
    assert_eq!(
        f.state.snapshot().settings.aux_router.decision_prefix,
        "decide"
    );
    let full: Value = gw
        .client()
        .get(format!("{gw}/api/settings-full"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(full["aux_router"]["decision_prefix"], "decide", "{full}");
    assert_eq!(full["aux_router"]["public_prefix"], "embed", "flat: {full}");
    assert!(full["router"].get("decision_prefix").is_none(), "{full}");
}

/// §3.10: the collision checks cover `decide/<id>`. A candidate alias cannot
/// take a decision row's name, and an aux row cannot take a candidate
/// alias's, also when a kind change moves it there.
#[tokio::test]
async fn a_candidate_alias_named_decide_id_collides() {
    let f = with_rows().await;
    let candidate = |alias: &str| CandidateAliasPatch {
        action: "create".into(),
        alias: Some(alias.into()),
        candidates: Some("no-such-chat-row".into()),
        ..Default::default()
    };
    let taken = ops::candidate_alias_set(&f.state, candidate("decide/laya"))
        .await
        .err()
        .unwrap();
    assert!(
        taken.contains("already the name of an aux model"),
        "{taken}"
    );
    let free = ops::candidate_alias_set(&f.state, candidate("embed/laya"))
        .await
        .err()
        .unwrap();
    assert!(!free.contains("already the name"), "{free}");

    store::insert_candidate_alias(
        &f.state.db,
        &NewCandidateAlias {
            alias: "decide/kev".into(),
            candidates: vec!["chat".into()],
            background: false,
            fallback_mode: HoldFallbackMode::Inherit,
            fallback: None,
            capabilities_disabled: vec![],
            capabilities_enabled: vec![],
            enabled: true,
            notes: String::new(),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    let kev = json!({"action": "create", "model_id": "kev", "gguf_path": "laya.gguf",
                     "kind": "decision"});
    let refused = f.set(kev).await.err().unwrap();
    assert!(
        refused.contains("'decide/kev' is already a candidate alias"),
        "{refused}"
    );
    f.set(
        json!({"action": "create", "model_id": "kev", "gguf_path": "bge.gguf",
                 "kind": "embed"}),
    )
    .await
    .expect("embed/kev is free");
    let refused = f
        .set(json!({"action": "update", "model_id": "kev", "kind": "decision"}))
        .await
        .err()
        .unwrap();
    assert!(refused.contains("'decide/kev'"), "{refused}");
}

/// §2.4: a decision row's fallback must answer System One. One whose catalog
/// states output modalities without `decisions` is refused when it is set;
/// one stating `decisions`, or nothing, is accepted. Only decision rows are
/// held to it, and turning a row into one checks the fallback it keeps.
#[tokio::test]
async fn a_decision_rows_fallback_must_answer_system_one_when_it_is_set() {
    let f = fixture().await;
    let decision = |id: &str, fallback: &str| {
        json!({"action": "create", "model_id": id, "gguf_path": "laya.gguf",
               "kind": "decision", "hold_fallback_mode": "alias", "hold_fallback": fallback})
    };
    let refused = f.set(decision("laya", "chatter")).await.err().unwrap();
    assert!(
        refused.contains("hold_fallback") && refused.contains("outputs text"),
        "{refused}"
    );
    assert!(f.state.snapshot().aux_models.is_empty(), "nothing saved");
    f.set(decision("laya", "decider"))
        .await
        .expect("outputs decisions");
    f.set(decision("laya-2", "silent"))
        .await
        .expect("a catalog that states nothing passes");

    let refused = f
        .set(json!({"action": "update", "model_id": "laya", "hold_fallback": "chatter"}))
        .await
        .err()
        .unwrap();
    assert!(refused.contains("outputs text"), "{refused}");

    f.set(
        json!({"action": "create", "model_id": "bge", "gguf_path": "bge.gguf",
                 "kind": "embed", "hold_fallback_mode": "alias", "hold_fallback": "chatter"}),
    )
    .await
    .expect("an embedder's fallback is not held to System One");
    let refused = f
        .set(json!({"action": "update", "model_id": "bge", "kind": "decision"}))
        .await
        .err()
        .unwrap();
    assert!(refused.contains("outputs text"), "{refused}");

    // A save that changes neither the kind nor the fallback is not checked
    // again (no catalog fetch for an enable): a row stored before its
    // fallback's catalog changed still disables and enables, and the gate
    // checks the fallback on every request anyway.
    store::insert_aux_model(
        &f.state.db,
        &store::NewAuxModel {
            model_id: "laya-old".into(),
            gguf_path: "laya.gguf".into(),
            kind: AuxKind::Decision,
            pooling: None,
            ctx_size: None,
            ubatch_size: None,
            batch_size: None,
            mmproj_path: None,
            backend: None,
            threads: None,
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: HoldFallbackMode::Alias,
            hold_fallback: Some("chatter".into()),
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
    for action in ["disable", "enable"] {
        f.set(json!({"action": action, "model_id": "laya-old"}))
            .await
            .unwrap_or_else(|e| panic!("{action}: {e}"));
    }
    let refused = f
        .set(json!({"action": "update", "model_id": "laya-old", "hold_fallback": "silent"}))
        .await;
    assert!(
        refused.is_ok(),
        "a new fallback is checked, and passes: {refused:?}"
    );
}
