//! What the five model saves answer over HTTP (`POST /api/op/<name>`), for
//! every action: each answer reads back through the type the document
//! describes, and keeps the keys its action has always sent: which are there,
//! which are absent, which are `null`, which are whole numbers.

use std::sync::Arc;

use lmgw_api_types as dto;
use lmgw_core::erased::Erased;
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewLocalModel};
use serde_json::{json, Value};

use crate::common::{self, serve, Gw};

const LLAMA_HELP: &str = include_str!("../fixtures/llama/llama-server-help-4b1a27fa0.txt");
const SD_HELP: &str = include_str!("../fixtures/sdcpp/sd-server-help-c678dfe.txt");

/// Answers the `--help` runs a save validates flags against; nothing starts.
struct HelpOnly;

impl CommandRunner for HelpOnly {
    fn run<'a>(
        &'a self,
        _program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        Box::pin(async move {
            let help = args.iter().any(|a| a == "--help");
            let sd = args.iter().any(|a| a == "/sd-server");
            Ok(CmdOutput {
                status: 0,
                stdout: match (help, sd) {
                    (true, true) => SD_HELP.into(),
                    (true, false) => LLAMA_HELP.into(),
                    _ => String::new(),
                },
                stderr: String::new(),
            })
        })
    }
}

/// A header-only GGUF, enough for a local chat row to be read.
fn write_gguf(path: &std::path::Path) {
    fn put_str(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }
    let mut kv = Vec::new();
    put_str(&mut kv, "general.architecture");
    kv.extend_from_slice(&8u32.to_le_bytes());
    put_str(&mut kv, "qwen3");
    let mut out = Vec::new();
    out.extend_from_slice(b"GGUF");
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&1u64.to_le_bytes());
    out.extend_from_slice(&kv);
    std::fs::write(path, out).unwrap();
}

/// A gateway whose four model directories hold one file each, and whose
/// container runtime only answers `--help`.
async fn gateway() -> (SharedState, Gw, tempfile::TempDir) {
    let state = AppState::init_for_tests().await.unwrap();
    state.set_runtime_for_tests(Arc::new(Registry::new(
        Arc::new(HelpOnly),
        reqwest::Client::new(),
    )));
    let dir = tempfile::tempdir().unwrap();
    for f in ["chat.gguf", "embed.gguf", "diffusion.gguf"] {
        std::fs::write(dir.path().join(f), b"x").unwrap();
    }
    write_gguf(&dir.path().join("m.gguf"));
    std::fs::create_dir_all(dir.path().join("tts")).unwrap();
    let mut s = state.snapshot().settings.clone();
    let d = dir.path().display().to_string();
    s.router.models_dir = d.clone();
    s.aux_router.models_dir = d.clone();
    s.image.models_dir = d.clone();
    s.audio.models_dir = d;
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw, dir)
}

/// `POST /api/op/{name}`, which has to answer 200.
async fn op(gw: &Gw, name: &str, args: Value) -> Value {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "{name} {args}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{name} is not JSON ({e}): {body}"))
}

/// The keys of `live`, to be exactly `want` (in any order).
fn assert_keys(live: &Value, want: &[&str]) {
    let mut got: Vec<&str> = live
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {live}"))
        .keys()
        .map(String::as_str)
        .collect();
    got.sort_unstable();
    let mut want = want.to_vec();
    want.sort_unstable();
    assert_eq!(got, want, "{live}");
}

#[tokio::test]
async fn a_local_model_save_answers_the_keys_of_its_action() {
    let (_state, gw, _dir) = gateway().await;
    let what = "POST /api/op/local_model_set";

    // `warnings` and `advisories` are lists even when empty.
    let live = op(
        &gw,
        "local_model_set",
        json!({"action": "create", "model_id": "chat", "gguf_path": "chat.gguf",
               "extra_args": ["--not-a-flag", "1"]}),
    )
    .await;
    let created = common::round_trips::<dto::LocalModelSaved>(what, &live);
    assert_keys(&live, &["ok", "id", "warnings", "advisories", "message"]);
    assert!(live["id"].is_i64(), "{live}");
    assert_eq!(live["advisories"], json!([]));
    assert!(
        live["warnings"][0]
            .as_str()
            .unwrap()
            .contains("--not-a-flag"),
        "{live}"
    );
    let id = created.id;

    // Only an update says whether the previous container is still up.
    for action in ["update", "disable", "enable"] {
        let live = op(&gw, "local_model_set", json!({"action": action, "id": id})).await;
        let typed = common::round_trips::<dto::LocalModelSaved>(what, &live);
        assert_keys(
            &live,
            &[
                "ok",
                "id",
                "warnings",
                "advisories",
                "container_kept_previous_config",
                "message",
            ],
        );
        assert_eq!(
            typed.container_kept_previous_config,
            Some(false),
            "{action}"
        );
    }

    let live = op(
        &gw,
        "local_model_set",
        json!({"action": "duplicate", "id": id}),
    )
    .await;
    let copy = common::round_trips::<dto::LocalModelSaved>(what, &live);
    assert_keys(&live, &["ok", "id", "message"]);
    assert_ne!(copy.id, id, "the copy has an id of its own");

    let live = op(
        &gw,
        "local_model_set",
        json!({"action": "delete", "id": copy.id}),
    )
    .await;
    let deleted = common::round_trips::<dto::LocalModelSaved>(what, &live);
    assert_keys(&live, &["ok", "id", "message"]);
    assert_eq!(deleted.warnings, None);
}

