//! Which answer fields the document says are never `null` (typing sprint
//! W16): an `Option` the gateway leaves out when it is `None`
//! (`skip_serializing_if`), or always fills, states it with
//! `openapi_ext::non_null`, one field at a time, each proven against every
//! builder of its type. "Optional" alone is not "never null" (commit
//! 9361b417): a flattened `Option`'s fields are optional and still sent as
//! `null`, so the fields that keep their `null` are pinned here too.
//!
//! The live half: an answer with such a field left out reads back through
//! its type and validates against the document, which no longer allows the
//! `null` the gateway never sends.

use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::Value;

use crate::common::{self, Gw};

/// `(schema, path)`: the type's documented schema (a component or an
/// answer's inline schema, by title) and the property, `.` stepping into a
/// property's object or its array's items.
const NEVER_NULL: &[(&str, &str)] = &[
    ("AgentServiceMount", "host"),
    ("ClipList", "error"),
    ("LabError", "code"),
    ("BuildsResponse", "usage_error"),
    ("BuildGetResponse", "usage_error"),
    ("Thread", "continue"),
    ("ApprovalRequest", "call_id"),
    ("ApprovalRequested", "call_id"),
    ("ApprovalDecided", "call_id"),
    ("ToolStartFrame", "id"),
    ("Timings", "prompt_ms"),
    ("Timings", "prompt_per_second"),
    ("Timings", "predicted_ms"),
    ("Timings", "predicted_per_second"),
    ("Timings", "cache_n"),
    ("Timings", "draft_n"),
    ("Timings", "draft_n_accepted"),
    ("ErrorFrame", "code"),
    ("ErrorFrame", "kind"),
    ("ErrorFrame", "server_label"),
    ("DoneFrame", "images_note"),
    ("DoneFrame", "pending_approvals"),
    ("Message", "images_note"),
    ("Message", "task"),
    ("Message", "pending_approvals"),
    ("SearchPage", "threads.hits.message_id"),
    ("SearchPage", "threads.hits.role"),
    ("ModelDone", "port"),
    ("ModelDone", "applied"),
    ("ModelDone", "running"),
    ("ModelDone", "held"),
    ("SearchTraceView", "knn_skipped"),
    ("ResumeResult", "kind"),
    ("ResumeResult", "already_running"),
    ("ResumeResult", "rechunk"),
    ("ResumeResult", "message"),
    ("KnowledgeSource", "notice"),
    ("ToolResultFrame", "task"),
    ("ModelTestPassed", "dimensions"),
    ("ModelTestPassed", "scored"),
    ("ChatTestPassed", "dimensions"),
    ("ChatTestPassed", "scored"),
    ("ChatTestPassed", "note"),
    ("ModelTestFailed", "public_name"),
    ("ModelTestFailed", "endpoint"),
    ("RungTest", "error"),
    ("ReasoningControl", "enabled"),
    ("ReasoningControl", "effort"),
    ("ReasoningControl", "budget_tokens"),
    ("Params", "top_k"),
    ("Params", "max_tokens"),
    ("Params", "seed"),
    ("Params", "reasoning"),
    ("AudioModelView", "residency_note"),
    ("Dictation", "asr_ms"),
    ("RuntimeStatus", "image_capabilities"),
    ("RuntimeStatus", "llama_props"),
    ("RuntimeStatus", "rung"),
    ("RuntimeStatus", "climbing"),
    ("RuntimeStatus", "owner"),
    ("RuntimeStatus", "placement"),
    ("ModelState", "cause"),
    ("ModelState", "answered_by"),
    ("ModelState", "reason"),
    ("ModelState", "message"),
    ("ModelState", "needed_bytes"),
    ("ModelState", "capacity_bytes"),
    ("MessageVoice", "asr"),
    ("MessageVoice", "asr_answered_by"),
    ("MessageVoice", "asr_ms"),
    ("MessageVoice", "audio_ms"),
    ("MessageVoice", "tts"),
    ("MessageVoice", "tts_answered_by"),
    ("MessageVoice", "voice"),
    ("MessageVoice", "unheard"),
    ("MessageVoice", "input"),
    ("MessageVoice", "transcript_error"),
    ("MessageVoice", "timing"),
    ("VoiceTiming", "reasoning_ms"),
    ("VoiceTiming", "first_clause"),
    ("VoiceTiming", "input"),
    ("VoiceTiming", "input_why"),
    ("VoiceTiming", "transcript_wait_ms"),
    ("ServedModel", "voice"),
    ("AttachmentMeta", "blockers"),
    ("AttachmentMeta", "hints"),
    ("ChatExport", "folder"),
    ("ExportMessage", "ir_messages_unparsed"),
    ("ExportMessage", "images_note"),
    ("ExportMessage", "task"),
];

