//! §8, first item: every row of the §2.1 table refused when malformed with the
//! field named, unknown fields refused, an unknown `schema_version` refused, a
//! `config.<unknown>` template refused, the §2.6 subset accepted and everything
//! outside it refused, and `enum_from` building its enum with the fallback last
//! and once.

use super::*;
use serde_json::json;

/// A minimal valid `batch` manifest, as a `Value` so each test can break
/// exactly one thing about it.
fn batch() -> Value {
    json!({
        "schema_version": 1,
        "id": "mail-labeler",
        "name": "Mail labeler",
        "description": "Classifies unread mail.",
        "version": "2.0.0",
        "model": { "alias": "{{config.model}}", "temperature": 0.0 },
        "config": { "schema": {
            "type": "object",
            "properties": {
                "model": { "type": "string", "format": "model_alias", "default": "gemma4-e4b" },
                "categories": { "type": "array", "items": { "type": "string" },
                                "default": ["Newsletter", "Work"] },
                "label_prefix": { "type": "string", "default": "lmgw" },
                "limit": { "type": "integer", "default": 50, "minimum": 1 },
                "concurrency": { "type": "integer", "default": 4, "minimum": 1 }
            },
            "required": ["model"]
        } },
        "tools": [ { "label": "gws",
            "allowed": ["gws__gmail_search", "gws__gmail_get"],
            "install": { "kind": "git", "ref": "https://example.test/workspace",
                         "notes": "run the headless login once" } } ],
        "run": {
            "kind": "batch",
            "source": { "tool": "gws__gmail_search",
                        "args": { "query": "is:unread", "maxResults": "{{config.limit}}" } },
            "items_path": "/messages",
            "item": {
                "id": "{{item.id}}",
                "fetch": { "tool": "gws__gmail_get", "args": { "messageId": "{{item.id}}" } },
                "columns": { "date": "{{fetched.date}}", "subject": "{{fetched.subject}}" },
                "system": "Pick one of: {{config.categories}}.",
                "user": "Subject: {{fetched.subject}}",
                "output": { "field": "category", "enum_from": "config.categories",
                            "fallback": "Other" },
                "concurrency": "{{config.concurrency}}"
            },
            "review": { "editable": ["category"] },
            "apply": { "turn": {
                "tools": ["gws__gmail_batchModify"],
                "system": "You apply labels.",
                "prompt": "Prefix '{{config.label_prefix}}'. Rows: {{rows}}",
                "output": { "type": "object",
                            "properties": { "applied": { "type": "integer" } },
                            "required": ["applied"] } } }
        }
    })
}

fn chat() -> Value {
    json!({
        "schema_version": 1,
        "id": "docs-librarian",
        "name": "Docs librarian",
        "model": { "alias": "{{config.model}}" },
        "config": { "schema": { "type": "object", "properties": {
            "model": { "type": "string", "format": "model_alias" }
        }, "required": ["model"] } },
        "tools": [ { "label": "docs" } ],
        "run": { "kind": "chat", "system": "Answer from the corpora. {{config.model}}" }
    })
}

fn load_v(v: &Value) -> Result<Manifest, String> {
    load(&v.to_string())
}

/// Break one path of `batch()` and assert the refusal names it.
fn refused(mutate: impl FnOnce(&mut Value), needle: &str) {
    let mut v = batch();
    mutate(&mut v);
    let err = load_v(&v).expect_err("expected a refusal");
    assert!(
        err.contains(needle),
        "error did not name '{needle}':\n{err}"
    );
}

// ---------------------------------------------------------------------------
// §2.1 — the manifest table, row by row
// ---------------------------------------------------------------------------

#[test]
fn the_two_shapes_of_the_spec_load_and_validate() {
    let b = load_v(&batch()).unwrap();
    assert_eq!(b.kind(), "batch");
    assert_eq!(b.labels(), vec!["gws".to_string()]);
    let c = load_v(&chat()).unwrap();
    assert_eq!(c.kind(), "chat");
}

#[test]
fn schema_version_must_be_present_and_known() {
    let mut v = batch();
    v.as_object_mut().unwrap().remove("schema_version");
    let err = load_v(&v).unwrap_err();
    assert!(err.contains("schema_version"), "{err}");
    assert!(err.contains("version 1"), "{err}");

    // An unknown version is named, not guessed at.
    refused(|v| v["schema_version"] = json!(2), "schema_version 2");
    refused(|v| v["schema_version"] = json!(2), "understands version 1");
    // ... and a future manifest's unknown fields never get to speak first.
    let mut future = batch();
    future["schema_version"] = json!(7);
    future["orchestration"] = json!({ "kind": "dag" });
    let err = load_v(&future).unwrap_err();
    assert!(err.contains("schema_version 7"), "{err}");
    assert!(!err.contains("orchestration"), "{err}");
}

#[test]
fn id_is_checked_against_the_documented_pattern() {
    for (id, needle) in [
        ("", "id is required"),
        ("Mail", "lowercase"),
        ("-mail", "must start with"),
        ("mail labeler", "contains ' '"),
        ("mail_labeler", "contains '_'"),
        ("runs", "reserved"),
    ] {
        refused(|v| v["id"] = json!(id), needle);
    }
    assert_eq!(validate_id(&"a".repeat(64)), Ok(()));
    assert!(validate_id(&"a".repeat(65)).unwrap_err().contains("64"));
    assert_eq!(validate_id("m4il-labeler-2"), Ok(()));
}

#[test]
fn name_is_required_and_description_is_not() {
    refused(|v| v["name"] = json!("  "), "name is required");
    let mut v = batch();
    v.as_object_mut().unwrap().remove("description");
    assert!(load_v(&v).is_ok());
}