#[tokio::test]
async fn an_aux_model_save_answers_the_keys_of_its_action() {
    let (_state, gw, _dir) = gateway().await;
    let what = "POST /api/op/aux_model_set";

    let live = op(
        &gw,
        "aux_model_set",
        json!({"action": "create", "model_id": "emb", "gguf_path": "embed.gguf",
               "kind": "embed"}),
    )
    .await;
    let created = common::round_trips::<dto::AuxModelSaved>(what, &live);
    assert_keys(&live, &["ok", "id", "warnings", "public_name", "message"]);
    assert_eq!(created.public_name.as_deref(), Some("embed/emb"));

    for action in ["update", "disable", "enable"] {
        let live = op(
            &gw,
            "aux_model_set",
            json!({"action": action, "id": created.id}),
        )
        .await;
        let typed = common::round_trips::<dto::AuxModelSaved>(what, &live);
        assert_keys(&live, &["ok", "id", "warnings", "message"]);
        assert_eq!(typed.public_name, None, "{action}");
    }

    let live = op(
        &gw,
        "aux_model_set",
        json!({"action": "delete", "id": created.id}),
    )
    .await;
    common::round_trips::<dto::AuxModelSaved>(what, &live);
    assert_keys(&live, &["ok", "id", "message"]);
}

#[tokio::test]
async fn an_audio_model_save_answers_the_keys_of_its_action() {
    let (state, gw, _dir) = gateway().await;
    let what = "POST /api/op/audio_model_set";

    let live = op(
        &gw,
        "audio_model_set",
        json!({"action": "create", "model_id": "tts", "family": "pocket_tts",
               "path": "tts", "task": "tts"}),
    )
    .await;
    let created = common::round_trips::<dto::AudioModelSaved>(what, &live);
    assert_keys(&live, &["ok", "id", "message"]);
    let id = created.id;

    // enable and disable say so, by the row's name.
    for (action, verb) in [("disable", "disabled"), ("enable", "enabled")] {
        let live = op(&gw, "audio_model_set", json!({"action": action, "id": id})).await;
        let typed = common::round_trips::<dto::AudioModelSaved>(what, &live);
        assert!(
            typed
                .message
                .starts_with(&format!("audio model 'tts' {verb}")),
            "{}",
            typed.message
        );
    }

    // Whether a learned residency was reset is an update's to say.
    let live = op(
        &gw,
        "audio_model_set",
        json!({"action": "update", "id": id}),
    )
    .await;
    let typed = common::round_trips::<dto::AudioModelSaved>(what, &live);
    assert_keys(&live, &["ok", "id", "residency_reset", "message"]);
    assert_eq!(typed.residency_reset, Some(false));

    store::set_audio_model_residency(&state.db, id, Some((1 << 30, "key")))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let live = op(
        &gw,
        "audio_model_set",
        json!({"action": "update", "id": id, "clear": "residency"}),
    )
    .await;
    let typed = common::round_trips::<dto::AudioModelSaved>(what, &live);
    assert_eq!(typed.residency_reset, Some(true));

    // A delete names the row like the other classes'.
    let live = op(
        &gw,
        "audio_model_set",
        json!({"action": "delete", "id": id}),
    )
    .await;
    let typed = common::round_trips::<dto::AudioModelSaved>(what, &live);
    assert_keys(&live, &["ok", "id", "message"]);
    assert_eq!(typed.id, id);
    assert!(
        typed.message.starts_with("audio model 'tts' deleted"),
        "{}",
        typed.message
    );
}

