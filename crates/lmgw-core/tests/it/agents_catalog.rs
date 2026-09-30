//! The agent catalog over the real router (agent-catalog design §5, §8):
//! import, export, the reads, the ops, and the once-only seed of the shipped
//! agents.
//!
//! These go through `build_router` over HTTP, so a route that stopped being
//! wired up fails here rather than in the browser.

use lmgw_core::agents::seed;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};

use crate::common;
use common::{serve, Gw};

async fn get(base: &Gw, path: &str) -> (u16, String) {
    let resp = base
        .client()
        .get(format!("{base}{path}"))
        .send()
        .await
        .unwrap();
    (
        resp.status().as_u16(),
        resp.text().await.unwrap_or_default(),
    )
}

async fn get_json(base: &Gw, path: &str) -> Value {
    let (status, body) = get(base, path).await;
    assert_eq!(status, 200, "GET {path}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("GET {path} is not JSON ({e}): {body}"))
}

async fn op(base: &Gw, name: &str, args: Value) -> (u16, Value) {
    let resp = base
        .client()
        .post(format!("{base}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let v = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("op {name} is not JSON ({e}): {body}"));
    (status, v)
}

/// `POST /api/agents/import` with the document as the raw body, exactly as a
/// dropped file or a paste arrives.
async fn import(base: &Gw, query: &str, doc: &str) -> (u16, Value) {
    let resp = base
        .client()
        .post(format!("{base}/api/agents/import{query}"))
        .header("content-type", "application/json")
        .body(doc.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let v =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("import is not JSON ({e}): {body}"));
    (status, v)
}

/// A batch agent with a `secret` config field and a tool label no gateway in a
/// test has registered — both halves of §8's import list in one document.
///
/// Its id is deliberately **not** `mail-labeler`: that one is shipped and
/// seeded into every catalog, so a fixture wearing it would collide with the
/// built-in and turn every import here into a replace.
///
/// A raw string rather than `json!`: `json!` builds a `BTreeMap`, which would
/// alphabetize the config properties before they ever reached the manifest
/// parser, and the order of the form's fields is part of what is asserted here.
fn mail_doc() -> String {
    r#"{
  "schema_version": 1,
  "id": "mail-tagger",
  "name": "Mail tagger",
  "description": "Classifies unread mail. Nothing is written until you apply.",
  "version": "2.0.0",
  "model": { "alias": "{{config.model}}", "temperature": 0.0 },
  "config": { "schema": { "type": "object", "properties": {
    "model":         { "type": "string", "format": "model_alias", "default": "gemma4-e4b" },
    "categories":    { "type": "array", "items": { "type": "string" },
                       "default": ["Newsletter", "Work"] },
    "label_prefix":  { "type": "string", "default": "lmgw" },
    "webhook_token": { "type": "string", "format": "secret" },
    "limit":         { "type": "integer", "default": 50, "minimum": 1 }
  }, "required": ["model"] } },
  "tools": [ { "label": "gws",
    "allowed": ["gws__gmail_search", "gws__gmail_batchModify"],
    "install": { "kind": "git", "ref": "https://example.test/workspace",
                 "notes": "run the headless login once" } } ],
  "run": { "kind": "batch",
    "source": { "tool": "gws__gmail_search",
                "args": { "query": "is:unread", "maxResults": "{{config.limit}}" } },
    "items_path": "/messages",
    "item": {
      "id": "{{item.id}}",
      "columns": { "subject": "{{item.subject}}" },
      "system": "Pick one of: {{config.categories}}.",
      "user": "Subject: {{item.subject}}",
      "output": { "field": "category", "enum_from": "config.categories",
                  "fallback": "Other" }
    },
    "review": { "editable": ["category"] },
    "apply": { "turn": {
      "tools": ["gws__gmail_batchModify"],
      "prompt": "Prefix '{{config.label_prefix}}'. Rows: {{rows}}",
      "output": { "type": "object",
                  "properties": { "applied": { "type": "integer" } },
                  "required": ["applied"] } } } }
}"#
    .to_string()
}

fn card<'a>(cards: &'a Value, id: &str) -> Option<&'a Value> {
    cards
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == json!(id))
}

// ---------------------------------------------------------------------------
// Import (§5, §8)
// ---------------------------------------------------------------------------

/// A label the gateway does not have is a **warning**, not a refusal: an MCP
/// server is registered before its image is pulled, and an agent is imported
/// before its server is wired, for the same reason.
#[tokio::test]
async fn a_missing_server_label_imports_with_a_warning_and_requires_ok_false() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    let (status, report) = import(&base, "", &mail_doc()).await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["ok"], json!(true));
    assert_eq!(report["id"], json!("mail-tagger"));
    assert_eq!(report["replaced"], json!(false));
    let warnings = report["warnings"].as_array().unwrap();
    // Two now: the tool gap, and §4.3's `apply_turn` — this fixture still
    // declares one, and an import says so rather than refusing the document.
    assert_eq!(warnings.len(), 2, "{report}");
    assert!(warnings[0].as_str().unwrap().contains("'gws'"), "{report}");
    assert!(
        warnings[1]
            .as_str()
            .unwrap()
            .contains("apply may not run a model turn"),
        "{report}"
    );
    // The report names the alternatives, and the install hint rides along for
    // the MCP page link.
    assert_eq!(report["requires"][0]["registered"], json!(false));
    assert_eq!(report["requires"][0]["install"]["kind"], json!("git"));

    let cards = get_json(&base, "/api/agents").await;
    let c = card(&cards, "mail-tagger").expect("the agent was not saved");
    assert_eq!(c["requires_ok"], json!(false));
    assert_eq!(c["kind"], json!("batch"));
    assert_eq!(c["source"], json!("imported"));
    assert_eq!(c["enabled"], json!(true));
    // `{{config.model}}` is resolved against the schema default, so the card
    // says what this agent is actually pointed at.
    assert_eq!(c["model_alias"], json!("{{config.model}}"));
    assert_eq!(c["effective_model"], json!("gemma4-e4b"));
}

#[tokio::test]
async fn validate_only_reports_and_writes_nothing() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    let (status, report) = import(&base, "?validate_only=1", &mail_doc()).await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["ok"], json!(true));
    assert_eq!(report["validate_only"], json!(true));
    assert_eq!(report["warnings"].as_array().unwrap().len(), 2);

    let cards = get_json(&base, "/api/agents").await;
    assert!(card(&cards, "mail-tagger").is_none(), "{cards}");
    let (status, _) = get(&base, "/api/agents/mail-tagger").await;
    assert_eq!(status, 404);

    // "Would this land?" has to include the collision the real import would
    // have refused on, rather than leaving it to be discovered by pressing
    // Import.
    import(&base, "", &mail_doc()).await;
    let (status, report) = import(&base, "?validate_only=1", &mail_doc()).await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["validate_only"], json!(true));
    let collision = report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w.as_str().unwrap().contains("replace=1"));
    assert!(collision, "no collision warning: {report}");
}