#[test]
fn model_needs_an_alias_and_takes_only_the_params_subset() {
    refused(
        |v| v["model"]["alias"] = json!(""),
        "model.alias is required",
    );
    // No `max_tokens`: the model's context is known and the run uses it (§2.1).
    refused(|v| v["model"]["max_tokens"] = json!(512), "max_tokens");
    refused(|v| v["model"]["top_q"] = json!(1), "top_q");
    // The subset that is allowed.
    let mut v = batch();
    v["model"] = json!({ "alias": "x", "temperature": 0.2, "top_p": 0.9, "top_k": 40,
                         "seed": 7, "reasoning": { "effort": "low" } });
    assert!(load_v(&v).is_ok(), "{:?}", load_v(&v));
}

#[test]
fn tools_entries_are_checked_and_install_is_an_enum() {
    refused(
        |v| v["tools"][0]["label"] = json!(""),
        "tools[0].label is required",
    );
    refused(
        |v| v["tools"][0]["allowed"] = json!([""]),
        "tools[0].allowed[0]",
    );
    refused(
        |v| v["tools"][0]["install"]["kind"] = json!("tarball"),
        "tarball",
    );
    refused(
        |v| v["tools"][0]["install"]["ref"] = json!(""),
        "install.ref is required",
    );
    refused(|v| v["tools"][0]["scope"] = json!("all"), "scope");
}

#[test]
fn an_unknown_run_kind_is_named() {
    let err = load_v(&{
        let mut v = batch();
        v["run"]["kind"] = json!("pipeline");
        v
    })
    .unwrap_err();
    assert!(err.contains("pipeline"), "{err}");
    assert!(err.contains("chat"), "{err}");
}

#[test]
fn an_unknown_field_anywhere_is_refused_naming_the_key() {
    refused(|v| v["categories"] = json!([]), "categories");
    refused(|v| v["run"]["item"]["colums"] = json!({}), "colums");
    refused(
        |v| v["run"]["apply"]["turn"]["temperature"] = json!(0.0),
        "temperature",
    );
    refused(|v| v["config"]["values"] = json!({}), "values");
}

// ---------------------------------------------------------------------------
// §2.2 — steps
// ---------------------------------------------------------------------------

#[test]
fn a_step_is_a_direct_call_or_a_turn_but_not_both_and_not_neither() {
    refused(
        |v| v["run"]["source"] = json!({ "args": { "q": "x" } }),
        "a step needs one of 'tool'",
    );
    refused(
        |v| v["run"]["source"] = json!({ "tool": "t", "turn": { "prompt": "p" } }),
        // Named, both of them (container-runtime §4.3).
        "declares tool and turn",
    );
    refused(
        |v| v["run"]["source"]["args"] = json!("not-an-object"),
        "args must be an object",
    );
    refused(
        |v| v["run"]["apply"] = json!({ "turn": { "prompt": "" } }),
        "turn.prompt is required",
    );
    refused(
        |v| v["run"]["apply"] = json!({ "turn": { "prompt": "p" }, "args": { "a": 1 } }),
        "args belongs to a direct call",
    );
}

/// `script` is sugar over the container runtime (container-runtime §4.2), and
/// its validation is the part that keeps the sugar honest.
#[test]
fn a_script_step_is_one_shape_with_one_schema() {
    // Two of the three, named.
    refused(
        |v| {
            v["run"]["apply"] =
                json!({ "script": "export async function apply(){}", "turn": { "prompt": "p" } })
        },
        "declares turn and script",
    );
    refused(
        |v| v["run"]["apply"] = json!({ "tool": "t", "script": "export async function apply(){}" }),
        "declares tool and script",
    );
    // `output` is the script's; a turn carries its own.
    refused(
        |v| {
            v["run"]["apply"] = json!({
                "turn": { "prompt": "p" },
                "output": { "type": "object", "properties": { "a": { "type": "integer" } } }
            })
        },
        "run.apply.output belongs to a script step",
    );
    refused(
        |v| v["run"]["apply"] = json!({ "script": "  " }),
        "script is empty",
    );
    refused(
        |v| v["run"]["apply"] = json!({ "script": "x", "args": { "a": 1 } }),
        "run.apply.args belongs to a direct call",
    );
    refused(
        |v| v["run"]["apply"] = json!({ "script": "x", "output": { "type": "string" } }),
        "run.apply.output.type is 'string'",
    );
}

/// A string and an array of lines are the same module — the array is what
/// survives hand-editing a manifest without a wall of `\n`.
#[test]
fn a_script_parses_the_same_from_a_string_and_from_lines() {
    let text = "export async function apply(ctx) {\n  return { applied: 0 };\n}";
    let mut a = batch();
    a["run"]["apply"] = json!({ "script": text });
    let mut b = batch();
    b["run"]["apply"] = json!({ "script": [
        "export async function apply(ctx) {",
        "  return { applied: 0 };",
        "}"
    ] });
    let (a, b) = (load_v(&a).unwrap(), load_v(&b).unwrap());
    let script = |m: &Manifest| m.apply_step().unwrap().script.as_ref().unwrap().text();
    assert_eq!(script(&a), text);
    assert_eq!(script(&a), script(&b));
    // Each form survives its own round trip, so a manifest the owner laid out
    // as lines does not come back as one long string.
    assert_eq!(load(&b.to_json()).unwrap(), b);
}

/// The step's `output` is the schema the run is closed against — the same
/// `ledger::check_output` a container's per-phase schema goes through.
#[test]
fn a_scripts_output_schema_is_the_apply_phases_output_schema() {
    let mut v = batch();
    v["run"]["apply"] = json!({
        "script": "export async function apply(){ return {}; }",
        "output": { "type": "object",
                    "properties": { "applied": { "type": "integer" } },
                    "required": ["applied"] },
    });
    let m = load_v(&v).unwrap();
    assert_eq!(
        m.output_schema("apply").and_then(|s| s.get("required")),
        Some(&json!(["applied"]))
    );
    assert!(m.output_schema("run").is_none());
}