/// Fields that are left out of some answers or frames and sent as `null` in
/// others, or whose wire is written by something other than their type: the
/// document keeps their `null`.
const STAYS_NULLABLE: &[(&str, &str, &str)] = &[
    (
        "DoneFrame",
        "answered_by",
        "a flattened Option's field: absent when nothing was saved, null when the thread's \
         model answered itself (9361b417)",
    ),
    (
        "DoneFrame",
        "timings",
        "null for a cloud upstream on every saved turn",
    ),
    (
        "StageHitView",
        "score",
        "null means the embedder or reranker returned NaN, which serde_json writes as null",
    ),
    (
        "Params",
        "temperature",
        "an f64: serde_json writes a non-finite one as null",
    ),
];

/// The `$ref` target of `schema` in `doc`, or `schema` itself.
fn deref<'a>(doc: &'a Value, schema: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(Value::as_str) {
        Some(r) => doc
            .pointer(&r.replacen("#", "", 1))
            .unwrap_or_else(|| panic!("{r} resolves in the document")),
        None => schema,
    }
}

/// The property schema at `path` under `schema`: each step reads the
/// property of the (dereferenced) object, through an array's `items`.
fn property<'a>(doc: &'a Value, schema: &'a Value, path: &str) -> Option<&'a Value> {
    let mut at = schema;
    for step in path.split('.') {
        at = deref(doc, at);
        while let Some(items) = at.get("items") {
            at = deref(doc, items);
        }
        at = at.get("properties")?.get(step)?;
    }
    Some(at)
}

/// Whether a property schema admits `null`: a `null` in its `type` list, or
/// an `anyOf`/`oneOf` arm of type `null`.
fn admits_null(prop: &Value) -> bool {
    let in_type = match prop.get("type") {
        Some(Value::String(t)) => t == "null",
        Some(Value::Array(ts)) => ts.iter().any(|t| t == "null"),
        _ => false,
    };
    let in_arms = ["anyOf", "oneOf"].iter().any(|k| {
        prop.get(*k).and_then(Value::as_array).is_some_and(|arms| {
            arms.iter()
                .any(|a| a.get("type") == Some(&Value::from("null")))
        })
    });
    in_type || in_arms
}

/// Every schema the document has for `name`, with the property at `path`
/// in each; a name or a property the document lacks fails.
fn properties_of(doc: &Value, name: &str, path: &str) -> Vec<Value> {
    let schemas = common::documented_schemas(doc, name);
    assert!(
        !schemas.is_empty(),
        "the document has no schema titled {name}"
    );
    schemas
        .iter()
        .map(|s| {
            property(doc, s, path)
                .unwrap_or_else(|| panic!("{name}.{path} is not in the document"))
                .clone()
        })
        .collect()
}

#[test]
fn the_fields_proven_never_null_say_so_in_the_document() {
    let doc = lmgw_core::openapi::admin_doc();
    for (name, path) in NEVER_NULL {
        for prop in properties_of(doc, name, path) {
            assert!(
                !admits_null(&prop),
                "{name}.{path} is never null on the wire, the document allows it: {prop}"
            );
            assert!(
                prop.get("default") != Some(&Value::Null),
                "{name}.{path} is never null, yet its default is `null`: {prop}"
            );
        }
    }
}