/// `agent_set` takes the manifest as a string or as an object. The object has
/// already been through a `BTreeMap` by the time it arrives, so its config form
/// is alphabetical — accepted, and said out loud.
#[tokio::test]
async fn the_object_form_of_agent_set_warns_that_it_re_sorted_the_form() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    let as_object: Value = serde_json::from_str(&mail_doc()).unwrap();
    let (status, res) = op(&base, "agent_set", json!({ "manifest": as_object })).await;
    assert_eq!(status, 200, "{res}");
    let warned = res["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w.as_str().unwrap().contains("JSON string"));
    assert!(warned, "the object form did not say what it cost: {res}");

    // ... and it really did re-sort, which is why the warning exists.
    let detail = get_json(&base, "/api/agents/mail-tagger").await;
    let names: Vec<&str> = detail["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "categories",
            "label_prefix",
            "limit",
            "model",
            "webhook_token"
        ]
    );

    // The string form of the very same document keeps the author's order and
    // earns no warning.
    let (status, res) = op(&base, "agent_set", json!({ "manifest": mail_doc() })).await;
    assert_eq!(status, 200, "{res}");
    assert!(
        !res["warnings"].to_string().contains("JSON string"),
        "the string form was warned at: {res}"
    );
    let detail = get_json(&base, "/api/agents/mail-tagger").await;
    let names: Vec<&str> = detail["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "model",
            "categories",
            "label_prefix",
            "webhook_token",
            "limit"
        ]
    );
}

#[tokio::test]
async fn an_existing_id_is_refused_unless_replace_and_replacing_keeps_the_config() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mail_doc()).await;

    // Somebody tuned the taxonomy.
    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger",
                "values": { "model": "local-a", "categories": ["Work", "Bills"], "limit": 7 } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    // The op answers with what is now stored, not with what was there before.
    assert_eq!(res["config"]["limit"], json!(7), "{res}");
    assert_eq!(
        res["config"]["categories"],
        json!(["Work", "Bills"]),
        "{res}"
    );

    // A second import of the same id is refused by default, naming the flag.
    let (status, err) = import(&base, "", &mail_doc()).await;
    assert_eq!(status, 400);
    assert!(
        err["message"].as_str().unwrap().contains("replace=1"),
        "{err}"
    );

    // With the flag: the manifest is updated, the config survives.
    let updated = mail_doc().replace("\"2.0.0\"", "\"2.1.0\"");
    let (status, report) = import(&base, "?replace=1", &updated).await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["replaced"], json!(true));

    let detail = get_json(&base, "/api/agents/mail-tagger").await;
    assert_eq!(detail["version"], json!("2.1.0"));
    assert_eq!(detail["config"]["categories"], json!(["Work", "Bills"]));
    assert_eq!(detail["config"]["limit"], json!(7));
    assert_eq!(detail["effective_model"], json!("local-a"));
}

#[tokio::test]
async fn a_manifest_that_does_not_validate_is_a_400_naming_the_field() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    let bad = mail_doc().replace("{{config.model}}", "{{config.mdl}}");
    let (status, err) = import(&base, "", &bad).await;
    assert_eq!(status, 400);
    let msg = err["message"].as_str().unwrap();
    assert!(msg.contains("model.alias"), "{msg}");
    assert!(msg.contains("config.mdl"), "{msg}");

    let (status, err) = import(&base, "", r#"{"schema_version": 9, "id": "x"}"#).await;
    assert_eq!(status, 400);
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("schema_version 9"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Export (§5, §8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_secret_never_appears_in_an_export_with_or_without_include_config() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mail_doc()).await;
    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger",
                "values": { "model": "local-a", "webhook_token": "hunter2" } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");

    for query in ["", "?include_config=1"] {
        let (status, body) = get(&base, &format!("/api/agents/mail-tagger/export{query}")).await;
        assert_eq!(status, 200, "{body}");
        assert!(!body.contains("hunter2"), "the secret leaked: {body}");
        let doc: Value = serde_json::from_str(&body).unwrap();
        // The export says what it left out, so the receiver knows what to fill in.
        assert_eq!(doc["config_omitted"], json!(["webhook_token"]));
        assert!(doc["exported_at"].is_string());
        assert!(doc["lmgw_version"].is_string());
    }

    // Config is deployment state: out by default, in on request, minus secrets.
    let plain = get_json(&base, "/api/agents/mail-tagger/export").await;
    assert!(plain.get("config_values").is_none(), "{plain}");
    let full = get_json(&base, "/api/agents/mail-tagger/export?include_config=1").await;
    assert_eq!(full["config_values"]["model"], json!("local-a"));
    assert!(
        full["config_values"].get("webhook_token").is_none(),
        "{full}"
    );

    // And the read path masks it rather than returning it.
    let detail = get_json(&base, "/api/agents/mail-tagger").await;
    assert_eq!(
        detail["config"]["webhook_token"],
        json!({ "has_value": true })
    );
    assert!(!detail.to_string().contains("hunter2"));
    let f = detail["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("webhook_token"))
        .unwrap();
    assert_eq!(f["format"], json!("secret"));
    assert_eq!(f["has_value"], json!(true));
}

/// §8: a round trip is byte-stable modulo `exported_at`. The export is a
/// downloadable file, so "download, hand it to someone else, they import it"
/// has to land on exactly the same agent.
#[tokio::test]
async fn an_exported_agent_round_trips_byte_for_byte() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mail_doc()).await;

    let (_, first) = get(&base, "/api/agents/mail-tagger/export?include_config=1").await;
    // The downloaded file goes straight back in — the envelope keys an export
    // adds must not be mistaken for unknown manifest fields.
    let (status, report) = import(&base, "?replace=1", &first).await;
    assert_eq!(status, 200, "{report}");
    let (_, second) = get(&base, "/api/agents/mail-tagger/export?include_config=1").await;

    // Textually, not through a `Value`: re-serializing both sides would sort
    // both sides the same way and hide exactly the drift this asserts against.
    let scrub = |s: &str| {
        s.lines()
            .filter(|l| !l.contains("\"exported_at\""))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(scrub(&first), scrub(&second));
    // ... and the only difference is the timestamp.
    assert_ne!(first.len() + second.len(), 0);
    let a: Value = serde_json::from_str(&first).unwrap();
    let b: Value = serde_json::from_str(&second).unwrap();
    assert!(a["exported_at"].is_string() && b["exported_at"].is_string());
}

/// The export document is not a manifest — it carries an envelope. A file with
/// `config_values` seeds a *new* agent's config; a replace keeps what is stored.
#[tokio::test]
async fn an_exported_config_seeds_a_new_agent_but_not_a_replaced_one() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mail_doc()).await;
    op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger", "values": { "model": "local-a", "limit": 9 } }),
    )
    .await;
    let (_, file) = get(&base, "/api/agents/mail-tagger/export?include_config=1").await;

    // Same file, new id: the config comes with it.
    let moved = file.replace("\"mail-tagger\"", "\"mail-tagger-2\"");
    let (status, report) = import(&base, "", &moved).await;
    assert_eq!(status, 200, "{report}");
    let detail = get_json(&base, "/api/agents/mail-tagger-2").await;
    assert_eq!(detail["config"]["limit"], json!(9));

    // Replacing the original with a file whose config differs keeps the stored
    // one — a manifest update is not a reason to lose the taxonomy — and says
    // which of the file's values it therefore ignored.
    let other = file.replace("\"limit\": 9", "\"limit\": 1");
    let (status, report) = import(&base, "?replace=1", &other).await;
    assert_eq!(status, 200, "{report}");
    let ignored = report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w.as_str().unwrap().contains("config_values"))
        .unwrap_or_else(|| panic!("no ignored-config warning: {report}"))
        .as_str()
        .unwrap()
        .to_string();
    assert!(ignored.contains("limit"), "{ignored}");
    assert!(ignored.contains("model"), "{ignored}");
    let detail = get_json(&base, "/api/agents/mail-tagger").await;
    assert_eq!(detail["config"]["limit"], json!(9));
}

