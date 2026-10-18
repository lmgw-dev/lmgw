//! What the rest of the agent ops answer over HTTP, through `POST /api/op/{name}`
//! (the install, delete and run ops are `agents_wire`'s, the app container's
//! are `agents_service_wire`'s): `agent_pull`, `agent_reimport`,
//! `agent_config_set`, `agent_enable`, `agent_duplicate`, `agent_reset`,
//! `agent_token_get` / `agent_token_rotate`, `agent_run_cancel`,
//! `agent_open_chat` and `agents_restore`. Each answer reads back through the
//! type the document describes for it and keeps the keys it has always sent:
//! which values are `null` rather than absent, which are empty text, and the
//! exact sentence a person is told.
//!
//! The container runtime is a fake `podman` (`support::agent_world`) and a run
//! reads an MCP stub that holds its first call, so nothing reaches past
//! loopback and no run finishes before a test says so.

use lmgw_api_types as dto;
use lmgw_core::config::Protocol;
use lmgw_core::store;
use serde_json::json;

use crate::agents_wire::{assert_keys, keeper, op};
use crate::common::{self, serve, Gw};
use crate::support::agent_world::{chat, gateway, held_source, pkg, register_held, Podman};

const IMAGE: &str = "localhost/board:1";
const EXTRA: &str = r#", "extra": { "type": "string" }"#;

fn board(version: &str, props: &str) -> String {
    pkg("board", version, IMAGE, "", props)
}

async fn install(gw: &Gw, image: &str) {
    let (status, live) = op(gw, "agent_install", json!({ "image": image })).await;
    assert_eq!(status, 200, "{live}");
}