/// Every schema object under `value`, at any depth, that names `null` as its
/// default while its own `type` or `anyOf`/`oneOf` leaves `null` out: a
/// field `non_null` touched, left with the `"default": null` a `serde(default)`
/// makes the derive write beside it.
fn null_default_without_null(value: &Value, at: &str, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            let names_a_type =
                map.contains_key("type") || map.contains_key("anyOf") || map.contains_key("oneOf");
            if names_a_type && map.get("default") == Some(&Value::Null) && !admits_null(value) {
                found.push(at.to_string());
            }
            for (k, v) in map {
                null_default_without_null(v, &format!("{at}/{k}"), found);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                null_default_without_null(v, &format!("{at}/{i}"), found);
            }
        }
        _ => {}
    }
}

/// `non_null` also removes the `"default": null` a `serde(default)` makes the
/// derive write beside a type: no schema in either document says its type is
/// never `null` and its default is.
#[test]
fn no_never_null_field_keeps_a_null_default() {
    for (which, doc) in [
        ("admin", lmgw_core::openapi::admin_doc()),
        ("v1", lmgw_core::openapi::v1_doc()),
    ] {
        let mut found = Vec::new();
        null_default_without_null(doc, "", &mut found);
        assert!(
            found.is_empty(),
            "the {which} document has a `null` default beside a type that is never `null`: {found:?}"
        );
    }
}

#[test]
fn a_field_sent_as_null_or_written_by_another_type_keeps_its_null() {
    let doc = lmgw_core::openapi::admin_doc();
    for (name, path, why) in STAYS_NULLABLE {
        for prop in properties_of(doc, name, path) {
            assert!(
                admits_null(&prop),
                "{name}.{path} ({why}) lost its null: {prop}"
            );
        }
    }
}

async fn gateway() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;
    (state, gw)
}

/// A title hit names no message: `message_id` and `role` are left out, and
/// the page reads back and validates against a document that gives them no
/// `null`.
#[tokio::test]
async fn a_title_hit_leaves_message_id_and_role_out() {
    let (state, gw) = gateway().await;
    let id = store::create_chat_thread(&state.db, "m", "chat")
        .await
        .unwrap();
    store::set_chat_thread_title(&state.db, id, "Quarterly planning", None)
        .await
        .unwrap();
    let resp = gw
        .client()
        .get(format!("{gw}/chat/api/search?q=Quarterly"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let live: Value = resp.json().await.unwrap();
    let hit = &live["threads"][0]["hits"][0];
    assert_eq!(hit["kind"], "t", "{live}");
    for key in ["message_id", "role"] {
        assert!(
            hit.get(key).is_none(),
            "{key} is left out of a title hit: {live}"
        );
    }
    common::round_trips::<lmgw_api_types::chat_threads::SearchPage>("chat search", &live);
    let doc = lmgw_core::openapi::admin_doc();
    for path in ["threads.hits.message_id", "threads.hits.role"] {
        for prop in properties_of(doc, "SearchPage", path) {
            assert!(!admits_null(&prop), "SearchPage.{path}: {prop}");
        }
    }
}

/// With no builds there is no image usage to read: `usage_error` is left
/// out, and the answer validates against a schema without its `null`.
#[tokio::test]
async fn the_builds_answer_leaves_usage_error_out_when_nothing_failed() {
    let (_state, gw) = gateway().await;
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/builds"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let live: Value = resp.json().await.unwrap();
    assert!(live.get("usage_error").is_none(), "{live}");
    common::round_trips::<lmgw_api_types::builds::BuildsResponse>("builds", &live);
    let doc = lmgw_core::openapi::admin_doc();
    for prop in properties_of(doc, "BuildsResponse", "usage_error") {
        assert_eq!(prop["type"], "string", "{prop}");
    }
}