// ---------------------------------------------------------------------------
// Reads and ops (§5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_five_reads_answer_and_an_unknown_id_is_a_404() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mail_doc()).await;

    let detail = get_json(&base, "/api/agents/mail-tagger").await;
    assert_eq!(detail["kind"], json!("batch"));
    // The manifest comes back as canonical *text*, not as a JSON tree: a tree
    // would have been through a `BTreeMap` and lost the form's field order.
    let manifest_text = detail["manifest"]
        .as_str()
        .unwrap_or_else(|| panic!("manifest is not a string: {detail}"))
        .to_string();
    let parsed: Value = serde_json::from_str(&manifest_text).unwrap();
    assert_eq!(parsed["id"], json!("mail-tagger"));
    assert!(
        manifest_text.find("\"webhook_token\"").unwrap() < manifest_text.find("\"limit\"").unwrap(),
        "the detail manifest alphabetized the config properties:\n{manifest_text}"
    );
    // The two budget values a turn runs under, from Settings → Agents & tools.
    assert!(detail["budget"]["max_tool_calls"].as_u64().unwrap() > 0);
    assert!(detail["budget"]["timeout_seconds"].as_u64().unwrap() > 0);
    assert_eq!(detail["live_job"], json!(null));
    // The config form, in document order, with the manifest's own metadata.
    let names: Vec<&str> = detail["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "model",
            "categories",
            "label_prefix",
            "webhook_token",
            "limit"
        ]
    );

    // No runs yet, and no such run.
    assert_eq!(
        get_json(&base, "/api/agents/mail-tagger/runs").await,
        json!([])
    );
    let (status, _) = get(&base, "/api/agents/runs/9999").await;
    assert_eq!(status, 404);

    for path in ["/api/agents/nope", "/api/agents/nope/export"] {
        let (status, body) = get(&base, path).await;
        assert_eq!(status, 404, "GET {path}: {body}");
        assert!(body.contains("no agent with id 'nope'"), "{body}");
    }
}

#[tokio::test]
async fn enable_duplicate_and_delete() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mail_doc()).await;
    op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger",
                "values": { "model": "local-a", "webhook_token": "hunter2" } }),
    )
    .await;

    let (status, res) = op(
        &base,
        "agent_enable",
        json!({ "id": "mail-tagger", "enabled": false }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let cards = get_json(&base, "/api/agents").await;
    assert_eq!(
        card(&cards, "mail-tagger").unwrap()["enabled"],
        json!(false)
    );

    // A duplicate copies the config **minus secrets** and says which it dropped.
    let (status, res) = op(
        &base,
        "agent_duplicate",
        json!({ "id": "mail-tagger", "new_id": "mail-tagger-work" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["config_omitted"], json!(["webhook_token"]));
    let copy = get_json(&base, "/api/agents/mail-tagger-work").await;
    assert_eq!(copy["name"], json!("Mail tagger (copy)"));
    assert_eq!(copy["source"], json!("authored"));
    assert_eq!(copy["config"]["model"], json!("local-a"));
    assert_eq!(
        copy["config"]["webhook_token"],
        json!({ "has_value": false })
    );

    // A duplicate onto a taken id is refused, and so is a malformed one.
    let (status, err) = op(
        &base,
        "agent_duplicate",
        json!({ "id": "mail-tagger", "new_id": "mail-tagger-work" }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(err["message"].as_str().unwrap().contains("already exists"));
    let (status, err) = op(
        &base,
        "agent_duplicate",
        json!({ "id": "mail-tagger", "new_id": "Mail Tagger" }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        err["message"].as_str().unwrap().contains("lowercase"),
        "{err}"
    );

    let (status, res) = op(&base, "agent_delete", json!({ "id": "mail-tagger" })).await;
    assert_eq!(status, 200, "{res}");
    let (status, _) = get(&base, "/api/agents/mail-tagger").await;
    assert_eq!(status, 404);
    let (status, err) = op(&base, "agent_delete", json!({ "id": "mail-tagger" })).await;
    assert_eq!(status, 400);
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("no agent with id"));
}

#[tokio::test]
async fn config_values_are_validated_against_the_schema_with_the_field_named() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    import(&base, "", &mail_doc()).await;

    for (values, needle) in [
        (json!({ "limit": "50" }), "'limit': expected integer"),
        (json!({ "limit": 0 }), "below the minimum"),
        (json!({ "categories": "Work, Bills" }), "array of strings"),
        (json!({ "nope": 1 }), "not a config field"),
    ] {
        let (status, err) = op(
            &base,
            "agent_config_set",
            json!({ "id": "mail-tagger", "values": values }),
        )
        .await;
        assert_eq!(status, 400, "{err}");
        assert!(
            err["message"].as_str().unwrap().contains(needle),
            "'{needle}' not in {err}"
        );
    }

    // The house convention for tokens: non-empty replaces, empty keeps. Read
    // the stored row directly — the API can only ever show the mask.
    let stored = |s: SharedState| async move {
        let row = store::get_agent(&s.db, "mail-tagger")
            .await
            .unwrap()
            .unwrap();
        serde_json::from_str::<Value>(&row.config).unwrap()
    };
    op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger", "values": { "model": "m", "webhook_token": "first" } }),
    )
    .await;
    assert_eq!(stored(state.clone()).await["webhook_token"], json!("first"));
    op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger", "values": { "webhook_token": "" } }),
    )
    .await;
    assert_eq!(stored(state.clone()).await["webhook_token"], json!("first"));
    op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger", "values": { "webhook_token": "second" } }),
    )
    .await;
    assert_eq!(stored(state).await["webhook_token"], json!("second"));
}

#[tokio::test]
async fn an_unknown_agent_op_is_named() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    let (status, err) = op(&base, "agent_nonsense", json!({})).await;
    assert_eq!(status, 400);
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("unknown op 'agent_nonsense'"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// The seed (§3, §8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_shipped_agents_are_seeded_once_and_a_deleted_one_stays_deleted() {
    let state = AppState::init_for_tests().await.unwrap();
    let shipped: Vec<String> = seed::shipped_all().into_iter().map(|m| m.id).collect();
    assert!(!shipped.is_empty(), "nothing ships");

    // `AppState::init*` already seeded; every shipped id is in the catalog and
    // marked as ours.
    for id in &shipped {
        let row = store::get_agent(&state.db, id).await.unwrap().unwrap();
        assert_eq!(row.source, store::AGENT_SOURCE_BUILTIN);
    }

    // Idempotent across restarts: a second seed inserts nothing.
    let report = seed::seed(&state).await;
    assert!(report.inserted.is_empty(), "{report:?}");
    assert_eq!(report.skipped, shipped);

    // A deleted built-in is not resurrected on the next start.
    let victim = shipped[0].clone();
    store::delete_agent(&state.db, &victim).await.unwrap();
    seed::seed(&state).await;
    assert!(store::get_agent(&state.db, &victim)
        .await
        .unwrap()
        .is_none());

    // "Restore shipped agents" is the deliberate way back.
    let base = serve(state.clone()).await;
    let (status, res) = op(&base, "agents_restore", json!({})).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["restored"], json!([victim]));
    assert!(store::get_agent(&state.db, &victim)
        .await
        .unwrap()
        .is_some());
    // ... and it is still once-only afterwards.
    let (_, res) = op(&base, "agents_restore", json!({})).await;
    assert_eq!(res["restored"], json!([]));
}

