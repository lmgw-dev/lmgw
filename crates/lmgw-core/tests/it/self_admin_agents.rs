//! The answers of the agent tools (`lmgw__agents`, `lmgw__agent_get`,
//! `lmgw__agent_set`, `lmgw__agent_install`, `lmgw__agent_delete`,
//! `lmgw__agent_run`): each tool's text is read into its type and written out
//! again, and the two texts are equal byte for byte. The text is the answer's
//! pretty JSON with sorted keys, so a key a type adds, drops, renames or turns
//! from absent into `null` shows up here.
//!
//! The container runtime is a fake `podman` and a run reads an MCP stub that
//! holds its first call (`support::agent_world`): nothing reaches past
//! loopback.

use lmgw_api_types as dto;
use lmgw_api_types::self_admin::AgentList;
use lmgw_core::config::Protocol;
use serde_json::{json, Value};

use crate::support::agent_world::{chat, gateway, held_source, pkg, register_held, Podman};
use crate::support::self_admin::{parse, read_back, Admin};

const IMAGE: &str = "localhost/board:1";
const NEEDS_GWS: &str = r#""tools": [ { "label": "gws", "allowed": ["gws__search"] } ],"#;

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

const ROW_KEYS: [&str; 10] = [
    "id",
    "name",
    "description",
    "kind",
    "model",
    "enabled",
    "source",
    "tools",
    "requires_ok",
    "error",
];

