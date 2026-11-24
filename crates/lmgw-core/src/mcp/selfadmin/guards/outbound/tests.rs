//! The outbound guard (security round B, L6, and the review of the round).
use std::collections::HashMap;

use serde_json::json;

use super::*;
use crate::config::{ModelAlias, Protocol, Upstream, UpstreamKind};

fn upstream(id: i64) -> Upstream {
    Upstream {
        id,
        name: format!("up{id}"),
        protocol: Protocol::OpenaiCompatible,
        kind: UpstreamKind::Generic,
        base_url: "http://192.0.2.1/v1".into(),
        api_key: None,
        extra_headers: vec![],
        timeout_ms: 1_000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

fn alias(id: i64, name: &str, up: i64, fallback: Option<&str>) -> (String, ModelAlias) {
    (
        name.to_string(),
        ModelAlias {
            id,
            alias: name.into(),
            upstream_id: up,
            upstream_model_id: format!("{name}-model"),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: fallback.map(str::to_string),
        },
    )
}

/// The page reader `example-vision` (on upstream 10) falls back to
/// `example-backup` (on 11); `example-other` (on 12) is nobody's.
fn snap() -> Snapshot {
    let mut s = Snapshot {
        upstreams: HashMap::from([(10, upstream(10)), (11, upstream(11)), (12, upstream(12))]),
        aliases: HashMap::from([
            alias(1, "example-vision", 10, Some("example-backup")),
            alias(2, "example-backup", 11, None),
            alias(3, "example-other", 12, None),
        ]),
        ..Snapshot::default()
    };
    s.settings.documents.page_reader = "example-vision".into();
    s
}

fn args(v: serde_json::Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

#[test]
fn the_chosen_model_its_fallback_and_their_upstreams_are_refused() {
    let s = snap();
    for (tool, a) in [
        (
            "lmgw__model_set",
            json!({"action": "update", "alias": "example-vision", "upstream": "up12"}),
        ),
        (
            "lmgw__model_set",
            json!({"action": "update", "alias": "EXAMPLE-BACKUP"}),
        ),
        (
            "lmgw__model_set",
            json!({"action": "update", "id": 2, "upstream_model": "x"}),
        ),
        (
            "lmgw__model_set",
            json!({"action": "update", "id": 3, "alias": "example-vision"}),
        ),
        (
            "lmgw__upstream_set",
            json!({"action": "update", "id": 10, "base_url": "http://198.51.100.1"}),
        ),
        (
            "lmgw__upstream_set",
            json!({"action": "update", "id": 11, "kind": "generic"}),
        ),
        (
            "lmgw__candidate_alias_set",
            json!({"action": "create", "alias": "example-backup"}),
        ),
    ] {
        let why = refusal(&s, tool, Some(&args(a.clone())));
        let why = why.unwrap_or_else(|| panic!("{tool} {a} passed"));
        assert!(
            why.contains("Nothing was changed") && why.contains("Settings › Documents"),
            "{why}"
        );
    }
    for (tool, a) in [
        (
            "lmgw__model_set",
            json!({"action": "update", "alias": "example-other"}),
        ),
        ("lmgw__model_set", json!({"action": "update", "id": 3})),
        ("lmgw__upstream_set", json!({"action": "update", "id": 12})),
        (
            "lmgw__candidate_alias_set",
            json!({"action": "preview", "alias": "example-vision"}),
        ),
        ("lmgw__settings_set", json!({"retention_days": 3})),
    ] {
        assert_eq!(
            refusal(&s, tool, Some(&args(a.clone()))),
            None,
            "{tool} {a}"
        );
    }
    // Nothing chosen: nothing is guarded.
    let mut none = snap();
    none.settings.documents.page_reader.clear();
    let a = args(json!({"action": "update", "id": 10}));
    assert_eq!(refusal(&none, "lmgw__upstream_set", Some(&a)), None);
}

fn local(id: i64, model_id: &str, fallback: Option<&str>) -> crate::config::LocalModel {
    crate::config::LocalModel {
        id,
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        params: crate::config::LlamaParams::default(),
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        fallback: fallback.map(String::from),
        capabilities_override: None,
        ladder: vec![],
    }
}

fn aux(id: i64, model_id: &str) -> crate::config::AuxModel {
    crate::config::AuxModel {
        id,
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
        kind: AuxKind::Embed,
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
        fallback: None,
    }
}

fn audio(id: i64, model_id: &str) -> crate::config::AudioModel {
    serde_json::from_value(json!({
        "id": id, "model_id": model_id, "family": "parakeet", "path": "p", "task": "asr",
        "mode": "offline", "backend": null, "load_options": {}, "session_options": {},
        "voice_presets": {}, "default_voice_preset": null, "enabled": true, "image": null,
        "extra_run_args": null, "warm_start": false,
    }))
    .unwrap()
}

/// The review of round B, S4, on each local class: a row picked by `id`
/// is checked as it is, so the rename is refused; and once a row was
/// renamed away (by the owner, say), a new row under the setting's name is
/// refused too. Each refusal names the setting and the model.
#[test]
fn a_rename_then_a_create_under_the_protected_name_is_refused() {
    // Local chat: the page reader `example-vl`, row 7.
    let mut s = snap();
    s.local_models = vec![local(7, "example-vl", None)];
    s.settings.documents.page_reader = s.local_public_name("example-vl");
    let a = args(json!({"action": "update", "id": 7, "model_id": "example-vl-old"}));
    let why = refusal(&s, "lmgw__local_model_set", Some(&a)).expect("rename passed");
    assert!(
        why.contains("the page-reading model 'example-vl'") && why.contains("dashboard"),
        "{why}"
    );
    s.local_models = vec![local(7, "example-vl-old", None)];
    let a = args(json!({
        "action": "create", "model_id": "example-vl", "gguf_path": "x.gguf",
        "fallback": "example-other",
    }));
    assert!(refusal(&s, "lmgw__local_model_set", Some(&a)).is_some());
    // Aux: the picture setting naming an aux row (the mechanism, not a
    // likely choice), row 8.
    let mut s = snap();
    s.aux_models = vec![aux(8, "example-embed")];
    s.settings.documents.picture_alias = s.aux_name_for(AuxKind::Embed, "example-embed");
    let a = args(json!({"action": "update", "id": 8, "model_id": "example-embed-old"}));
    let why = refusal(&s, "lmgw__aux_model_set", Some(&a)).expect("aux rename passed");
    assert!(why.contains("the picture-description model"), "{why}");
    s.aux_models = vec![aux(8, "example-embed-old")];
    let a = args(json!({"action": "create", "model_id": "example-embed", "gguf_path": "x"}));
    assert!(refusal(&s, "lmgw__aux_model_set", Some(&a)).is_some());
    // Audio: the speech-to-text row 9.
    let mut s = snap();
    s.audio_models = vec![audio(9, "example-asr")];
    s.settings.documents.asr_alias = s.audio_public_name("example-asr");
    let a = args(json!({"action": "update", "id": 9, "model_id": "example-asr-old"}));
    let why = refusal(&s, "lmgw__audio_model_set", Some(&a)).expect("audio rename passed");
    assert!(why.contains("the speech-to-text model"), "{why}");
    s.audio_models = vec![audio(9, "example-asr-old")];
    let a = args(json!({"action": "create", "model_id": "example-asr", "path": "x"}));
    assert!(refusal(&s, "lmgw__audio_model_set", Some(&a)).is_some());
    // Another row of the class, under another name, passes.
    let a = args(json!({"action": "update", "id": 9, "threads": 4}));
    assert_eq!(refusal(&s, "lmgw__audio_model_set", Some(&a)), None);
}

/// A candidate alias's candidates are local rows behind its name.
#[test]
fn a_candidate_alias_s_alternates_are_protected() {
    let mut s = snap();
    s.local_models = vec![local(1, "cand-a", None), local(2, "cand-b", None)];
    s.candidate_aliases.insert(
        "example-cand".into(),
        serde_json::from_value(json!({
            "id": 5, "alias": "example-cand", "candidates": ["cand-a", "cand-b"],
            "enabled": true,
        }))
        .unwrap(),
    );
    s.settings.documents.page_reader = "example-cand".into();
    let a = args(json!({"action": "update", "id": 2, "image": "registry.example/x"}));
    let why = refusal(&s, "lmgw__local_model_set", Some(&a)).expect("an alternate passed");
    assert!(why.contains("through its candidate 'cand-b'"), "{why}");
}

/// The upstream a protected model routes to: its key and timeout may
/// change and it may be disabled; anything that moves traffic is refused.
/// An upstream that would answer a protected name by its prefix is refused.
#[test]
fn an_upstream_keeps_its_key_timeout_and_disable_free_and_no_prefix_catches_a_name() {
    let s = snap();
    for a in [
        json!({"action": "update", "id": 10, "api_key": "sk-example"}),
        json!({"action": "update", "id": 10, "timeout_ms": 5000}),
        json!({"action": "disable", "id": 10}),
        json!({"action": "update", "id": 10, "enabled": false}),
    ] {
        assert_eq!(
            refusal(&s, "lmgw__upstream_set", Some(&args(a.clone()))),
            None,
            "{a}"
        );
    }
    for a in [
        json!({"action": "update", "id": 10, "base_url": "http://198.51.100.1"}),
        json!({"action": "update", "id": 10, "name": "renamed"}),
        json!({"action": "update", "id": 10, "api_key": "k", "expose_all": true}),
        json!({"action": "enable", "id": 10}),
        json!({"action": "delete", "id": 11}),
    ] {
        let why = refusal(&s, "lmgw__upstream_set", Some(&args(a.clone())))
            .unwrap_or_else(|| panic!("{a} passed"));
        assert!(
            why.contains("the page-reading model 'example-vision'")
                && why.contains("key and timeout_ms can still be changed"),
            "{why}"
        );
    }
    // A protected passthrough name: its upstream is not disabled here, and
    // no upstream may expose a prefix that catches it.
    let mut s = snap();
    let mut or = upstream(13);
    or.expose_all = true;
    or.expose_prefix = "or".into();
    s.upstreams.insert(13, or);
    s.settings.documents.picture_alias = "or/example-model".into();
    let a = args(json!({"action": "disable", "id": 13}));
    assert!(refusal(&s, "lmgw__upstream_set", Some(&a)).is_some());
    for a in [
        json!({"action": "create", "name": "x", "base_url": "http://198.51.100.2",
               "protocol": "openai_compatible", "expose_all": true, "expose_prefix": "or"}),
        json!({"action": "create", "name": "x", "base_url": "http://198.51.100.2",
               "protocol": "openai_compatible", "expose_all": true}),
        json!({"action": "update", "id": 12, "expose_all": true, "expose_prefix": "OR/"}),
    ] {
        let why = refusal(&s, "lmgw__upstream_set", Some(&args(a.clone())))
            .unwrap_or_else(|| panic!("{a} passed"));
        assert!(why.contains("'or/example-model'"), "{why}");
    }
    for a in [
        json!({"action": "create", "name": "x", "base_url": "http://198.51.100.2",
               "protocol": "openai_compatible", "expose_all": true, "expose_prefix": "zz"}),
        json!({"action": "update", "id": 12, "expose_prefix": "or"}),
        json!({"action": "update", "id": 12, "timeout_ms": 1}),
    ] {
        assert_eq!(
            refusal(&s, "lmgw__upstream_set", Some(&args(a.clone()))),
            None,
            "{a}"
        );
    }
}

/// A refusal says which setting, which model and which hop.
#[test]
fn a_refusal_names_the_setting_the_model_and_the_hop() {
    let s = snap();
    let a = args(json!({"action": "update", "id": 2, "upstream_model": "x"}));
    let why = refusal(&s, "lmgw__model_set", Some(&a)).unwrap();
    assert!(
        why.contains("the page-reading model 'example-vision', chosen on Settings › Documents")
            && why.contains("through its fallback 'example-backup'")
            && why.contains("The owner changes it in the dashboard. Nothing was changed"),
        "{why}"
    );
}