/// Editing a shipped agent keeps `source = 'builtin'`, which is what makes
/// "Reset to shipped" possible; the reset keeps the config.
#[tokio::test]
async fn a_built_in_can_be_edited_and_reset_while_keeping_its_config() {
    let state = AppState::init_for_tests().await.unwrap();
    let id = seed::shipped_all()[0].id.clone();
    let base = serve(state.clone()).await;

    let detail = get_json(&base, &format!("/api/agents/{id}")).await;
    assert_eq!(detail["source"], json!("builtin"));
    assert_eq!(detail["resettable"], json!(true));

    let model_field = detail["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["format"] == json!("model_alias"))
        .unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    let mut values = serde_json::Map::new();
    values.insert(model_field.clone(), json!("local-a"));
    op(
        &base,
        "agent_config_set",
        json!({ "id": id, "values": Value::Object(values) }),
    )
    .await;

    // The editor's path: the manifest is text in, text out, so nothing re-sorts
    // on the way through and the save earns no warning.
    let mut manifest: Value = serde_json::from_str(detail["manifest"].as_str().unwrap()).unwrap();
    manifest["description"] = json!("edited by hand");
    let (status, res) = op(
        &base,
        "agent_set",
        json!({ "manifest": serde_json::to_string(&manifest).unwrap() }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["warnings"], json!([]), "{res}");
    let edited = get_json(&base, &format!("/api/agents/{id}")).await;
    assert_eq!(edited["description"], json!("edited by hand"));
    assert_eq!(
        edited["source"],
        json!("builtin"),
        "an edit changed the source"
    );

    let (status, res) = op(&base, "agent_reset", json!({ "id": id })).await;
    assert_eq!(status, 200, "{res}");
    let reset = get_json(&base, &format!("/api/agents/{id}")).await;
    assert_ne!(reset["description"], json!("edited by hand"));
    assert_eq!(reset["config"][&model_field], json!("local-a"));

    // An agent with no shipped original says so instead of pretending.
    import(&base, "", &mail_doc()).await;
    let (status, err) = op(&base, "agent_reset", json!({ "id": "mail-tagger" })).await;
    assert_eq!(status, 400);
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("not a shipped agent"),
        "{err}"
    );
}

/// §2.6: the config form is rendered "in document order" — the author put
/// `model` before `categories` on purpose. `serde_json::Map` is a `BTreeMap` in
/// this build, so any path that carries the manifest through a `Value`
/// alphabetizes the properties; that is the whole reason the manifest holds an
/// order-preserving map, and the export/import pair has to honour it too.
#[tokio::test]
async fn an_export_import_cycle_keeps_the_config_form_in_document_order() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mail_doc()).await;
    let authored = [
        "model",
        "categories",
        "label_prefix",
        "webhook_token",
        "limit",
    ];
    let names = |d: &Value| -> Vec<String> {
        d["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_string())
            .collect()
    };
    let detail = get_json(&base, "/api/agents/mail-tagger").await;
    assert_eq!(names(&detail), authored);

    // The downloaded file itself: `limit` is authored last, after the secret.
    let (_, file) = get(&base, "/api/agents/mail-tagger/export").await;
    assert!(
        file.find("\"webhook_token\"").unwrap() < file.find("\"limit\"").unwrap(),
        "the export alphabetized the config properties:\n{file}"
    );

    // ... and back in again.
    let (status, report) = import(&base, "?replace=1", &file).await;
    assert_eq!(status, 200, "{report}");
    let detail = get_json(&base, "/api/agents/mail-tagger").await;
    assert_eq!(
        names(&detail),
        authored,
        "an export/import cycle re-sorted the form"
    );
}