/// **`apply.turn` loads and warns rather than erroring** — the regression the
/// whole §4.3 decision exists to prevent. `Manifest::errors` feeds `validate`,
/// which feeds `load`, which feeds `Agent::from_row`: an error here would brick
/// every stored manifest that has one.
#[test]
fn an_apply_turn_still_loads_because_it_is_a_warning_not_an_error() {
    let m = load_v(&batch()).expect("a manifest with apply.turn still loads");
    assert!(m.apply_step().unwrap().is_turn());
    assert!(m.errors().is_empty(), "{:?}", m.errors());
}

#[test]
fn a_turn_output_schema_must_be_an_object_schema() {
    refused(
        |v| v["run"]["apply"]["turn"]["output"] = json!({ "type": "string" }),
        "run.apply.turn.output.type is 'string'",
    );
    refused(
        |v| v["run"]["apply"]["turn"]["output"] = json!({ "type": "object" }),
        "has no 'properties'",
    );
    refused(
        |v| v["run"]["apply"]["turn"]["output"] = json!("Category"),
        "must be a JSON object schema",
    );
}

// ---------------------------------------------------------------------------
// §2.3 — templates checked against the config schema
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_config_field_in_any_template_is_refused_at_load() {
    refused(
        |v| v["model"]["alias"] = json!("{{config.mdl}}"),
        "model.alias",
    );
    refused(
        |v| v["model"]["alias"] = json!("{{config.mdl}}"),
        "config.mdl",
    );
    refused(
        |v| v["run"]["source"]["args"]["maxResults"] = json!("{{config.max}}"),
        "run.source.args.maxResults",
    );
    refused(
        |v| v["run"]["item"]["system"] = json!("{{config.cats}}"),
        "run.item.system",
    );
    refused(
        |v| v["run"]["apply"]["turn"]["prompt"] = json!("{{config.prefix}} {{rows}}"),
        "run.apply.turn.prompt",
    );
    refused(
        |v| v["run"]["item"]["columns"]["x"] = json!("{{config.q}}"),
        "columns.x",
    );
}

/// A `secret` config field may not be written into a model prompt (§2.6): a
/// chat thread persists its system prompt in the clear and a classify call
/// logs its request, so the manifest is refused before the token ever exists.
/// A tool argument is the case secrets are *for*, and stays allowed.
#[test]
fn a_secret_config_field_is_refused_in_a_prompt_and_allowed_in_a_tool_argument() {
    let with_secret = |v: &mut Value| {
        v["config"]["schema"]["properties"]["api_token"] =
            json!({ "type": "string", "format": "secret" });
    };

    // batch: both halves of the per-item call.
    for at in ["system", "user"] {
        let mut v = batch();
        with_secret(&mut v);
        v["run"]["item"][at] = json!("Use {{config.api_token}} and answer.");
        let err = load_v(&v).expect_err("a secret in a prompt must be refused");
        assert!(err.contains(&format!("run.item.{at}")), "{err}");
        assert!(err.contains("api_token"), "{err}");
        assert!(err.contains("prompt"), "{err}");
    }

    // chat: the one prompt it has.
    let mut v = chat();
    v["config"]["schema"]["properties"]["api_token"] =
        json!({ "type": "string", "format": "secret" });
    v["run"]["system"] = json!("You are a librarian. Key: {{config.api_token}}");
    let err = load_v(&v).expect_err("a secret in a chat prompt must be refused");
    assert!(
        err.contains("run.system") && err.contains("api_token"),
        "{err}"
    );

    // A turn's `prompt` and `system` are model prompts too — the batch
    // executor renders both straight into messages — so the rule reaches them.
    for at in ["prompt", "system"] {
        let mut v = batch();
        with_secret(&mut v);
        v["run"]["apply"]["turn"][at] = json!("Token {{config.api_token}}. Rows: {{rows}}");
        let err = load_v(&v).expect_err("a secret in a turn's prompt must be refused");
        assert!(err.contains(&format!("run.apply.turn.{at}")), "{err}");
        assert!(err.contains("api_token"), "{err}");
    }

    // The same placeholder in a tool argument loads: that is how a secret is
    // meant to be spent.
    let mut v = batch();
    with_secret(&mut v);
    v["run"]["source"]["args"]["token"] = json!("{{config.api_token}}");
    load_v(&v).expect("a secret in a tool argument is allowed");

    // And a non-secret field in a prompt is untouched by the rule.
    let mut v = batch();
    with_secret(&mut v);
    v["run"]["item"]["system"] = json!("Pick one of: {{config.categories}}.");
    load_v(&v).expect("the rule is about secret fields only");
}

#[test]
fn a_root_that_is_not_bound_at_that_position_is_refused() {
    // `rows` exists only for apply; `fetched` only after the item's fetch.
    refused(
        |v| v["run"]["source"]["args"]["q"] = json!("{{rows}}"),
        "not available here",
    );
    refused(
        |v| v["run"]["item"]["id"] = json!("{{fetched.id}}"),
        "not available here",
    );
    refused(
        |v| v["run"]["apply"]["turn"]["prompt"] = json!("{{item.id}} {{rows}}"),
        "not available here",
    );
    // ... but the positions that do bind them are fine.
    let m = load_v(&batch()).unwrap();
    assert!(m.errors().is_empty());
}

#[test]
fn item_and_fetched_paths_are_not_second_guessed() {
    let mut v = batch();
    v["run"]["item"]["columns"]["anything"] = json!("{{fetched.deeply.nested.thing}}");
    assert!(load_v(&v).is_ok());
}

// ---------------------------------------------------------------------------
// §2.4 — the batch shape
// ---------------------------------------------------------------------------

#[test]
fn items_path_must_be_a_json_pointer() {
    refused(
        |v| v["run"]["items_path"] = json!("messages"),
        "JSON pointer",
    );
    let mut v = batch();
    v["run"]["items_path"] = json!("/result/messages");
    assert!(load_v(&v).is_ok());
}

#[test]
fn the_row_identity_is_required() {
    refused(
        |v| v["run"]["item"]["id"] = json!(""),
        "run.item.id is required",
    );
}