#[tokio::test]
async fn an_image_model_save_answers_the_keys_of_its_action() {
    let (state, gw, _dir) = gateway().await;
    let what = "POST /api/op/image_model_set";

    let live = op(
        &gw,
        "image_model_set",
        json!({"action": "create", "model_id": "img",
               "files": {"diffusion_model": "diffusion.gguf"}}),
    )
    .await;
    let created = common::round_trips::<dto::ImageModelSaved>(what, &live);
    assert_keys(
        &live,
        &[
            "ok",
            "id",
            "warnings",
            "public_name",
            "command_line",
            "message",
        ],
    );
    assert_eq!(created.public_name.as_deref(), Some("image/img"));
    assert!(
        created
            .command_line
            .as_deref()
            .unwrap()
            .starts_with("podman run"),
        "{live}"
    );

    // A learned peak survives a change that is not the pipeline, and is
    // dropped by one that is.
    store::set_image_model_peak(&state.db, created.id, Some(3 << 30))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let live = op(
        &gw,
        "image_model_set",
        json!({"action": "update", "id": created.id, "idle_seconds": 60}),
    )
    .await;
    let typed = common::round_trips::<dto::ImageModelSaved>(what, &live);
    assert_keys(
        &live,
        &[
            "ok",
            "id",
            "warnings",
            "peak_reset",
            "command_line",
            "message",
        ],
    );
    assert_eq!(typed.peak_reset, Some(false));
    let live = op(
        &gw,
        "image_model_set",
        json!({"action": "update", "id": created.id, "args": {"offload_to_cpu": true}}),
    )
    .await;
    let typed = common::round_trips::<dto::ImageModelSaved>(what, &live);
    assert_eq!(typed.peak_reset, Some(true));
    assert!(typed.command_line.unwrap().contains("--offload-to-cpu"));

    for action in ["disable", "enable"] {
        let live = op(
            &gw,
            "image_model_set",
            json!({"action": action, "id": created.id}),
        )
        .await;
        common::round_trips::<dto::ImageModelSaved>(what, &live);
    }
    let live = op(
        &gw,
        "image_model_set",
        json!({"action": "delete", "id": created.id}),
    )
    .await;
    common::round_trips::<dto::ImageModelSaved>(what, &live);
    assert_keys(&live, &["ok", "id", "message"]);
}

/// A local chat row whose facets and limits are pinned.
fn local_row(id: &str, vision: bool, ctx: Option<i64>, n_predict: Option<i64>) -> NewLocalModel {
    let mut m = NewLocalModel {
        model_id: id.into(),
        gguf_path: "m.gguf".into(),
        params: Default::default(),
        args: vec![],
        idle_seconds: 300,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: Some(json!({
            "capabilities": {
                "vision": vision,
                "input_modalities": if vision { json!(["text", "image"]) } else { json!(["text"]) },
                "tool_calls": {"kind": "native"},
                "reasoning": {"kind": "fixed", "enabled": false},
                "structured_output": {"json_schema": true, "json_object": true},
            }
        })),
        ladder: vec![],
    };
    m.params.ctx_size = ctx;
    m.params.n_predict = n_predict;
    m
}