/// Replacing a manifest keeps the stored config (§5) — including the value of
/// a field the new manifest no longer declares. If that field *was* the secret,
/// nothing in the column marks it as one any more, and an export with
/// `include_config=1` would write the credential out in the clear.
#[tokio::test]
async fn a_secret_left_behind_by_a_manifest_edit_still_never_leaves() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mail_doc()).await;
    op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger",
                "values": { "model": "local-a", "webhook_token": "hunter2" } }),
    )
    .await;

    // The field is dropped from the schema; the value stays in the column.
    let narrowed = mail_doc().replace(
        r#""webhook_token": { "type": "string", "format": "secret" },"#,
        "",
    );
    let (status, report) = import(&base, "?replace=1", &narrowed).await;
    assert_eq!(status, 200, "{report}");

    let (status, body) = get(&base, "/api/agents/mail-tagger/export?include_config=1").await;
    assert_eq!(status, 200, "{body}");
    assert!(
        !body.contains("hunter2"),
        "the orphaned secret leaked: {body}"
    );

    // Same for a copy: a duplicate must not inherit it either.
    let (status, res) = op(
        &base,
        "agent_duplicate",
        json!({ "id": "mail-tagger", "new_id": "mail-tagger-copy" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let copy = get_json(&base, "/api/agents/mail-tagger-copy").await;
    assert!(!copy.to_string().contains("hunter2"), "{copy}");
}

// ---------------------------------------------------------------------------
// The `chat` run kind (§2.5) — `agent_open_chat`
// ---------------------------------------------------------------------------

/// The shipped `chat` agent — the kind this section is about, and the only one
/// with a thread to open. The other built-in, `mail-labeler`, is a `batch`
/// agent: its run shape is driven in `agents/batch.rs`'s own tests.
const CHAT_AGENT: &str = "docs-librarian";

fn local(model_id: &str) -> store::NewLocalModel {
    store::NewLocalModel {
        model_id: model_id.into(),
        gguf_path: format!("{model_id}.gguf"),
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
        capabilities_override: None,
        ladder: vec![],
    }
}

/// The whole of the `chat` kind: the config form gates it, the thread is seeded
/// from the manifest in one write, and it lists as the agent's.
#[tokio::test]
async fn opening_a_chat_agent_seeds_one_thread_from_its_manifest() {
    let state = AppState::init_for_tests().await.unwrap();
    store::insert_local_model(&state.db, &local("gemma4-12b"))
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;

    // A required config field nobody has filled in refuses, naming the field:
    // the alias is `{{config.model}}`, so the thread would point at nothing.
    let (status, err) = op(&base, "agent_open_chat", json!({ "id": CHAT_AGENT })).await;
    assert_eq!(status, 400, "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("'model' is required"),
        "{err}"
    );

    op(
        &base,
        "agent_config_set",
        json!({ "id": CHAT_AGENT,
                "values": { "model": "gemma4-12b", "house_style": "Prefer Podman over Docker." } }),
    )
    .await;

    let (status, res) = op(&base, "agent_open_chat", json!({ "id": CHAT_AGENT })).await;
    assert_eq!(status, 200, "{res}");
    let thread_id = res["thread_id"].as_i64().unwrap();
    assert_eq!(res["model"], json!("gemma4-12b"));
    assert_eq!(res["url"], json!(format!("/chat?t={thread_id}")));
    // The alias resolves on this gateway and the manifest sets nothing a thread
    // cannot carry, so there is nothing to warn about.
    assert_eq!(res["warnings"], json!([]), "{res}");

    let t = store::get_chat_thread(&state.db, thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(t.agent_id.as_deref(), Some(CHAT_AGENT));
    assert_eq!(t.model_alias, "gemma4-12b");
    assert_eq!(t.kind, "chat");
    // The system prompt arrives rendered — the config is substituted here, once,
    // not left as a template for the chat send to trip over.
    assert!(
        t.system_prompt.contains("Prefer Podman over Docker."),
        "config was not rendered into the prompt: {}",
        t.system_prompt
    );
    assert!(!t.system_prompt.contains("{{config"), "{}", t.system_prompt);
    assert_eq!(
        t.mcp_tools
            .iter()
            .map(|m| m.server_label.clone())
            .collect::<Vec<_>>(),
        vec!["docs".to_string()],
    );
    // Left at the table default so the first message still names it.
    assert_eq!(t.title, "New chat");

    // It counts as the agent's, on the card and on the detail.
    let detail = get_json(&base, &format!("/api/agents/{CHAT_AGENT}")).await;
    assert_eq!(detail["threads"], json!(1));
    assert_eq!(
        card(&get_json(&base, "/api/agents").await, CHAT_AGENT).unwrap()["threads"],
        json!(1)
    );
    // And it is an ordinary thread on the Chat page, which is what the agent's
    // Threads tab filters.
    // `GET /chat/api/threads` is `{threads, archived_count}`, not a bare array
    // (chat-archive-pin-attachments design §1) — the wrapper is what lets the
    // sidebar's "Archived (n)" toggle label itself without a second request.
    let threads = get_json(&base, "/chat/api/threads").await;
    assert_eq!(threads["threads"][0]["id"], json!(thread_id));
    assert_eq!(threads["threads"][0]["agent_id"], json!(CHAT_AGENT));
}

/// The Run tab's form is what a thread opens with, and Save config is what
/// sets the default it falls back to — two acts, deliberately separate.
///
/// Picking a different model for one conversation is an ordinary thing to
/// want, and it must not rewrite the agent to get it. So `values` rides along
/// with the open, is merged over what is stored for that thread only, and the
/// catalog row is left exactly as it was.
#[tokio::test]
async fn a_thread_opens_with_the_form_and_leaves_the_saved_default_alone() {
    let state = AppState::init_for_tests().await.unwrap();
    for id in ["gemma4-12b", "qwen3.8"] {
        store::insert_local_model(&state.db, &local(id))
            .await
            .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let base = serve(state.clone()).await;

    op(
        &base,
        "agent_config_set",
        json!({ "id": CHAT_AGENT,
                "values": { "model": "gemma4-12b", "house_style": "Prefer Podman." } }),
    )
    .await;

    // A model picked in the form and never saved is what the thread opens with.
    let (status, res) = op(
        &base,
        "agent_open_chat",
        json!({ "id": CHAT_AGENT, "values": { "model": "qwen3.8" } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["model"], json!("qwen3.8"), "{res}");
    let t = store::get_chat_thread(&state.db, res["thread_id"].as_i64().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(t.model_alias, "qwen3.8");
    // Sparse: the field the form did not have to re-send is still the stored
    // one, in the thread's prompt as well as in the catalog.
    assert!(
        t.system_prompt.contains("Prefer Podman."),
        "{}",
        t.system_prompt
    );

    // And the agent's own default is untouched — the next open with no values
    // is still the saved model.
    let detail = get_json(&base, &format!("/api/agents/{CHAT_AGENT}")).await;
    assert_eq!(detail["config"]["model"], json!("gemma4-12b"), "{detail}");
    let (status, res) = op(&base, "agent_open_chat", json!({ "id": CHAT_AGENT })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["model"], json!("gemma4-12b"), "{res}");

    // A form that blanks a required field is refused naming it, exactly as an
    // unconfigured agent is: the values are validated as the complete config
    // they are about to be run as, not as a patch.
    let (status, err) = op(
        &base,
        "agent_open_chat",
        json!({ "id": CHAT_AGENT, "values": { "model": "" } }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("'model' is required"),
        "{err}"
    );
    // A name the schema does not declare is a mistake worth naming, not a
    // value to carry into a run.
    let (status, err) = op(
        &base,
        "agent_open_chat",
        json!({ "id": CHAT_AGENT, "values": { "modle": "qwen3.8" } }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("not a config field"),
        "{err}"
    );
}

/// A model this gateway does not serve **warns** and opens anyway — the same
/// rule §5 applies to a missing MCP server. A wrong run kind and a disabled
/// agent refuse, because neither can produce a usable thread.
#[tokio::test]
async fn open_chat_warns_about_an_unserved_model_and_refuses_what_cannot_work() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;

    op(
        &base,
        "agent_config_set",
        json!({ "id": CHAT_AGENT, "values": { "model": "not-served-here" } }),
    )
    .await;
    let (status, res) = op(&base, "agent_open_chat", json!({ "id": CHAT_AGENT })).await;
    assert_eq!(status, 200, "{res}");
    let warnings = res["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{res}");
    assert!(
        warnings[0].as_str().unwrap().contains("not-served-here"),
        "{res}"
    );

    // A batch agent has no thread to open.
    import(&base, "", &mail_doc()).await;
    let (status, err) = op(&base, "agent_open_chat", json!({ "id": "mail-tagger" })).await;
    assert_eq!(status, 400, "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("is a batch agent"),
        "{err}"
    );

    // A disabled agent does not run, and opening a thread is running it.
    op(
        &base,
        "agent_enable",
        json!({ "id": CHAT_AGENT, "enabled": false }),
    )
    .await;
    let (status, err) = op(&base, "agent_open_chat", json!({ "id": CHAT_AGENT })).await;
    assert_eq!(status, 400, "{err}");
    assert!(
        err["message"].as_str().unwrap().contains("disabled"),
        "{err}"
    );

    let (status, err) = op(&base, "agent_open_chat", json!({ "id": "nope" })).await;
    assert_eq!(status, 400, "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("no agent with id 'nope'"),
        "{err}"
    );
}

/// A `chat_threads` row carries the alias, the prompt, the tools and the
/// temperature. A manifest that sets sampling knobs the row has no column for
/// is told, once, rather than leaving the author to wonder why `seed` did
/// nothing.
#[tokio::test]
async fn open_chat_names_the_manifest_settings_a_thread_cannot_carry() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    let doc = r#"{
  "schema_version": 1,
  "id": "sampler",
  "name": "Sampler",
  "description": "",
  "model": { "alias": "some-model", "temperature": 0.2, "top_p": 0.9, "seed": 7 },
  "run": { "kind": "chat", "system": "Be brief." }
}"#;
    let (status, res) = import(&base, "", doc).await;
    assert_eq!(status, 200, "{res}");

    let (status, res) = op(&base, "agent_open_chat", json!({ "id": "sampler" })).await;
    assert_eq!(status, 200, "{res}");
    let warnings: Vec<String> = res["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect();
    let dropped = warnings
        .iter()
        .find(|w| w.contains("top_p"))
        .unwrap_or_else(|| panic!("{warnings:?}"));
    assert!(dropped.contains("seed"), "{dropped}");
    // Temperature is the one the table does hold, so it is not in the list —
    // and it is actually on the row.
    assert!(!dropped.contains("temperature · "), "{dropped}");
}

/// Deleting an agent keeps its conversations. `chat_threads.agent_id` has no
/// foreign key and the design does not say what happens here, so the rule is
/// the one the rest of the app follows for user data: a conversation is what
/// was said, the agent is only the preset it was said through.
#[tokio::test]
async fn deleting_an_agent_keeps_its_threads_and_unlinks_them() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    op(
        &base,
        "agent_config_set",
        json!({ "id": CHAT_AGENT, "values": { "model": "whatever" } }),
    )
    .await;
    let (_, res) = op(&base, "agent_open_chat", json!({ "id": CHAT_AGENT })).await;
    let thread_id = res["thread_id"].as_i64().unwrap();

    let (status, res) = op(&base, "agent_delete", json!({ "id": CHAT_AGENT })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["threads_kept"], json!(1));
    assert!(
        res["message"].as_str().unwrap().contains("kept"),
        "the op has to say the thread survived: {res}"
    );

    let t = store::get_chat_thread(&state.db, thread_id)
        .await
        .unwrap()
        .expect("the thread was deleted with its agent");
    assert_eq!(t.agent_id, None, "the dead agent is still claimed");
    assert_eq!(
        store::count_chat_threads_by_agent(&state.db, CHAT_AGENT)
            .await
            .unwrap(),
        0
    );
}

/// A chat agent with a `secret` config field: the value is stored, masked on
/// read, and — the point of this test — never reaches the thread the agent
/// opens. `chat_threads.system_prompt` is persisted in the clear and read back
/// by the Chat page, so a token that got in there would be unrecoverable.
/// The manifest rule (a secret may not appear in a prompt template) is what
/// makes this hold; this proves the whole path, not just the rule.
#[tokio::test]
async fn a_secret_never_reaches_the_thread_a_chat_agent_opens() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    let doc = r#"{
  "schema_version": 1,
  "id": "keyed",
  "name": "Keyed",
  "description": "",
  "model": { "alias": "{{config.model}}" },
  "config": { "schema": { "type": "object", "properties": {
      "model": { "type": "string", "format": "model_alias" },
      "api_token": { "type": "string", "format": "secret" }
  }, "required": ["model"] } },
  "tools": [],
  "run": { "kind": "chat", "system": "You are helpful." }
}"#;
    let (status, res) = import(&base, "", doc).await;
    assert_eq!(status, 200, "{res}");

    // The same manifest with the token in its prompt does not import at all.
    let refused = doc.replace("You are helpful.", "Key: {{config.api_token}}");
    let (status, err) = import(&base, "", &refused).await;
    assert_eq!(status, 400, "{err}");
    let msg = err["message"].as_str().unwrap();
    assert!(
        msg.contains("run.system") && msg.contains("api_token"),
        "{msg}"
    );

    op(
        &base,
        "agent_config_set",
        json!({ "id": "keyed", "values": { "model": "gemma4-12b", "api_token": "hunter2" } }),
    )
    .await;
    let (status, res) = op(&base, "agent_open_chat", json!({ "id": "keyed" })).await;
    assert_eq!(status, 200, "{res}");
    assert!(!res.to_string().contains("hunter2"), "{res}");

    let t = store::get_chat_thread(&state.db, res["thread_id"].as_i64().unwrap())
        .await
        .unwrap()
        .unwrap();
    let row = serde_json::to_string(&t).unwrap();
    assert!(
        !row.contains("hunter2"),
        "the secret is in the thread: {row}"
    );
    let threads = get_json(&base, "/chat/api/threads").await;
    assert!(!threads.to_string().contains("hunter2"), "{threads}");
}

/// The tool gap §5 names is a warning here too: the thread opens, and says
/// that what it attached will not resolve until the server is registered.
#[tokio::test]
async fn open_chat_warns_when_the_agents_mcp_server_is_not_registered() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    let doc = r#"{
  "schema_version": 1,
  "id": "filer",
  "name": "Filer",
  "description": "",
  "model": { "alias": "some-model" },
  "tools": [ { "label": "gws", "allowed": ["gws__gmail_list"] } ],
  "run": { "kind": "chat", "system": "File things." }
}"#;
    let (status, res) = import(&base, "", doc).await;
    assert_eq!(status, 200, "{res}");

    let (status, res) = op(&base, "agent_open_chat", json!({ "id": "filer" })).await;
    assert_eq!(status, 200, "{res}");
    let warnings: Vec<String> = res["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect();
    assert!(
        warnings.iter().any(|w| w.contains("gws")),
        "the missing server has to be said: {warnings:?}"
    );
    // A warning, not a refusal: the thread exists and still carries the label,
    // so registering the server later makes it work without reopening.
    assert!(res["thread_id"].as_i64().is_some(), "{res}");

    // The label that *is* registered says nothing.
    op(
        &base,
        "agent_config_set",
        json!({ "id": CHAT_AGENT, "values": { "model": "m" } }),
    )
    .await;
    let (_, res) = op(&base, "agent_open_chat", json!({ "id": CHAT_AGENT })).await;
    let quiet: Vec<String> = res["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect();
    assert!(!quiet.iter().any(|w| w.contains("docs")), "{quiet:?}");
}

/// `clear` is the way out of the "empty keeps the stored secret" convention:
/// it names the fields to forget, refuses a name the schema does not have, and
/// refuses to leave a required field with nothing in it.
#[tokio::test]
async fn agent_config_set_clears_named_fields_and_refuses_the_rest() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mail_doc()).await;
    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger",
                "values": { "model": "local-a", "webhook_token": "hunter2" } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["config"]["webhook_token"], json!({ "has_value": true }));

    // An empty submission keeps it — the convention `clear` exists to escape.
    let (_, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger", "values": { "webhook_token": "" } }),
    )
    .await;
    assert_eq!(res["config"]["webhook_token"], json!({ "has_value": true }));

    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger", "values": {}, "clear": ["webhook_token"] }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(
        res["config"]["webhook_token"],
        json!({ "has_value": false })
    );
    let detail = get_json(&base, "/api/agents/mail-tagger").await;
    assert_eq!(
        detail["config"]["webhook_token"],
        json!({ "has_value": false })
    );
    assert!(!detail.to_string().contains("hunter2"), "{detail}");

    // Clear wins over a value submitted in the same call, so the two orders of
    // "type a new one, then tick clear" agree.
    op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger",
                "values": { "webhook_token": "again" }, "clear": ["webhook_token"] }),
    )
    .await;
    let detail = get_json(&base, "/api/agents/mail-tagger").await;
    assert!(!detail.to_string().contains("again"), "{detail}");

    // A name the schema does not have is **cleared and named**, not refused
    // (container-runtime §5.1's pruning rule, WP5 review): a value orphaned by
    // a manifest that dropped its field is exactly what has to be removable,
    // and `clear` is the only gesture that can remove it — `values` merges.
    // Silent is the one thing it must not be, because a typo looks identical.
    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger", "values": {}, "clear": ["nope"] }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["cleared_undeclared"], json!(["nope"]), "{res}");
    assert!(
        res["message"].as_str().unwrap().contains("nope"),
        "the answer has to name it: {res}"
    );

    // Clearing a required field that has a schema default is fine — the
    // default is what the run would use anyway.
    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger", "values": {}, "clear": ["model"] }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let detail = get_json(&base, "/api/agents/mail-tagger").await;
    assert!(detail["config"].get("model").is_none(), "{detail}");

    // Clearing a required field with nothing to fall back on fails the same
    // way a missing required value does, naming the field.
    op(
        &base,
        "agent_config_set",
        json!({ "id": CHAT_AGENT, "values": { "model": "m" } }),
    )
    .await;
    let (status, err) = op(
        &base,
        "agent_config_set",
        json!({ "id": CHAT_AGENT, "values": {}, "clear": ["model"] }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("'model' is required"),
        "{err}"
    );

    let (status, err) = op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger", "values": {}, "clear": "webhook_token" }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert!(err["message"].as_str().unwrap().contains("array"), "{err}");
}