#[tokio::test]
async fn the_agents_tool_lists_trimmed_cards_and_the_row_it_could_not_read() {
    let (state, _gw) = gateway(Podman::default()).await;
    let admin = Admin::new(&state).await;
    admin
        .text(
            "lmgw__agent_set",
            json!({ "manifest": pkg("alpha", "1.0.0", IMAGE, NEEDS_GWS, "") }),
        )
        .await;
    admin
        .text(
            "lmgw__agent_set",
            json!({ "manifest": chat("beta", "whatever") }),
        )
        .await;
    admin
        .text(
            "lmgw__agent_set",
            json!({ "manifest": chat("gamma", "some-alias") }),
        )
        .await;
    // A row written by a newer build: the card still lists, saying why.
    sqlx::query("UPDATE agents SET manifest = ?1 WHERE id = 'beta'")
        .bind(r#"{"schema_version": 99, "id": "beta"}"#)
        .execute(&state.db)
        .await
        .unwrap();

    let text = admin.text("lmgw__agents", json!({})).await;
    let list: AgentList = read_back(&text);
    let live = parse(&text);
    assert_keys(&live, &["agents", "count", "next_step"]);
    assert_eq!(list.count, list.agents.len());
    assert!(live["count"].is_u64(), "{live}");
    // The hint names a listed agent, not a placeholder.
    assert_eq!(
        list.next_step,
        format!(
            "lmgw__agent_get id={} for the manifest and the config form",
            list.agents[0].id
        )
    );
    // `model` is absent while the agent has none (alpha's is a template the
    // config has not filled in; beta's row could not be read), never the empty
    // text; gamma names one.
    let no_model: Vec<&str> = ROW_KEYS.iter().copied().filter(|k| *k != "model").collect();
    for row in live["agents"].as_array().unwrap() {
        match row["id"].as_str().unwrap() {
            "gamma" => {
                assert_keys(row, &ROW_KEYS);
                assert_eq!(row["model"], json!("some-alias"), "{row}");
            }
            _ => assert_keys(row, &no_model),
        }
    }

    let alpha = list.agents.iter().find(|a| a.id == "alpha").expect("alpha");
    assert_eq!(alpha.kind, "container");
    assert_eq!(alpha.tools, ["gws"]);
    // No server is registered for `gws`, so the card says it cannot start.
    assert!(!alpha.requires_ok);
    assert_eq!(alpha.error, None);
    let alpha_live = live["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "alpha")
        .unwrap();
    assert_eq!(alpha_live["error"], Value::Null, "{alpha_live}");

    let beta = list.agents.iter().find(|a| a.id == "beta").expect("beta");
    assert!(beta.error.is_some(), "{beta:?}");
}

#[tokio::test]
async fn the_agent_get_tool_answers_the_agent_detail() {
    let (state, _gw) = gateway(Podman::default()).await;
    let admin = Admin::new(&state).await;
    admin
        .text(
            "lmgw__agent_set",
            json!({ "manifest": pkg("alpha", "1.0.0", IMAGE, NEEDS_GWS, "") }),
        )
        .await;

    let text = admin
        .text("lmgw__agent_get", json!({ "id": "alpha" }))
        .await;
    let detail: dto::AgentDetail = read_back(&text);
    assert_eq!(detail.id, "alpha");
    assert_eq!(detail.kind, "container");
    // The manifest as stored, as text.
    assert!(detail.manifest.contains("\"schema_version\""), "{text}");
    assert!(!detail.requires_ok);

    let (failed, text) = admin
        .call("lmgw__agent_get", json!({ "id": "ghost" }))
        .await;
    assert!(failed, "{text}");
    assert_eq!(text, "no agent with id 'ghost'");
}

#[tokio::test]
async fn the_agent_set_tool_answers_the_report_with_the_next_step_that_follows_from_it() {
    let (state, _gw) = gateway(Podman::default()).await;
    let admin = Admin::new(&state).await;
    let set = |manifest: String, extra: Value| {
        let admin = &admin;
        async move {
            let mut args = json!({ "manifest": manifest });
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().cloned().unwrap_or_default());
            admin.text("lmgw__agent_set", args).await
        }
    };

    // Nothing missing: look at the form it needs filled in.
    let text = set(pkg("alpha", "1.0.0", IMAGE, "", ""), json!({})).await;
    let done: dto::WithNextStep<dto::AgentImportReport> = read_back(&text);
    assert_keys(
        &parse(&text),
        &[
            "ok",
            "id",
            "warnings",
            "requires",
            "replaced",
            "dropped_config",
            "config_unbound",
            "validate_only",
            "next_step",
        ],
    );
    assert_eq!(done.answer.id, "alpha");
    assert_eq!(
        done.next_step,
        "lmgw__agent_get id=alpha to see the config form it needs filled in"
    );

    // A label no server answers: register it first.
    let text = set(pkg("tooled", "1.0.0", IMAGE, NEEDS_GWS, ""), json!({})).await;
    let missing: dto::WithNextStep<dto::AgentImportReport> = read_back(&text);
    assert!(!missing.answer.requires[0].registered);
    assert_eq!(
        missing.next_step,
        "register the missing MCP server (lmgw__mcp_server_set), then lmgw__agent_get \
         id=tooled to confirm requires_ok"
    );

    // Validating writes nothing.
    let text = set(
        pkg("alpha", "2.0.0", IMAGE, "", ""),
        json!({ "validate_only": true }),
    )
    .await;
    let checked: dto::WithNextStep<dto::AgentImportReport> = read_back(&text);
    assert!(checked.answer.validate_only);
    assert_eq!(
        checked.next_step,
        "nothing was written; call again without validate_only to install it"
    );

    // An object manifest earns the ordering warning, on top of the report.
    let object: Value = serde_json::from_str(&pkg("shaped", "1.0.0", IMAGE, "", "")).unwrap();
    let text = admin
        .text("lmgw__agent_set", json!({ "manifest": object }))
        .await;
    let shaped: dto::WithNextStep<dto::AgentImportReport> = read_back(&text);
    assert!(
        shaped
            .answer
            .warnings
            .iter()
            .any(|w| w.contains("arrived as a JSON object")),
        "{text}"
    );
}

#[tokio::test]
async fn the_agent_install_tool_answers_where_the_manifest_came_from_and_names_the_agent() {
    let podman = Podman::default()
        .with(
            IMAGE,
            Some("sha256:one"),
            Some(&pkg("board", "1.0.0", IMAGE, "", "")),
        )
        .with(
            "localhost/blind:1",
            None,
            Some(&pkg("blind", "1.0.0", "localhost/blind:1", "", "")),
        );
    let (state, _gw) = gateway(podman).await;
    let admin = Admin::new(&state).await;

    let text = admin
        .text("lmgw__agent_install", json!({ "image": IMAGE }))
        .await;
    let done: dto::WithNextStep<dto::AgentInstalled> = read_back(&text);
    assert_keys(
        &parse(&text),
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
            "digest",
            "pulled",
            "manifest_path",
            "message",
            "next_step",
        ],
    );
    assert_eq!(done.answer.report.id, "board");
    assert_eq!(done.answer.digest.as_deref(), Some("sha256:one"));
    assert!(!done.answer.pulled);
    assert_eq!(
        done.next_step,
        "lmgw__agent_get id=board for the manifest that was installed and the config form it \
         declares; the owner fills the config in on the Run tab"
    );

    // No digest is an absent key, and says so in the warnings.
    let text = admin
        .text(
            "lmgw__agent_install",
            json!({ "image": "localhost/blind:1" }),
        )
        .await;
    let blind: dto::WithNextStep<dto::AgentInstalled> = read_back(&text);
    assert!(parse(&text).get("digest").is_none(), "{text}");
    assert!(blind
        .answer
        .report
        .warnings
        .iter()
        .any(|w| w.contains("digest")));

    // Installing over an agent is asked for.
    let (failed, text) = admin
        .call("lmgw__agent_install", json!({ "image": IMAGE }))
        .await;
    assert!(failed, "{text}");
}