// ---------------------------------------------------------------------------
// agent_pull, agent_reimport
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pull_says_whether_the_image_moved_and_how_its_manifest_compares() {
    let podman = Podman::default()
        .with(IMAGE, Some("sha256:one"), Some(&board("1.0.0", "")))
        .pulls(IMAGE, Some("sha256:two"), Some(&board("2.0.0", EXTRA)));
    let (_state, gw) = gateway(podman).await;
    install(&gw, IMAGE).await;

    let (status, live) = op(&gw, "agent_pull", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    let moved = common::round_trips::<dto::AgentPulled>("pull, moved", &live);
    assert_keys(
        &live,
        &[
            "ok",
            "id",
            "image",
            "old_digest",
            "new_digest",
            "recorded_digest",
            "changed",
            "manifest_differs",
            "note",
            "message",
        ],
    );
    assert!(moved.ok && moved.changed == Some(true));
    assert_eq!(
        (moved.old_digest.as_deref(), moved.new_digest.as_deref()),
        (Some("sha256:one"), Some("sha256:two"))
    );
    // What the row recorded when it was installed.
    assert_eq!(moved.recorded_digest.as_deref(), Some("sha256:one"));
    assert_eq!(moved.manifest_differs, Some(true));
    assert!(
        moved
            .note
            .starts_with("the new image carries a different manifest."),
        "{live}"
    );
    assert_eq!(
        moved.message,
        "'localhost/board:1' moved: sha256:one → sha256:two"
    );

    // The tag is where it was: nothing was compared, and that is `null` and an
    // empty note, not an absent key.
    let (status, live) = op(&gw, "agent_pull", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    let still = common::round_trips::<dto::AgentPulled>("pull, unchanged", &live);
    assert_eq!(still.changed, Some(false));
    assert!(live["manifest_differs"].is_null(), "{live}");
    assert_eq!(still.note, "");
    assert_eq!(still.recorded_digest.as_deref(), Some("sha256:two"));
    assert_eq!(
        still.message,
        "'localhost/board:1' is unchanged (sha256:two)"
    );
}

#[tokio::test]
async fn a_pull_of_an_image_that_was_not_here_has_no_old_digest_and_no_record() {
    let podman = Podman::default().pulls(IMAGE, Some("sha256:one"), Some(&board("1.0.0", "")));
    let (_state, gw) = gateway(podman).await;
    // Authored, not installed from the image: the row has no provenance.
    let (status, live) = op(&gw, "agent_set", json!({ "manifest": board("1.0.0", "") })).await;
    assert_eq!(status, 200, "{live}");

    let (status, live) = op(&gw, "agent_pull", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    let pulled = common::round_trips::<dto::AgentPulled>("pull, new to the box", &live);
    // Unknown is an absent key, not the empty text.
    assert!(live.get("old_digest").is_none(), "{live}");
    assert!(live.get("recorded_digest").is_none(), "{live}");
    assert_eq!(pulled.new_digest.as_deref(), Some("sha256:one"));
    // There was no image to move from, so the image arrived: that is a change,
    // and the manifest was read and found the same as the stored one.
    assert_eq!(pulled.changed, Some(true), "{live}");
    assert_eq!(pulled.manifest_differs, Some(false), "{live}");
    assert_eq!(
        pulled.message,
        "pulled 'localhost/board:1' (sha256:one); it was not on this box before"
    );
}

#[tokio::test]
async fn a_pull_after_which_podman_reports_no_digest_leaves_changed_absent() {
    let podman = Podman::default().pulls(IMAGE, None, Some(&board("1.0.0", "")));
    let (_state, gw) = gateway(podman).await;
    let (status, live) = op(&gw, "agent_set", json!({ "manifest": board("1.0.0", "") })).await;
    assert_eq!(status, 200, "{live}");

    let (status, live) = op(&gw, "agent_pull", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    let pulled = common::round_trips::<dto::AgentPulled>("pull, no digest", &live);
    // Unknown is not "no": the key is absent, never `false` or `null`.
    assert!(live.get("changed").is_none(), "{live}");
    assert!(live.get("new_digest").is_none(), "{live}");
    assert!(pulled.changed.is_none() && pulled.manifest_differs.is_none());
    assert_eq!(pulled.note, "", "nothing was compared");
}

#[tokio::test]
async fn a_reimport_answers_the_report_the_image_and_whether_the_manifest_differed() {
    let podman = Podman::default()
        .with(IMAGE, Some("sha256:one"), Some(&board("1.0.0", "")))
        .pulls(IMAGE, Some("sha256:two"), Some(&board("2.0.0", EXTRA)));
    let (_state, gw) = gateway(podman).await;
    install(&gw, IMAGE).await;

    // Nothing moved: the image's manifest is the stored one, and the report
    // says so in its warnings.
    let (status, live) = op(&gw, "agent_reimport", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    let same = common::round_trips::<dto::AgentReimported>("reimport, identical", &live);
    assert_keys(
        &live,
        &[
            "ok",
            "id",
            "warnings",
            "requires",
            "replaced",
            "dropped_config",
            "config_unbound",
            "validate_only",
            "image",
            "changed",
            "message",
        ],
    );
    assert!(!same.changed && same.report.replaced);
    assert_eq!(same.image, IMAGE);
    assert!(
        same.report
            .warnings
            .iter()
            .any(|w| w.contains("identical to the one already stored")),
        "{live}"
    );
    assert_eq!(
        same.message,
        "'board' already matches 'localhost/board:1'; nothing changed"
    );

    // After a pull moved the tag the manifest differs, and the config is kept.
    let (status, _) = op(&gw, "agent_pull", json!({ "id": "board" })).await;
    assert_eq!(status, 200);
    let (status, live) = op(&gw, "agent_reimport", json!({ "id": "board" })).await;
    assert_eq!(status, 200, "{live}");
    let changed = common::round_trips::<dto::AgentReimported>("reimport, changed", &live);
    assert!(changed.changed);
    assert_eq!(
        changed.message,
        "re-imported 'board' from 'localhost/board:1', keeping its config"
    );
}

// ---------------------------------------------------------------------------
// agent_config_set, agent_enable, agent_duplicate, agent_reset
// ---------------------------------------------------------------------------

fn cfg(id: &str) -> String {
    json!({
        "schema_version": 1, "id": id, "name": id,
        "model": { "alias": "{{config.model}}" },
        "config": { "schema": { "type": "object", "properties": {
            "model": { "type": "string", "format": "model_alias" },
            "api_token": { "type": "string", "format": "secret" },
            "label": { "type": "string", "default": "x" }
        } } },
        "run": { "kind": "chat", "system": "Be brief." }
    })
    .to_string()
}

async fn put(gw: &Gw, manifest: &str) {
    let (status, live) = op(gw, "agent_set", json!({ "manifest": manifest })).await;
    assert_eq!(status, 200, "{live}");
}

#[tokio::test]
async fn a_config_save_answers_the_masked_config_and_the_scope_its_token_now_has() {
    let (_state, gw) = gateway(Podman::default()).await;
    put(&gw, &cfg("cfg")).await;

    let (status, live) = op(
        &gw,
        "agent_config_set",
        json!({ "id": "cfg", "values": { "model": "m1", "api_token": "s3cr3t", "label": "hi" } }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let saved = common::round_trips::<dto::AgentConfigSaved>("config_set", &live);
    assert_keys(
        &live,
        &[
            "ok",
            "id",
            "token_scope",
            "mounts_repointed",
            "config",
            "cleared_undeclared",
            "message",
        ],
    );
    assert_eq!(saved.token_scope, "token: m1");
    // The secret is never echoed; whether it is set is.
    assert_eq!(saved.config["api_token"], json!({ "has_value": true }));
    assert_eq!(saved.config["label"], json!("hi"));
    assert_eq!(saved.message, "config saved");
    // Nothing was repointed and no app was holding anything: absent.
    assert!(saved.mounts_repointed.is_empty());
    assert!(live.get("service_stopped").is_none(), "{live}");

    // A name the manifest does not declare is cleared, and said.
    let (status, live) = op(
        &gw,
        "agent_config_set",
        json!({ "id": "cfg", "values": {}, "clear": ["ghost", "phantom", "label"] }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let cleared = common::round_trips::<dto::AgentConfigSaved>("config_set, clearing", &live);
    assert_eq!(cleared.cleared_undeclared, ["ghost", "phantom"]);
    assert_eq!(
        cleared.message,
        "config saved; ghost, phantom are not declared by this manifest and were removed from \
         the stored config"
    );
}

#[tokio::test]
async fn an_enable_answers_the_state_and_what_disabling_did_to_the_app() {
    let (_state, gw) = gateway(Podman::default()).await;
    keeper(&gw, "plain").await;

    let (status, live) = op(
        &gw,
        "agent_enable",
        json!({ "id": "plain", "enabled": false }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let off = common::round_trips::<dto::AgentEnabled>("enable, off", &live);
    assert_keys(&live, &["ok", "id", "enabled", "message"]);
    assert!(!off.enabled);
    // No app was running: the key is absent.
    assert!(live.get("service_stopped").is_none(), "{live}");
    assert_eq!(off.message, "agent disabled");

    let (status, live) = op(
        &gw,
        "agent_enable",
        json!({ "id": "plain", "enabled": true }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let on = common::round_trips::<dto::AgentEnabled>("enable, on", &live);
    assert!(on.enabled && on.service_stopped.is_none());
    assert_eq!(on.message, "agent enabled");
}

#[tokio::test]
async fn a_duplicate_names_the_copy_and_the_secret_fields_it_left_out() {
    let (_state, gw) = gateway(Podman::default()).await;
    put(&gw, &cfg("cfg")).await;

    let (status, live) = op(
        &gw,
        "agent_duplicate",
        json!({ "id": "cfg", "new_id": "cfg2" }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    let copy = common::round_trips::<dto::AgentDuplicated>("duplicate", &live);
    assert_keys(&live, &["ok", "id", "name", "config_omitted", "message"]);
    assert_eq!(copy.id, "cfg2");
    assert_eq!(copy.name, "cfg (copy)");
    assert_eq!(copy.config_omitted, ["api_token"]);
    assert_eq!(copy.message, "duplicated 'cfg' as 'cfg2'");

    let (status, live) = op(
        &gw,
        "agent_duplicate",
        json!({ "id": "cfg", "new_id": "cfg3", "name": "Third" }),
    )
    .await;
    assert_eq!(status, 200, "{live}");
    assert_eq!(
        common::round_trips::<dto::AgentDuplicated>("duplicate, named", &live).name,
        "Third"
    );
}

#[tokio::test]
async fn a_reset_answers_the_config_keys_the_shipped_schema_dropped() {
    let (state, gw) = gateway(Podman::default()).await;
    let shipped = lmgw_core::agents::seed::shipped_all()
        .first()
        .expect("an agent ships")
        .id
        .clone();

    let (status, live) = op(&gw, "agent_reset", json!({ "id": shipped })).await;
    assert_eq!(status, 200, "{live}");
    let plain = common::round_trips::<dto::AgentReset>("reset", &live);
    assert_keys(&live, &["ok", "id", "dropped_config", "message"]);
    assert!(plain.dropped_config.is_empty());
    assert_eq!(
        plain.message,
        format!("'{shipped}' restored to the shipped manifest; its config was kept")
    );

    // A stored value the shipped schema does not declare would fail the next
    // Start: it is dropped, and named.
    store::set_agent_config(&state.db, &shipped, r#"{"bogus":"x"}"#)
        .await
        .unwrap();
    let (status, live) = op(&gw, "agent_reset", json!({ "id": shipped })).await;
    assert_eq!(status, 200, "{live}");
    let dropped = common::round_trips::<dto::AgentReset>("reset, dropping", &live);
    assert_eq!(dropped.dropped_config, ["bogus"]);
    assert_eq!(
        dropped.message,
        format!(
            "'{shipped}' restored to the shipped manifest; its config was kept except bogus, \
             which the shipped schema no longer declares"
        )
    );
}

#[tokio::test]
async fn a_restore_names_the_shipped_agents_it_put_back() {
    let (_state, gw) = gateway(Podman::default()).await;
    let (status, live) = op(&gw, "agents_restore", json!({})).await;
    assert_eq!(status, 200, "{live}");
    let none = common::round_trips::<dto::AgentsRestored>("restore, nothing missing", &live);
    assert_keys(&live, &["ok", "restored", "message"]);
    assert!(none.restored.is_empty());
    assert_eq!(
        none.message,
        "every shipped agent is already in the catalog"
    );

    let shipped = lmgw_core::agents::seed::shipped_all()
        .first()
        .expect("an agent ships")
        .id
        .clone();
    let (status, live) = op(&gw, "agent_delete", json!({ "id": shipped })).await;
    assert_eq!(status, 200, "{live}");
    let (status, live) = op(&gw, "agents_restore", json!({})).await;
    assert_eq!(status, 200, "{live}");
    let back = common::round_trips::<dto::AgentsRestored>("restore", &live);
    assert_eq!(back.restored, [shipped.as_str()]);
    assert_eq!(back.message, format!("restored {shipped}"));
}

// ---------------------------------------------------------------------------
// agent_token_get, agent_token_rotate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_token_answer_carries_the_token_and_the_scope_and_never_prints_it() {
    let (_state, gw) = gateway(Podman::default()).await;
    put(&gw, &cfg("cfg")).await;

    let (status, live) = op(&gw, "agent_token_get", json!({ "id": "cfg" })).await;
    assert_eq!(status, 200, "{live}");
    let got = common::round_trips::<dto::AgentTokenRead>("token_get", &live);
    assert_keys(
        &live,
        &[
            "ok",
            "id",
            "name",
            "token",
            "scope_mode",
            "scope_patterns",
            "scope_note",
            "message",
        ],
    );
    assert_eq!(got.name, "agent:cfg");
    assert!(!got.token.is_empty());
    // No model chosen yet: the token opens any model.
    assert_eq!(
        (got.scope_mode.as_str(), got.scope_patterns.as_str()),
        ("all", "")
    );
    assert_eq!(got.scope_note, "token: any model");
    // Only a rotation has a stopped app container or a scope warning to report.
    assert!(
        live.get("service_stopped").is_none() && live.get("warning").is_none(),
        "{live}"
    );
    assert_eq!(got.message, "the token for 'cfg'");
    // A log line of the answer does not carry the token.
    assert!(!format!("{got:?}").contains(&got.token));

    // Reading it again is the same token; a model in the config fences it.
    let (_, saved) = op(
        &gw,
        "agent_config_set",
        json!({ "id": "cfg", "values": { "model": "m1" } }),
    )
    .await;
    assert_eq!(saved["token_scope"], json!("token: m1"), "{saved}");
    let (_, again) = op(&gw, "agent_token_get", json!({ "id": "cfg" })).await;
    let again = common::round_trips::<dto::AgentTokenRead>("token_get, scoped", &again);
    assert_eq!(again.token, got.token);
    assert_eq!(
        (again.scope_mode.as_str(), again.scope_patterns.as_str()),
        ("allow", "m1")
    );
    assert_eq!(again.scope_note, "token: m1");

    // Rotating replaces it, and says the old one stopped working.
    let (status, live) = op(&gw, "agent_token_rotate", json!({ "id": "cfg" })).await;
    assert_eq!(status, 200, "{live}");
    let rotated = common::round_trips::<dto::AgentTokenIssued>("token_rotate", &live);
    assert!(live.get("warning").is_none(), "{live}");
    assert_ne!(rotated.token, got.token);
    assert_eq!(
        rotated.message,
        "rotated the token for 'cfg' — the previous one stopped working now"
    );
}

// ---------------------------------------------------------------------------
// agent_run_cancel, agent_open_chat
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancel_answers_the_agent_and_what_the_job_registry_said() {
    let state = crate::agents_runs::gateway("http://127.0.0.1:1", Protocol::Openai).await;
    let (source, release) = held_source().await;
    register_held(&state, &source).await;
    let gw = serve(state.clone()).await;
    crate::agents_runs::install(&gw, &crate::agents_runs::doc("m1")).await;

    let (status, started) = op(
        &gw,
        "agent_run",
        json!({ "id": "labeler", "phase": "list" }),
    )
    .await;
    assert_eq!(status, 200, "{started}");
    source.wait_calls(1).await;

    let (status, live) = op(&gw, "agent_run_cancel", json!({ "id": "labeler" })).await;
    assert_eq!(status, 200, "{live}");
    let cancelled = common::round_trips::<dto::AgentRunCancelled>("cancel", &live);
    assert_keys(&live, &["ok", "id", "message"]);
    assert!(cancelled.ok);
    assert_eq!(cancelled.id, "labeler");
    let job = started["job_id"].as_i64().unwrap();
    assert_eq!(cancelled.message, format!("cancel requested for job {job}"));

    release.now();
    crate::agents_runs::settle(&state, job).await;
}

#[tokio::test]
async fn an_open_chat_answers_the_thread_and_what_is_off_about_it() {
    let (_state, gw) = gateway(Podman::default()).await;
    // An alias nobody serves, a tool server nobody registered, and sampling
    // knobs a thread has no column for.
    let manifest = json!({
        "schema_version": 1, "id": "chatty", "name": "Chatty",
        "model": { "alias": "nope", "temperature": 0.2, "top_p": 0.9, "seed": 7 },
        "tools": [ { "label": "gws" } ],
        "run": { "kind": "chat", "system": "Be brief." }
    })
    .to_string();
    put(&gw, &manifest).await;

    let (status, live) = op(&gw, "agent_open_chat", json!({ "id": "chatty" })).await;
    assert_eq!(status, 200, "{live}");
    let opened = common::round_trips::<dto::AgentChatOpened>("open_chat", &live);
    assert_keys(
        &live,
        &[
            "ok",
            "id",
            "thread_id",
            "model",
            "tools",
            "url",
            "warnings",
            "message",
        ],
    );
    assert_eq!(opened.id, "chatty");
    assert!(opened.thread_id > 0);
    assert_eq!(opened.model, "nope");
    assert_eq!(opened.tools, ["gws"]);
    assert_eq!(opened.url, format!("/chat?t={}", opened.thread_id));
    assert_eq!(opened.message, "opened a thread for 'Chatty'");
    assert_eq!(opened.warnings.len(), 3, "{live}");
    assert!(opened.warnings[0].contains("does not currently serve the model 'nope'"));
    assert!(opened.warnings[1].contains("'gws', which is not a registered MCP server"));
    assert!(opened.warnings[2].contains("top_p, seed are not applied"));

    // Served and nothing amiss: the warnings are an empty list, not absent.
    let state = crate::agents_runs::gateway("http://127.0.0.1:1", Protocol::Openai).await;
    let gw = serve(state).await;
    put(&gw, &chat("calm", "m1")).await;
    let (status, live) = op(&gw, "agent_open_chat", json!({ "id": "calm" })).await;
    assert_eq!(status, 200, "{live}");
    common::round_trips::<dto::AgentChatOpened>("open_chat, calm", &live);
    assert_eq!(live["warnings"], json!([]), "{live}");
}