// ---------------------------------------------------------------------------
// A replace that de-secrets a field (container-runtime §5.1, final review)
// ---------------------------------------------------------------------------

/// Pruning by name alone kept the stored value of a field that stopped saying
/// `format: "secret"` — and every reader decides what to mask by asking the
/// *current* schema, so the next page load, `lmgw__agent_get`, an export with
/// config and the container's `input.json` would all print a credential that
/// was typed into a masked field. A replace is not a reveal.
#[tokio::test]
async fn a_replace_that_de_secrets_a_field_drops_the_value_it_had_hidden() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    let (status, res) = import(&base, "", &mail_doc()).await;
    assert_eq!(status, 200, "{res}");

    let (status, saved) = op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger", "values": {
            "webhook_token": "s3cr3t-value",
            "label_prefix": "inbox"
        } }),
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    // Masked while the schema still says it is a secret.
    let d = get_json(&base, "/api/agents/mail-tagger").await;
    assert_eq!(
        d["config"]["webhook_token"],
        json!({ "has_value": true }),
        "{d}"
    );

    // The same manifest, with `format: "secret"` taken off that one field.
    let relaxed = mail_doc().replace(
        r#""webhook_token": { "type": "string", "format": "secret" }"#,
        r#""webhook_token": { "type": "string" }"#,
    );
    assert!(!relaxed.contains("secret"), "the fixture edit has to bite");
    let (status, res) = import(&base, "?replace=1", &relaxed).await;
    assert_eq!(status, 200, "{res}");

    // Reported like any other dropped key — and only that key.
    assert_eq!(res["dropped_config"], json!(["webhook_token"]), "{res}");
    assert!(
        res["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("webhook_token")),
        "{res}"
    );

    // And really gone, not merely re-masked: the field is declared, so nothing
    // downstream would hide it any more.
    let d = get_json(&base, "/api/agents/mail-tagger").await;
    assert!(
        d["config"].get("webhook_token").is_none()
            || d["config"]["webhook_token"] == json!(Value::Null),
        "{d}"
    );
    let stored = store::get_agent(&state.db, "mail-tagger")
        .await
        .unwrap()
        .unwrap();
    assert!(
        !stored.config.contains("s3cr3t-value"),
        "the value is still in the column: {}",
        stored.config
    );
    // The values whose fields did not change are kept — that is still the
    // promise this rule is carved out of.
    assert_eq!(d["config"]["label_prefix"], json!("inbox"), "{d}");
}