#[test]
fn a_classify_stage_needs_both_the_prompt_and_the_schema() {
    refused(
        |v| {
            v["run"]["item"].as_object_mut().unwrap().remove("output");
        },
        "run.item.user is set but run.item.output is not",
    );
    refused(
        |v| {
            v["run"]["item"].as_object_mut().unwrap().remove("user");
        },
        "run.item.output is set but run.item.user is not",
    );
    // Neither: the list-only agent, which is a legitimate shape.
    let mut v = batch();
    let item = v["run"]["item"].as_object_mut().unwrap();
    item.remove("user");
    item.remove("system");
    item.remove("output");
    v["run"].as_object_mut().unwrap().remove("review");
    v["run"].as_object_mut().unwrap().remove("apply");
    assert!(load_v(&v).is_ok(), "{:?}", load_v(&v));
}

#[test]
fn concurrency_is_an_integer_or_a_template() {
    refused(
        |v| v["run"]["item"]["concurrency"] = json!(1.5),
        "must be an integer or a template",
    );
    refused(
        |v| v["run"]["item"]["concurrency"] = json!("{{config.threads}}"),
        "run.item.concurrency",
    );
    let mut v = batch();
    v["run"]["item"]["concurrency"] = json!(8);
    assert!(load_v(&v).is_ok());
}

#[test]
fn review_editable_must_name_a_field_the_output_produces() {
    refused(
        |v| v["run"]["review"]["editable"] = json!(["label"]),
        "run.review.editable names 'label'",
    );
    refused(
        |v| v["run"]["review"]["editable"] = json!(["label"]),
        "fields: category",
    );
}

#[test]
fn the_two_item_output_forms_are_exclusive_and_complete() {
    refused(
        |v| {
            v["run"]["item"]["output"] = json!({ "field": "c", "enum_from": "config.categories", "fallback": "Other",
                        "schema": { "type": "object", "properties": { "c": {} } } })
        },
        "not both",
    );
    refused(
        |v| v["run"]["item"]["output"] = json!({ "fallback": "Other" }),
        "needs either",
    );
    refused(
        |v| {
            v["run"]["item"]["output"] =
                json!({ "enum_from": "config.categories", "fallback": "x" })
        },
        "output.field is required with enum_from",
    );
    refused(
        |v| v["run"]["item"]["output"] = json!({ "field": "c", "enum_from": "config.categories" }),
        "output.fallback is required with enum_from",
    );
    refused(
        |v| v["run"]["item"]["output"]["enum_from"] = json!("categories"),
        "it must name a config field as config.<field>",
    );
    refused(
        |v| v["run"]["item"]["output"]["enum_from"] = json!("config.nope"),
        "output.enum_from names no config field",
    );
    // An enum has to come from an array of string, not a scalar field.
    refused(
        |v| v["run"]["item"]["output"]["enum_from"] = json!("config.label_prefix"),
        "which is string",
    );
    // The general form: a schema, and `field` belongs to the other one.
    let mut v = batch();
    v["run"]["item"]["output"] = json!({
        "schema": { "type": "object", "properties": { "category": { "type": "string" } },
                    "required": ["category"] },
        "fallback": { "category": "Other" }
    });
    assert!(load_v(&v).is_ok(), "{:?}", load_v(&v));
}

/// §8: `enum_from` builds the enum with `fallback` appended exactly once and
/// last — it is what sorts a row into "needs attention".
#[test]
fn enum_from_appends_the_fallback_once_and_last() {
    let m = load_v(&batch()).unwrap();
    let RunSpec::Batch { item, .. } = &m.run else {
        panic!("expected a batch run");
    };
    let out = item.output.as_ref().unwrap();

    let cfg = json!({ "categories": ["Work", "Finance"] });
    assert_eq!(out.enum_values(&cfg), vec!["Work", "Finance", "Other"]);

    // Already present, in the middle: moved to the end, still exactly once.
    let cfg = json!({ "categories": ["Work", "Other", "Finance"] });
    assert_eq!(out.enum_values(&cfg), vec!["Work", "Finance", "Other"]);

    // Duplicates in the config collapse; the fallback is not doubled.
    let cfg = json!({ "categories": ["Work", "Work", "Other", "Other"] });
    assert_eq!(out.enum_values(&cfg), vec!["Work", "Other"]);

    // Nothing configured at all: the fallback alone, never an empty enum.
    assert_eq!(out.enum_values(&json!({})), vec!["Other"]);

    // ... and the schema handed to the model wraps exactly that.
    let schema = out.response_schema(&json!({ "categories": ["Work"] }));
    assert_eq!(
        schema["properties"]["category"]["enum"],
        json!(["Work", "Other"])
    );
    assert_eq!(schema["required"], json!(["category"]));
    assert_eq!(schema["additionalProperties"], json!(false));
}

// ---------------------------------------------------------------------------
// §2.6 — the config schema subset
// ---------------------------------------------------------------------------

/// Schemas are parsed from **text**, never from a `json!` value: `json!`
/// builds a `BTreeMap`, which would alphabetize the properties before the
/// order-preserving deserializer ever saw them.
fn schema_text(doc: &str) -> ConfigSchema {
    serde_json::from_str(doc).unwrap()
}

fn schema_with(props: Value, required: Value) -> ConfigSchema {
    schema_text(&json!({ "type": "object", "properties": props, "required": required }).to_string())
}

