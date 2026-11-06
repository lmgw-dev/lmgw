//! Self-admin tools over `/mcp/admin` — the agent catalog over the tool plane.

use super::*;

// ---------------------------------------------------------------------------
// The agent catalog over the tool plane (agent-catalog design §5)
// ---------------------------------------------------------------------------

/// An agent holding the admin token can install an agent — the point of the
/// catalog being data. The read/write split is the usual one: browsing the
/// catalog is a read, installing and deleting are not.
#[tokio::test]
async fn an_agent_can_read_the_catalog_at_read_only_and_install_only_at_full() {
    let manifest = r#"{
      "schema_version": 1,
      "id": "note-taker",
      "name": "Note taker",
      "description": "Files notes into the docs corpora.",
      "model": { "alias": "{{config.model}}" },
      "config": { "schema": { "type": "object", "properties": {
        "model": { "type": "string", "format": "model_alias" }
      }, "required": ["model"] } },
      "tools": [ { "label": "docs" } ],
      "run": { "kind": "chat", "system": "You file notes." }
    }"#;

    // read_only: the two reads are there, the two writes are not.
    let ro = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let ro_sid = initialize(&ro).await;
    let names = list_tool_names(&ro, &ro_sid).await;
    for read in ["lmgw__agents", "lmgw__agent_get"] {
        assert!(names.iter().any(|n| n == read), "missing {read}: {names:?}");
    }
    for write in ["lmgw__agent_set", "lmgw__agent_delete"] {
        assert!(
            !names.iter().any(|n| n == write),
            "{write} leaked into read_only"
        );
    }
    let refused = call(
        &ro,
        &ro_sid,
        "lmgw__agent_set",
        json!({ "manifest": manifest }),
    )
    .await;
    assert!(is_error(&refused), "{refused:?}");
    assert!(
        text_of(&refused).contains("read_only"),
        "{}",
        text_of(&refused)
    );
    // ... and the read really answers, listing the shipped agents.
    let listed = payload(&call(&ro, &ro_sid, "lmgw__agents", json!({})).await);
    assert!(listed["count"].as_u64().unwrap() >= 1, "{listed}");

    // full: validate, install, read back, delete.
    let base = serve_with(state_with_mode(SelfAdmin::Full).await).await;
    let sid = initialize(&base).await;

    let dry = payload(
        &call(
            &base,
            &sid,
            "lmgw__agent_set",
            json!({ "manifest": manifest, "validate_only": true }),
        )
        .await,
    );
    assert_eq!(dry["ok"], json!(true));
    assert_eq!(dry["validate_only"], json!(true));
    let missing = call(
        &base,
        &sid,
        "lmgw__agent_get",
        json!({ "id": "note-taker" }),
    )
    .await;
    assert!(is_error(&missing), "validate_only wrote something");

    let done = payload(
        &call(
            &base,
            &sid,
            "lmgw__agent_set",
            json!({ "manifest": manifest }),
        )
        .await,
    );
    assert_eq!(done["id"], json!("note-taker"));
    // `docs` is a built-in label, so nothing is missing and the report says so.
    assert_eq!(done["warnings"], json!([]));

    let got = payload(
        &call(
            &base,
            &sid,
            "lmgw__agent_get",
            json!({ "id": "note-taker" }),
        )
        .await,
    );
    assert_eq!(got["kind"], json!("chat"));
    assert_eq!(got["requires_ok"], json!(true));
    // The manifest comes back as canonical text — a JSON tree would have been
    // through a `BTreeMap` and re-sorted the config form (agents §2.6).
    let stored: Value = serde_json::from_str(got["manifest"].as_str().unwrap()).unwrap();
    assert_eq!(stored["run"]["system"], json!("You file notes."));

    // The same document as an object installs, and says what that cost.
    let as_object: Value = serde_json::from_str(manifest).unwrap();
    let loose = payload(
        &call(
            &base,
            &sid,
            "lmgw__agent_set",
            json!({ "manifest": as_object }),
        )
        .await,
    );
    assert!(
        loose["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("JSON string")),
        "{loose}"
    );
    // ... and an ordering warning is not a missing server: the next step still
    // points at the config form.
    assert!(
        loose["next_step"].as_str().unwrap().contains("agent_get"),
        "{loose}"
    );

    // A manifest that does not validate is a tool error the model can read and
    // fix, not a protocol failure.
    let bad = call(
        &base,
        &sid,
        "lmgw__agent_set",
        json!({ "manifest": manifest.replace("{{config.model}}", "{{config.nope}}") }),
    )
    .await;
    assert!(is_error(&bad));
    assert!(text_of(&bad).contains("config.nope"), "{}", text_of(&bad));

    let gone = payload(
        &call(
            &base,
            &sid,
            "lmgw__agent_delete",
            json!({ "id": "note-taker" }),
        )
        .await,
    );
    assert_eq!(gone["ok"], json!(true));
    assert!(is_error(
        &call(
            &base,
            &sid,
            "lmgw__agent_get",
            json!({ "id": "note-taker" })
        )
        .await
    ));
}