/// The other side of the same rule: a field that *stays* secret keeps its
/// value across a replace, which is what "a manifest update is not a reason to
/// lose the taxonomy someone tuned" means.
#[tokio::test]
async fn a_replace_that_leaves_a_secret_field_secret_keeps_its_value() {
    let state = AppState::init_for_tests().await.unwrap();
    let base = serve(state.clone()).await;
    import(&base, "", &mail_doc()).await;
    op(
        &base,
        "agent_config_set",
        json!({ "id": "mail-tagger", "values": { "webhook_token": "s3cr3t-value" } }),
    )
    .await;

    let same_shape = mail_doc().replace(r#""version": "2.0.0""#, r#""version": "2.1.0""#);
    let (status, res) = import(&base, "?replace=1", &same_shape).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["dropped_config"], json!([]), "{res}");
    let d = get_json(&base, "/api/agents/mail-tagger").await;
    assert_eq!(
        d["config"]["webhook_token"],
        json!({ "has_value": true }),
        "{d}"
    );
    let stored = store::get_agent(&state.db, "mail-tagger")
        .await
        .unwrap()
        .unwrap();
    assert!(stored.config.contains("s3cr3t-value"), "{}", stored.config);
}

// ---------------------------------------------------------------------------
// Host mounts (mounts design §5.2, §5.3, §5.9)
// ---------------------------------------------------------------------------

/// A container agent with one required `rw` directory slot and one optional
/// `ro` file slot — the two formats of §5.1, and nothing else to distract.
///
/// No `default` and no `enum` on either: a manifest names a slot, never a host
/// path, and the load-time refusals that enforce it are unit-tested next to the
/// schema.
fn mount_doc(id: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "Notes desk",
  "model": {{ "alias": "m1" }},
  "config": {{ "schema": {{ "type": "object", "properties": {{
    "notes": {{ "type": "string", "format": "directory", "access": "rw",
                "title": "Notes folder" }},
    "key":   {{ "type": "string", "format": "file", "title": "Signing key" }},
    "label_prefix": {{ "type": "string", "default": "ai" }}
  }}, "required": ["notes"] }} }},
  "run": {{ "kind": "container", "image": "ghcr.io/acme/notes:1" }}
}}"#
    )
}

/// A canonical tempdir: `/tmp` is a symlink on some boxes, and every path the
/// rules return has been through `canonicalize`.
fn workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    (tmp, root)
}

#[tokio::test]
async fn a_mount_value_is_stored_canonical_and_the_rules_refuse_by_name() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mount_doc("notes-desk")).await;
    let (_tmp, root) = workspace();
    let notes = root.join("Notes");
    std::fs::create_dir_all(&notes).unwrap();
    let link = root.join("notes-link");
    std::os::unix::fs::symlink(&notes, &link).unwrap();

    // Rule 1, at the store: a relative path is refused naming the field.
    let (status, err) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": "Notes" } }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["code"], json!("mount_path_refused"), "{err}");
    let msg = err["message"].as_str().unwrap();
    assert!(msg.starts_with("notes: "), "{msg}");
    assert!(msg.contains("absolute path"), "{msg}");

    // Rule 4, with the reason in the message.
    let (status, err) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": "/etc" } }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["code"], json!("mount_path_refused"), "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("not a data directory"),
        "{err}"
    );

    // Rule 2: what is stored is the target, not the link.
    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": link.display().to_string() } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(
        res["config"]["notes"],
        json!(notes.display().to_string()),
        "{res}"
    );
    // Rule 3, once the required slot is bound: the kind has to match the
    // format the manifest declared.
    let (status, err) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "key": notes.display().to_string() } }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("a file field names a regular file"),
        "{err}"
    );

    // And the owner reads their own path back (§5.2); the container's view is
    // the Agent one, which is the runtime's half.
    let d = get_json(&base, "/api/agents/notes-desk").await;
    assert_eq!(d["config"]["notes"], json!(notes.display().to_string()));
    let field = d["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("notes"))
        .unwrap();
    assert_eq!(field["format"], json!("directory"), "{field}");
    assert_eq!(field["access"], json!("rw"), "{field}");
}

/// §5.3 rule 5: an `rw` mount is a tree a confined container can rewrite, so
/// nothing else may resolve through it — and the refusal names who holds it.
#[tokio::test]
async fn a_path_nested_in_another_agents_rw_mount_is_refused_naming_it() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mount_doc("notes-desk")).await;
    import(&base, "", &mount_doc("notes-desk-2")).await;
    let (_tmp, root) = workspace();
    let notes = root.join("Notes");
    let daily = notes.join("daily");
    std::fs::create_dir_all(&daily).unwrap();

    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": notes.display().to_string() } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");

    let (status, err) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk-2", "values": { "notes": daily.display().to_string() } }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["code"], json!("mount_path_nested"), "{err}");
    let msg = err["message"].as_str().unwrap();
    assert!(msg.contains("'notes-desk'"), "{msg}");
    assert!(msg.contains("'notes'"), "{msg}");

    // The *same* folder twice is allowed: that is what the shared label is for.
    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk-2", "values": { "notes": notes.display().to_string() } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
}