#[test]
fn the_documented_subset_is_accepted_in_document_order() {
    let s = schema_text(
        r#"{ "type": "object", "required": ["model"], "properties": {
            "model":      { "type": "string", "format": "model_alias" },
            "token":      { "type": "string", "format": "secret" },
            "notes":      { "type": "string", "format": "multiline" },
            "mode":       { "type": "string", "enum": ["fast", "thorough"], "default": "fast" },
            "limit":      { "type": "integer", "default": 50, "minimum": 1, "maximum": 500 },
            "ratio":      { "type": "number", "default": 0.5 },
            "dry":        { "type": "boolean", "default": true },
            "categories": { "type": "array", "items": { "type": "string" },
                            "title": "Categories", "description": "One per label" }
        } }"#,
    );
    let fields = s.fields().unwrap();
    // Order is the author's, not the alphabet's — the form renders top to
    // bottom.
    assert_eq!(
        fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        [
            "model",
            "token",
            "notes",
            "mode",
            "limit",
            "ratio",
            "dry",
            "categories"
        ]
    );
    assert!(fields[0].required);
    assert_eq!(fields[0].format, Some(Format::ModelAlias));
    assert!(fields[1].is_secret());
    assert_eq!(fields[3].enum_values, ["fast", "thorough"]);
    assert_eq!(fields[4].minimum, Some(1.0));
    assert_eq!(fields[4].maximum, Some(500.0));
    assert_eq!(fields[7].ty, FieldType::Array);
    assert_eq!(fields[7].title.as_deref(), Some("Categories"));
}

#[test]
fn everything_outside_the_subset_is_refused_with_the_field_named() {
    for (props, needle) in [
        (json!({ "who": { "oneOf": [{ "type": "string" }] } }), "who"),
        (
            json!({ "who": { "type": "object", "properties": {} } }),
            "properties",
        ),
        (
            json!({ "who": { "type": "object" } }),
            "unknown type 'object'",
        ),
        (
            json!({ "who": { "type": "array", "items": { "type": "object" } } }),
            "only an array of string",
        ),
        (json!({ "who": { "type": "array" } }), "needs items"),
        (
            json!({ "who": { "type": "string", "items": { "type": "string" } } }),
            "items applies to an array field only",
        ),
        (
            json!({ "who": { "type": "string", "format": "email" } }),
            "unknown format 'email'",
        ),
        (
            json!({ "who": { "type": "integer", "format": "secret" } }),
            "format 'secret' applies to a string field",
        ),
        (
            json!({ "who": { "type": "integer", "enum": [1, 2] } }),
            "enum applies to a string field",
        ),
        (
            json!({ "who": { "type": "string", "minimum": 1 } }),
            "minimum/maximum apply to a number",
        ),
        (
            json!({ "who": { "type": "integer", "default": "50" } }),
            "default: expected integer",
        ),
        (
            json!({ "who": { "type": "array", "items": { "type": "string" }, "default": "a,b" } }),
            "expected an array of strings",
        ),
        (
            json!({ "who": { "type": "string", "enum": ["a"], "default": "b" } }),
            "'b' is not one of: a",
        ),
        (
            json!({ "who": { "type": "integer", "minimum": 5, "default": 1 } }),
            "below the minimum",
        ),
    ] {
        let s = schema_with(props, json!([]));
        let errs = s.fields().expect_err("expected a refusal");
        assert!(
            errs.iter().any(|e| e.contains(needle)),
            "no error named '{needle}': {errs:?}"
        );
        assert!(
            errs.iter().any(|e| e.contains("who")),
            "no error located the field: {errs:?}"
        );
    }
}

/// Mounts §5.1: two formats and one keyword, and the keyword belongs to
/// nothing else.
#[test]
fn a_mount_field_carries_an_access_mode_and_nothing_else_does() {
    let s = schema_text(
        r#"{ "type": "object", "required": ["notes"], "properties": {
            "notes": { "type": "string", "format": "directory", "access": "rw",
                       "title": "Notes folder" },
            "key":   { "type": "string", "format": "file" },
            "label": { "type": "string" }
        } }"#,
    );
    let fields = s.fields().unwrap();
    assert_eq!(fields[0].format, Some(Format::Directory));
    assert_eq!(fields[0].access, Access::Rw);
    // `ro` is the default, so a manifest that says nothing asks for the
    // narrower of the two.
    assert_eq!(fields[1].access, Access::Ro);
    assert_eq!(fields[1].format, Some(Format::File));
    assert!(!fields[2].is_mount());

    // The query the runtime and the path rules both iterate.
    let mounts: Vec<MountField> = fields.iter().filter_map(Field::mount).collect();
    assert_eq!(
        mounts
            .iter()
            .map(|m| (m.name.as_str(), m.kind, m.access, m.required))
            .collect::<Vec<_>>(),
        [
            ("notes", MountKind::Directory, Access::Rw, true),
            ("key", MountKind::File, Access::Ro, false),
        ]
    );
    assert_eq!(mounts[0].inside(), "/lmgw/mounts/notes");
}

/// Principle 3 in code: a manifest names a slot, never a host path — so the
/// two keywords that *would* name one are refused at load, and `access` is
/// refused anywhere it does not belong.
#[test]
fn a_mount_field_may_not_name_a_host_path_and_access_belongs_to_nowhere_else() {
    for (props, needle) in [
        (
            json!({ "notes": { "type": "string", "format": "directory",
                               "default": "/home/alice/Notes" } }),
            "a directory or file field cannot have a default — a manifest names a slot, never a \
             host path",
        ),
        (
            json!({ "notes": { "type": "string", "format": "file",
                               "default": "/etc/hosts" } }),
            "cannot have a default",
        ),
        (
            json!({ "notes": { "type": "string", "format": "directory",
                               "enum": ["/srv/a", "/srv/b"] } }),
            "a directory or file field cannot have an enum — a manifest names a slot, never a \
             host path",
        ),
        (
            json!({ "notes": { "type": "string", "access": "rw" } }),
            "access applies to a directory or file field only",
        ),
        (
            json!({ "notes": { "type": "string", "format": "secret", "access": "ro" } }),
            "access applies to a directory or file field only",
        ),
        (
            json!({ "notes": { "type": "string", "format": "directory", "access": "write" } }),
            "access is 'ro' or 'rw', not 'write'",
        ),
    ] {
        let s = schema_with(props, json!([]));
        let errs = s.fields().expect_err("expected a refusal");
        assert!(
            errs.iter().any(|e| e.contains(needle)),
            "no error said '{needle}': {errs:?}"
        );
        assert!(
            errs.iter()
                .any(|e| e.starts_with("config.schema.properties.notes:")),
            "the refusal did not name the property: {errs:?}"
        );
    }
}