#[tokio::test]
async fn the_agent_delete_tool_answers_what_it_removed() {
    let (state, _gw) = gateway(Podman::default()).await;
    let admin = Admin::new(&state).await;
    admin
        .text(
            "lmgw__agent_set",
            json!({ "manifest": chat("scratch", "whatever") }),
        )
        .await;

    let text = admin
        .text("lmgw__agent_delete", json!({ "id": "scratch" }))
        .await;
    let gone: dto::AgentDeleted = read_back(&text);
    assert_keys(
        &parse(&text),
        &["ok", "id", "threads_kept", "message", "hint"],
    );
    assert_eq!(gone.id, "scratch");
    assert_eq!(gone.threads_kept, 0);
    assert_eq!(gone.hint, "gone; import the exported file to bring it back");

    let (failed, text) = admin
        .call("lmgw__agent_delete", json!({ "id": "scratch" }))
        .await;
    assert!(failed, "{text}");
    assert_eq!(text, "no agent with id 'scratch'");
}

#[tokio::test]
async fn the_agent_run_tool_answers_the_job_and_points_at_agent_get_for_its_outcome() {
    let state = crate::agents_runs::gateway("http://127.0.0.1:1", Protocol::Openai).await;
    let (source, release) = held_source().await;
    register_held(&state, &source).await;
    let admin = Admin::new(&state).await;
    admin
        .text(
            "lmgw__agent_set",
            json!({ "manifest": crate::agents_runs::doc("m1") }),
        )
        .await;

    let text = admin
        .text(
            "lmgw__agent_run",
            json!({ "id": "labeler", "phase": "list" }),
        )
        .await;
    let started: dto::WithNextStep<dto::AgentRunStarted> = read_back(&text);
    assert_keys(
        &parse(&text),
        &[
            "ok",
            "id",
            "job_id",
            "phase",
            "already_running",
            "message",
            "next_step",
        ],
    );
    let job = started.answer.job_id;
    assert!(!started.answer.already_running);
    assert_eq!(
        started.next_step,
        format!(
            "lmgw__agent_get id=labeler shows run #{job} while it runs (live_job) and its \
             outcome after (last_run); its rows are on the agent's dashboard page, where the \
             owner applies them"
        )
    );

    // In flight: the tool is handed the run it met.
    source.wait_calls(1).await;
    let text = admin
        .text(
            "lmgw__agent_run",
            json!({ "id": "labeler", "phase": "classify" }),
        )
        .await;
    let met: dto::WithNextStep<dto::AgentRunStarted> = read_back(&text);
    assert!(met.answer.already_running);
    assert_eq!(met.answer.job_id, job);
    // The in-flight run's phase, not the one asked for.
    assert_eq!(met.answer.phase, "list");

    release.now();
    crate::agents_runs::settle(&state, job).await;

    // The hint's promise: the outcome is readable through the tool afterwards.
    let text = admin
        .text("lmgw__agent_get", json!({ "id": "labeler" }))
        .await;
    let detail: dto::AgentDetail = read_back(&text);
    let last = detail.last_run.expect("last_run after the run");
    assert_eq!(last.job_id, job);
    assert_eq!(last.status, "done", "{text}");

    // Applying is a human action.
    let (failed, text) = admin
        .call(
            "lmgw__agent_run",
            json!({ "id": "labeler", "phase": "apply" }),
        )
        .await;
    assert!(failed, "{text}");
}