/// A saved alias carries the derivation of the row read back; `context_length`
/// and `max_output_tokens` are numbers or `null`, and absent only where the
/// whole derivation is (a delete).
#[tokio::test]
async fn a_candidate_alias_save_answers_the_keys_of_its_action() {
    let (state, gw, _dir) = gateway().await;
    let what = "POST /api/op/candidate_alias_set";
    for row in [
        local_row("m1", true, Some(8192), Some(1024)),
        local_row("m2", false, Some(4096), None),
    ] {
        store::insert_local_model(&state.db, &row).await.unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let derived = [
        "ok",
        "id",
        "enabled_facets",
        "common_facets",
        "routable_candidates",
        "problems",
        "advisories",
        "fallback_usable",
        "context_length",
        "max_output_tokens",
        "message",
    ];

    // One limit known, the other not: a number and a `null` that is there.
    let live = op(
        &gw,
        "candidate_alias_set",
        json!({"action": "create", "alias": "smart", "candidates": "m1,m2"}),
    )
    .await;
    let both = common::round_trips::<dto::CandidateAliasSaved>(what, &live);
    assert_keys(&live, &derived);
    assert!(live["context_length"].is_u64(), "{live}");
    assert_eq!(live["context_length"], json!(4096));
    assert!(live["max_output_tokens"].is_null(), "{live}");
    assert_eq!(both.context_length, Some(Some(4096)));
    assert_eq!(both.max_output_tokens, Some(None));
    assert_eq!(both.routable_candidates.unwrap(), ["m1", "m2"]);

    let live = op(
        &gw,
        "candidate_alias_set",
        json!({"action": "create", "alias": "solo", "candidates": "m1"}),
    )
    .await;
    let solo = common::round_trips::<dto::CandidateAliasSaved>(what, &live);
    assert_eq!(
        (solo.context_length, solo.max_output_tokens),
        (Some(Some(8192)), Some(Some(1024)))
    );

    for action in ["update", "disable", "enable"] {
        let live = op(
            &gw,
            "candidate_alias_set",
            json!({"action": action, "id": solo.id}),
        )
        .await;
        common::round_trips::<dto::CandidateAliasSaved>(what, &live);
        assert_keys(&live, &derived);
    }

    // A delete has no row to derive from.
    let live = op(
        &gw,
        "candidate_alias_set",
        json!({"action": "delete", "id": solo.id}),
    )
    .await;
    let deleted = common::round_trips::<dto::CandidateAliasSaved>(what, &live);
    assert_keys(&live, &["ok", "id", "message"]);
    assert_eq!(deleted.enabled_facets, None);
    assert_eq!(deleted.context_length, None, "absent, not null");
}

/// The preview sends every key every time: `error` and the unknown limits as
/// `null`, an `addable` that may be empty, and `unsupported_by` as an object.
#[tokio::test]
async fn a_candidate_alias_preview_answers_every_key_whatever_the_draft() {
    let (state, gw, _dir) = gateway().await;
    let what = "POST /api/op/candidate_alias_set";
    for row in [
        local_row("m1", true, Some(8192), Some(1024)),
        local_row("m2", false, Some(4096), None),
    ] {
        store::insert_local_model(&state.db, &row).await.unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let all = [
        "ok",
        "enabled_facets",
        "common_facets",
        "unsupported_by",
        "routable_candidates",
        "problems",
        "advisories",
        "fallback_usable",
        "context_length",
        "max_output_tokens",
        "addable",
        "error",
    ];

    // A draft with nothing in it: the first refusal is `error`, the rest of the
    // answer is still there.
    let live = op(&gw, "candidate_alias_set", json!({"action": "preview"})).await;
    let empty = common::round_trips::<dto::CandidateAliasPreview>(what, &live);
    assert_keys(&live, &all);
    assert!(!empty.ok);
    assert!(empty
        .error
        .as_deref()
        .unwrap()
        .contains("must not be empty"));
    assert_eq!(empty.addable.len(), 2, "every enabled row is addable");
    assert!(live["context_length"].is_null(), "{live}");

    // Everything already in: `addable` is an empty list, `error` a `null`.
    let live = op(
        &gw,
        "candidate_alias_set",
        json!({"action": "preview", "alias": "smart", "candidates": "m1,m2"}),
    )
    .await;
    let full = common::round_trips::<dto::CandidateAliasPreview>(what, &live);
    assert_keys(&live, &all);
    assert!(full.ok);
    assert_eq!(live["addable"], json!([]));
    assert!(live["error"].is_null(), "{live}");
    assert_eq!(full.context_length, Some(4096));
    assert_eq!(full.max_output_tokens, None, "m2 states none");
    assert!(live["context_length"].is_u64(), "{live}");
    assert!(live["max_output_tokens"].is_null(), "{live}");
    assert_eq!(full.unsupported_by["vision"], ["m2"]);

    // One candidate: the other is addable, and names what it would drop.
    let live = op(
        &gw,
        "candidate_alias_set",
        json!({"action": "preview", "alias": "smart", "candidates": "m1"}),
    )
    .await;
    let one = common::round_trips::<dto::CandidateAliasPreview>(what, &live);
    assert_eq!(
        one.addable,
        [dto::AddableCandidate {
            id: "m2".into(),
            missing: vec!["vision".into()],
        }]
    );
    assert_eq!(
        live["addable"],
        json!([{"id": "m2", "missing": ["vision"]}])
    );

    // A refusal of the facets leaves the answer whole.
    let live = op(
        &gw,
        "candidate_alias_set",
        json!({"action": "preview", "alias": "smart", "candidates": "m1",
               "capabilities_disabled": "bogus"}),
    )
    .await;
    let refused = common::round_trips::<dto::CandidateAliasPreview>(what, &live);
    assert_keys(&live, &all);
    assert!(!refused.ok);
    assert!(refused.error.unwrap().contains("bogus"));
}

/// The document says the op answers one of the two, and both validate.
#[tokio::test]
async fn the_document_describes_both_answers_of_candidate_alias_set() {
    let doc = lmgw_core::openapi::admin_doc();
    let schema = &doc["paths"]["/api/op/candidate_alias_set"]["post"]["responses"]["200"]
        ["content"]["application/json"]["schema"];
    let arms: Vec<&str> = schema["anyOf"]
        .as_array()
        .unwrap_or_else(|| panic!("an anyOf of the two answers: {schema}"))
        .iter()
        .filter_map(|a| a["$ref"].as_str())
        .collect();
    assert_eq!(
        arms,
        [
            "#/components/schemas/CandidateAliasPreview",
            "#/components/schemas/CandidateAliasSaved",
        ]
    );
    // Every key the saved answer's derivation has is optional (absent after a
    // delete) except the ones every action sends; the preview's are all sent.
    let required = |name: &str| -> Vec<String> {
        let mut r: Vec<String> = doc["components"]["schemas"][name]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        r.sort();
        r
    };
    assert_eq!(required("CandidateAliasSaved"), ["id", "message", "ok"]);
    assert_eq!(required("CandidateAliasPreview").len(), 12);
}