#[test]
fn a_config_schema_that_is_not_an_object_and_a_dangling_required_are_refused() {
    let s = schema_text(r#"{ "type": "array", "properties": {} }"#);
    assert!(s.fields().unwrap_err()[0].contains("a config schema is an object"));

    let s = schema_with(json!({ "a": { "type": "string" } }), json!(["b"]));
    let errs = s.fields().unwrap_err();
    assert!(errs[0].contains("required names 'b'"), "{errs:?}");
}

#[test]
fn a_duplicate_property_is_a_named_error_rather_than_last_wins() {
    let err = load(
        r#"{"schema_version":1,"id":"a","name":"A","model":{"alias":"m"},
            "config":{"schema":{"type":"object","properties":{
              "x":{"type":"string"},"x":{"type":"integer"}}}},
            "run":{"kind":"chat"}}"#,
    )
    .unwrap_err();
    assert!(err.contains("duplicate key 'x'"), "{err}");
}

// ---------------------------------------------------------------------------
// Stored values (§2.6)
// ---------------------------------------------------------------------------

fn value_fields() -> Vec<Field> {
    schema_with(
        json!({
            "model": { "type": "string", "format": "model_alias" },
            "token": { "type": "string", "format": "secret" },
            "limit": { "type": "integer", "default": 50, "minimum": 1, "maximum": 100 },
            "mode":  { "type": "string", "enum": ["fast", "thorough"] },
            "cats":  { "type": "array", "items": { "type": "string" }, "default": ["A"] }
        }),
        json!(["model"]),
    )
    .fields()
    .unwrap()
}