/// §5.2: the export strips the value and names the slot instead, the file
/// re-imports, and the imported agent says what is left to bind.
#[tokio::test]
async fn an_export_carries_no_mount_value_and_names_the_slot_instead() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mount_doc("notes-desk")).await;
    let (_tmp, root) = workspace();
    let notes = root.join("Notes");
    std::fs::create_dir_all(&notes).unwrap();
    let (status, res) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": notes.display().to_string() } }),
    )
    .await;
    assert_eq!(status, 200, "{res}");

    let (_, file) = get(&base, "/api/agents/notes-desk/export?include_config=1").await;
    assert!(
        !file.contains(&notes.display().to_string()),
        "the host path leaked into the export: {file}"
    );
    let doc: Value = serde_json::from_str(&file).unwrap();
    assert_eq!(doc["config_unbound"], json!(["notes", "key"]), "{doc}");
    assert!(doc["config_values"].get("notes").is_none(), "{doc}");

    // The downloaded file goes straight back in: `config_unbound` is an
    // envelope key, so the manifest parse never sees it.
    let moved = file.replace("\"notes-desk\"", "\"notes-desk-2\"");
    let (status, report) = import(&base, "", &moved).await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(
        report["config_unbound"],
        json!(["notes", "key"]),
        "{report}"
    );

    // The slot arrived unbound, and the required one blocks Start with a name
    // that says what to do about it (§5.9).
    let d = get_json(&base, "/api/agents/notes-desk-2").await;
    assert!(d["config"].get("notes").is_none(), "{d}");
    let warning = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == json!("mount_unbound"))
        .unwrap_or_else(|| panic!("no mount_unbound: {d}"));
    assert_eq!(warning["blocks_start"], json!(true), "{warning}");
    assert!(
        warning["message"]
            .as_str()
            .unwrap()
            .contains("bind 'notes' on the Run tab"),
        "{warning}"
    );
}

/// A save is judged on the slots it **changes** (§5.3, checked per value).
///
/// Checking every mount field of the merged map made one dead folder a wall in
/// front of every other field: the disk holding `notes` is unplugged, and the
/// prefix on an unrelated text field cannot be saved — behind the very refusal
/// whose only fix is a save. The dead mount is still refused where it matters,
/// at the start that would have bound it.
#[tokio::test]
async fn a_dead_mount_does_not_block_a_save_that_does_not_touch_it() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mount_doc("notes-desk")).await;
    let (tmp, root) = workspace();
    let notes = root.join("Notes");
    std::fs::create_dir_all(&notes).unwrap();
    let (status, v) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": notes.display().to_string() } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");

    // The folder goes.
    drop(tmp);

    // An unrelated field still saves, and the dead mount is left as it is.
    let (status, v) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "label_prefix": "inbox" } }),
    )
    .await;
    assert_eq!(status, 200, "a dead mount blocked an unrelated save: {v}");
    assert_eq!(v["config"]["label_prefix"], json!("inbox"), "{v}");
    assert_eq!(
        v["config"]["notes"],
        json!(notes.display().to_string()),
        "the value the owner has to fix is still there to fix: {v}"
    );

    // Re-submitting the dead slot *is* touching it, and says what to do.
    let (status, err) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": notes.display().to_string() } }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["code"], json!("mount_path_refused"), "{err}");
    let message = err["message"].as_str().unwrap();
    assert!(message.starts_with("notes: "), "{message}");
    assert!(
        message.ends_with("clear the field or point it at a folder that exists"),
        "the refusal ends with the way out: {message}"
    );

    // And taking the advice is a save that goes through. (Clearing is the
    // other half of the sentence, and on this manifest `notes` is `required`,
    // so that one is refused by the required rule instead — which is the
    // honest answer to "clear the thing the agent cannot run without".)
    let (_tmp2, root2) = workspace();
    let elsewhere = root2.join("Notes");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let (status, v) = op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": elsewhere.display().to_string() } }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["config"]["label_prefix"], json!("inbox"), "{v}");
}

/// §5.2 again, from the other end: a **hand-made** package can carry a
/// `config_values` naming any path at all, and `validate_values` judges the
/// shape of a value and not where it points.
///
/// `{"notes": "/"}` used to land in the store, and from there into every other
/// agent's rule 5 as a bound path containing the whole box. The store-time
/// rules run on the import too, before anything is written.
#[tokio::test]
async fn an_imported_package_carrying_a_hostile_mount_value_is_refused() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    let hostile =
        mount_doc("notes-desk").replacen('{', "{\n  \"config_values\": { \"notes\": \"/\" },", 1);
    let (status, err) = import(&base, "", &hostile).await;
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["code"], json!("mount_path_refused"), "{err}");
    let message = err["message"].as_str().unwrap();
    assert!(message.contains("notes"), "the field is named: {message}");
    assert!(message.contains("too broad to relabel"), "{message}");

    // Nothing was written: not the config, and not the agent either.
    let (status, body) = get(&base, "/api/agents/notes-desk").await;
    assert_eq!(status, 404, "the half-import left a row behind: {body}");

    // The same file with a folder that is allowed imports and is bound.
    let (_tmp, root) = workspace();
    let notes = root.join("Notes");
    std::fs::create_dir_all(&notes).unwrap();
    let fine = hostile.replace(
        "\"notes\": \"/\"",
        &format!("\"notes\": \"{}\"", notes.display()),
    );
    let (status, report) = import(&base, "", &fine).await;
    assert_eq!(status, 200, "{report}");
    let d = get_json(&base, "/api/agents/notes-desk").await;
    assert_eq!(
        d["config"]["notes"],
        json!(notes.display().to_string()),
        "{d}"
    );
}

/// `agent_duplicate` copies mount values: same box, same paths (§5.2).
#[tokio::test]
async fn duplicating_an_agent_copies_the_folder_it_is_bound_to() {
    let base = serve(AppState::init_for_tests().await.unwrap()).await;
    import(&base, "", &mount_doc("notes-desk")).await;
    let (_tmp, root) = workspace();
    let notes = root.join("Notes");
    std::fs::create_dir_all(&notes).unwrap();
    op(
        &base,
        "agent_config_set",
        json!({ "id": "notes-desk", "values": { "notes": notes.display().to_string() } }),
    )
    .await;

    let (status, res) = op(
        &base,
        "agent_duplicate",
        json!({ "id": "notes-desk", "new_id": "notes-desk-copy" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let d = get_json(&base, "/api/agents/notes-desk-copy").await;
    assert_eq!(d["config"]["notes"], json!(notes.display().to_string()));

    // Copying is a **store** of that value, so it is judged like one: the
    // folder has gone, and the copy would be a row bound to nothing that every
    // other agent's rule 5 would still have to answer for.
    std::fs::remove_dir_all(&notes).unwrap();
    let (status, err) = op(
        &base,
        "agent_duplicate",
        json!({ "id": "notes-desk", "new_id": "notes-desk-second" }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["code"], json!("mount_path_refused"), "{err}");
    let message = err["message"].as_str().unwrap();
    assert!(message.contains("notes: "), "the field is named: {message}");
    assert!(message.contains("does not exist"), "{message}");
    assert!(
        message.contains("duplicate again"),
        "and what the owner does about it: {message}"
    );
    let (status, body) = get(&base, "/api/agents/notes-desk-second").await;
    assert_eq!(status, 404, "a refused duplicate left a row behind: {body}");
}