fn map(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

#[test]
fn stored_values_are_checked_with_the_field_named() {
    let f = value_fields();
    assert!(validate_values(&f, &map(json!({ "model": "m" }))).is_ok());

    for (values, needle) in [
        (json!({}), "'model' is required"),
        (json!({ "model": "" }), "'model' is required"),
        (
            json!({ "model": "m", "limit": "50" }),
            "'limit': expected integer",
        ),
        (json!({ "model": "m", "limit": 0 }), "below the minimum"),
        (json!({ "model": "m", "limit": 1000 }), "above the maximum"),
        (
            json!({ "model": "m", "mode": "quick" }),
            "not one of: fast, thorough",
        ),
        (
            json!({ "model": "m", "cats": "A,B" }),
            "expected an array of strings",
        ),
        (
            json!({ "model": "m", "nope": 1 }),
            "'nope' is not a config field",
        ),
    ] {
        let err = validate_values(&f, &map(values)).expect_err("expected a refusal");
        assert!(err.contains(needle), "'{needle}' not in: {err}");
    }
}

#[test]
fn defaults_fill_in_and_stored_values_win() {
    let f = value_fields();
    let eff = effective_values(&f, &map(json!({ "model": "m", "limit": 7 })));
    assert_eq!(eff["model"], json!("m"));
    assert_eq!(eff["limit"], json!(7));
    assert_eq!(eff["cats"], json!(["A"]));
    assert!(eff.get("token").is_none());
}

#[test]
fn a_secret_is_never_returned_and_an_empty_submission_keeps_it() {
    let f = value_fields();
    let stored = map(json!({ "model": "m", "token": "hunter2" }));

    let masked = masked_values(&f, &stored);
    assert_eq!(masked["token"], json!({ "has_value": true }));
    assert!(!masked.to_string().contains("hunter2"));
    assert_eq!(
        masked_values(&f, &map(json!({ "model": "m" })))["token"],
        json!({ "has_value": false })
    );

    // The house convention: empty keeps, non-empty replaces, and the masked
    // view posted straight back keeps too.
    let kept = merge_values(&f, &stored, &map(json!({ "token": "" })));
    assert_eq!(kept["token"], json!("hunter2"));
    let kept = merge_values(&f, &stored, &map(json!({ "token": { "has_value": true } })));
    assert_eq!(kept["token"], json!("hunter2"));
    let replaced = merge_values(&f, &stored, &map(json!({ "token": "next" })));
    assert_eq!(replaced["token"], json!("next"));
    // A non-secret field submitted empty is just an empty value.
    let cleared = merge_values(&f, &stored, &map(json!({ "mode": "" })));
    assert_eq!(cleared["mode"], json!(""));

    // Export and duplicate drop it entirely, and say which names they dropped.
    let stripped = without_secrets(&f, &stored);
    assert!(stripped.get("token").is_none());
    assert_eq!(stripped["model"], json!("m"));
    assert_eq!(secret_names(&f), vec!["token".to_string()]);
}

/// Replacing a manifest keeps the stored config (§5), so a field the new
/// document no longer declares leaves its value behind in the column. If that
/// field was the `secret`, nothing marks the value as one any more — and export
/// and duplicate are the two paths that would carry it out of the process.
#[test]
fn a_value_whose_field_the_schema_no_longer_declares_is_not_exported() {
    let f = value_fields();
    let orphaned = map(json!({ "model": "m", "token": "hunter2", "gone": "old-secret" }));
    let stripped = without_secrets(&f, &orphaned);
    assert_eq!(stripped["model"], json!("m"));
    assert!(stripped.get("token").is_none(), "{stripped:?}");
    assert!(stripped.get("gone").is_none(), "{stripped:?}");
    // The read path has always dropped it; the two agree now.
    let masked = masked_values(&f, &orphaned);
    assert!(masked.get("gone").is_none(), "{masked}");
}

// ---------------------------------------------------------------------------
// container-runtime §4.1 / §4.3 — `run.kind = "container"`
// ---------------------------------------------------------------------------

/// A minimal valid `container` manifest, as a `Value` so each test can break
/// exactly one thing about it.
fn container() -> Value {
    json!({
        "schema_version": 1,
        "id": "mail-labeler",
        "name": "Mail labeler",
        "model": { "alias": "{{config.model}}" },
        "config": { "schema": { "type": "object", "properties": {
            "model": { "type": "string", "format": "model_alias" }
        } } },
        "tools": [ { "label": "gws", "allowed": ["gws__gmail_batchModify"] } ],
        "run": {
            "kind": "container",
            "image": "localhost/mail-labeler:1",
            "columns": ["date", "subject"],
            "review": { "editable": ["category"] },
            "phases": ["run", "apply"],
            "limits": { "memory_mb": 512, "cpus": 2.0, "pids": 256,
                        "deadline_seconds": 600, "stop_grace_seconds": 10,
                        "read_only": true },
            "output": { "apply": { "type": "object",
                        "properties": { "applied": { "type": "integer" } },
                        "required": ["applied"] } }
        }
    })
}

/// Break one path of `container()` and assert the refusal names it.
fn container_refused(mutate: impl FnOnce(&mut Value), needle: &str) {
    let mut v = container();
    mutate(&mut v);
    let err = load_v(&v).expect_err("expected a refusal");
    assert!(
        err.contains(needle),
        "error did not name '{needle}':\n{err}"
    );
}

#[test]
fn the_container_shape_of_the_spec_loads_and_validates() {
    let m = load_v(&container()).unwrap();
    assert_eq!(m.kind(), "container");
    assert_eq!(m.image(), Some("localhost/mail-labeler:1"));
    assert_eq!(m.phases(), vec!["run".to_string(), "apply".to_string()]);
    assert!(m.output_schema("apply").is_some());
    // Per phase, and a phase with no key is unvalidated rather than sharing
    // the other one's schema.
    assert!(m.output_schema("run").is_none());
    assert_eq!(m.output_validated(), vec!["apply".to_string()]);
    // A byte-stable round trip: the canonical serialization re-reads.
    let again = load(&m.to_json()).unwrap();
    assert_eq!(again, m);
}

#[test]
fn the_limits_defaults_are_the_printed_ones_and_survive_an_omitted_block() {
    let mut v = container();
    v["run"].as_object_mut().unwrap().remove("limits");
    let l = load_v(&v).unwrap().limits();
    assert_eq!(l.memory_mb, DEFAULT_MEMORY_MB);
    assert_eq!(l.cpus, DEFAULT_CPUS);
    assert_eq!(l.pids, DEFAULT_PIDS);
    assert_eq!(l.deadline_seconds, DEFAULT_DEADLINE_SECONDS);
    assert_eq!(l.stop_grace_seconds, DEFAULT_STOP_GRACE_SECONDS);
    assert!(l.read_only);
    // And a partial block keeps the defaults for what it did not mention.
    let mut v = container();
    v["run"]["limits"] = json!({ "memory_mb": 64 });
    let l = load_v(&v).unwrap().limits();
    assert_eq!(l.memory_mb, 64);
    assert_eq!(l.pids, DEFAULT_PIDS);
}

#[test]
fn zero_is_accepted_everywhere_as_no_limit_and_below_zero_is_refused() {
    let mut v = container();
    v["run"]["limits"] = json!({ "memory_mb": 0, "cpus": 0, "pids": 0,
                                 "deadline_seconds": 0, "stop_grace_seconds": 0 });
    let l = load_v(&v).unwrap().limits();
    assert_eq!(
        (
            l.memory_mb,
            l.pids,
            l.deadline_seconds,
            l.stop_grace_seconds
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(l.cpus, 0.0);

    // Named, every one of them: a limit the owner cannot locate is one they
    // cannot fix, which is why `Limits` deserializes by hand.
    container_refused(
        |v| v["run"]["limits"] = json!({ "cpus": -1.0 }),
        "run.limits.cpus is -1",
    );
    container_refused(
        |v| v["run"]["limits"] = json!({ "memory_mb": -1 }),
        "run.limits.memory_mb is -1",
    );
    container_refused(
        |v| v["run"]["limits"] = json!({ "deadline_seconds": "600" }),
        "run.limits.deadline_seconds",
    );
    container_refused(|v| v["run"]["limits"] = json!({ "nope": 1 }), "nope");
}

#[test]
fn a_container_with_no_image_and_no_service_is_refused_naming_the_field() {
    container_refused(
        |v| {
            v["run"].as_object_mut().unwrap().remove("image");
        },
        "run.image is required",
    );
    container_refused(|v| v["run"]["image"] = json!("  "), "run.image is empty");
    // With a service declared it loads: the image can come from a dev_url.
    let mut v = container();
    v["run"].as_object_mut().unwrap().remove("image");
    v["run"]["service"] = json!({ "port": 8080 });
    let m = load_v(&v).unwrap();
    assert_eq!(m.image(), None);
}

#[test]
fn the_phase_list_is_checked_against_what_a_container_can_implement() {
    container_refused(
        |v| v["run"]["phases"] = json!(["classify"]),
        "run.phases[0]",
    );
    container_refused(|v| v["run"]["phases"] = json!([]), "run.phases is empty");
    container_refused(
        |v| v["run"]["phases"] = json!(["run", "run"]),
        "names 'run' twice",
    );
    // A review gate in front of a phase the image does not implement.
    container_refused(
        |v| {
            v["run"]["phases"] = json!(["run"]);
            v["run"].as_object_mut().unwrap().remove("output");
        },
        "run.review is set but run.phases has no 'apply'",
    );
    // The default is `["run"]`, and it is a default, not a silence.
    let mut v = container();
    for key in ["phases", "review", "output"] {
        v["run"].as_object_mut().unwrap().remove(key);
    }
    assert_eq!(load_v(&v).unwrap().phases(), vec!["run".to_string()]);
}

#[test]
fn service_and_provides_are_validated_even_though_wp4_serves_them() {
    container_refused(
        |v| v["run"]["provides"] = json!({ "mcp": "/mcp" }),
        "run.provides.mcp needs run.service",
    );
    container_refused(
        |v| {
            v["run"]["service"] = json!({ "port": 70000 });
        },
        "a TCP port is 1–65535",
    );
    container_refused(
        |v| {
            v["run"]["service"] = json!({ "port": 8080 });
            v["run"]["provides"] = json!({ "mcp": "mcp" });
        },
        "must start with '/'",
    );
    // A reserved tool prefix: the registration would be named after the agent.
    container_refused(
        |v| {
            v["id"] = json!("docs");
            v["run"]["service"] = json!({ "port": 8080 });
            v["run"]["provides"] = json!({ "mcp": "/mcp" });
        },
        "which is reserved",
    );
    container_refused(
        |v| {
            v["run"]["service"] = json!({ "port": 8080, "health_path": "healthz" });
        },
        "run.service.health_path 'healthz' must start with '/'",
    );
    let mut ok = container();
    ok["run"]["service"] = json!({ "port": 8080, "health_path": "/healthz" });
    ok["run"]["provides"] = json!({ "mcp": "/mcp" });
    load_v(&ok).unwrap();

    // `0` is "no limit" here too, and a **blank** health_path is a choice, not
    // an omission: it health-checks with a TCP connect instead of an HTTP GET
    // (§3.3), for an image whose port speaks something HTTP cannot introduce
    // itself to.
    let mut ok = container();
    ok["run"]["service"] =
        json!({ "port": 8080, "health_path": "", "idle_seconds": 0, "start_timeout_seconds": 0 });
    let m = load_v(&ok).unwrap();
    let RunSpec::Container { service, .. } = &m.run else {
        panic!("a container manifest");
    };
    let s = service.as_ref().unwrap();
    assert_eq!(s.health_path, "");
    assert_eq!(s.idle_seconds, 0, "never idle-stop");
    assert_eq!(s.start_timeout_seconds, 0, "wait as long as it takes");

    // The defaults are the ones the Run tab prints.
    let mut ok = container();
    ok["run"]["service"] = json!({ "port": 8080 });
    let m = load_v(&ok).unwrap();
    let RunSpec::Container { service, .. } = &m.run else {
        panic!("a container manifest");
    };
    let s = service.as_ref().unwrap();
    assert_eq!(s.health_path, "/");
    assert_eq!(s.idle_seconds, 300);
    assert_eq!(s.start_timeout_seconds, 30);
}

#[test]
fn the_columns_and_the_output_schema_are_checked_like_every_other_field() {
    container_refused(
        |v| v["run"]["columns"] = json!(["a", "a"]),
        "names 'a' twice",
    );
    container_refused(
        |v| v["run"]["columns"] = json!([" "]),
        "run.columns[0] is empty",
    );
    // `output` is **per phase** (§4.1, revised in review): one schema shared by
    // run and apply is unusable, since a run reports rows and an apply reports
    // what it wrote. Every error names the phase it is about.
    container_refused(
        |v| v["run"]["output"] = json!({ "apply": { "type": "array" } }),
        "run.output.apply.type",
    );
    container_refused(
        |v| v["run"]["output"] = json!({ "apply": "nope" }),
        "run.output.apply must be a JSON object schema",
    );
    container_refused(
        |v| {
            v["run"]["output"] = json!({ "classify": { "type": "object",
                                         "properties": { "a": { "type": "integer" } } } })
        },
        "run.output names the phase 'classify'",
    );
    container_refused(|v| v["run"]["output"] = json!({}), "run.output is empty");
    // The map itself is refused by serde, which says what shape it wanted.
    container_refused(
        |v| v["run"]["output"] = json!("nope"),
        "expected a JSON object",
    );
    // And an unknown field is refused naming the key, as everywhere else.
    container_refused(|v| v["run"]["retries"] = json!(3), "retries");
}

#[test]
fn the_pull_policy_is_a_named_value_with_never_as_the_default() {
    let mut v = container();
    v["run"].as_object_mut().unwrap().remove("pull");
    let m = load_v(&v).unwrap();
    // Round-tripped explicitly rather than skipped: the choice is never
    // implicit (§3.4), so the canonical document says which one is in force.
    let doc: Value = serde_json::from_str(&m.to_json()).unwrap();
    assert_eq!(doc["run"]["pull"], json!("never"));
    // serde's own variant-list message, which names the three: a policy that
    // is never implicit has to say what the alternatives were.
    container_refused(
        |v| v["run"]["pull"] = json!("maybe"),
        "`never`, `missing`, `always`",
    );
}

/// An agent whose own `provides.mcp` tools are in its allow list would call
/// itself through the gateway: out on `/mcp`, back in through
/// `/agents/<id>/app/`, into the container that made the call. The registration
/// prefix *is* the agent id, so the loop is spottable at validation time
/// (final review).
#[test]
fn an_allow_list_naming_this_agents_own_provided_tools_is_refused() {
    container_refused(
        |v| {
            v["id"] = json!("board");
            v["run"]["service"] = json!({ "port": 8080 });
            v["run"]["provides"] = json!({ "mcp": "/mcp" });
            v["tools"] = json!([{ "label": "board", "allowed": ["board__pin"] }]);
        },
        "one of this agent's own tools",
    );

    // Only under *its own* prefix, and only when it actually provides tools:
    // another agent's `provides.mcp` row is an ordinary MCP server to this one.
    let mut ok = container();
    ok["id"] = json!("board");
    ok["run"]["service"] = json!({ "port": 8080 });
    ok["run"]["provides"] = json!({ "mcp": "/mcp" });
    ok["tools"] = json!([{ "label": "otherboard", "allowed": ["otherboard__pin"] }]);
    load_v(&ok).unwrap();

    // And a manifest with no `provides.mcp` registers nothing, so a tool that
    // happens to share its id's prefix belongs to somebody else.
    let mut ok = container();
    ok["id"] = json!("board");
    ok["tools"] = json!([{ "label": "board", "allowed": ["board__pin"] }]);
    load_v(&ok).unwrap();
}
