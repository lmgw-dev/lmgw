//! The agent catalog (agent-catalog design §5). Thin, like `docs_corpora`: the
//! dashboard's own handlers do the work, so the tool plane and the human plane
//! cannot drift into two different import rules. An agent holding the admin
//! token can install an agent, which is the point of the catalog being data.
//!
//! Every tool answers a type of `lmgw-api-types`: the dashboard's own where the
//! op has an HTTP twin (`AgentDetail`, `AgentDeleted`), with the tool plane's
//! `next_step` beside it where one is given, and `self_admin::AgentList` for
//! the one read only the tool plane has.

use lmgw_api_types::self_admin::AgentList;
use lmgw_api_types::{
    AgentDeleted, AgentDetail, AgentImportReport, AgentInstalled, AgentRunStarted, WithNextStep,
};
use serde_json::{Map, Value};

use crate::state::SharedState;

pub async fn agents(state: &SharedState) -> Result<AgentList, String> {
    crate::web::api_agents::tool_list(state).await
}

pub async fn agent_get(state: &SharedState, id: &str) -> Result<AgentDetail, String> {
    crate::web::api_agents::tool_get(state, id).await
}

/// `manifest` is the whole document, as a JSON string or as an object — the
/// same two shapes the dashboard op takes, warning about the second for the
/// same reason (see `api_agents::manifest_arg`). `by_device` is the paired
/// device that called, if one did: it records the agent as its own, and may
/// replace only an agent it created (client-apps design L5's note,
/// 2026-10-07).
pub async fn agent_set(
    state: &SharedState,
    manifest: &Value,
    replace: Option<bool>,
    validate_only: Option<bool>,
    by_device: Option<i64>,
) -> Result<WithNextStep<AgentImportReport>, String> {
    let (text, order_warning) = crate::web::api_agents::manifest_arg(Some(manifest))?;
    // The tool plane is text: a refusal's code is an HTTP concern, and what
    // reaches a model here is the message that names the rule.
    let mut report = crate::web::api_agents::import_inner_as(
        state,
        &text,
        replace.unwrap_or(true),
        validate_only.unwrap_or(false),
        by_device,
    )
    .await
    .map_err(|e| e.message)?;
    report.warnings.extend(order_warning);
    // What to do next follows from the *requirements*, not from the warning
    // count: an ordering warning is not a missing MCP server.
    let missing = report
        .requires
        .iter()
        .any(|r| !r.registered || !r.missing_tools.is_empty());
    let id = &report.id;
    let next_step = if report.validate_only {
        "nothing was written; call again without validate_only to install it".to_string()
    } else if missing {
        format!(
            "register the missing MCP server (lmgw__mcp_server_set), then \
             lmgw__agent_get id={id} to confirm requires_ok"
        )
    } else {
        format!("lmgw__agent_get id={id} to see the config form it needs filled in")
    };
    Ok(WithNextStep::new(report, next_step))
}

/// `by_device` is the paired device that called, if one did: it deletes only
/// an agent it created (client-apps design L5's note).
pub async fn agent_delete(
    state: &SharedState,
    id: &str,
    by_device: Option<i64>,
) -> Result<AgentDeleted, String> {
    crate::web::api_agents::agent_delete_as(state, id, by_device).await
}

/// `lmgw__agent_install` — install an agent from an **image** (container-runtime
/// §3.4, §8).
///
/// The one path the dashboard's Install-from-image takes, so the report is the
/// same one `agent_set` returns plus where the document came from. `pull`
/// defaults to `never` and `replace` to `false`, exactly as the op does: a
/// model that wants a download or an overwrite has to ask for it.
pub async fn agent_install(
    state: &SharedState,
    image: &str,
    pull: Option<&str>,
    replace: Option<bool>,
    validate_only: Option<bool>,
    by_device: Option<i64>,
) -> Result<WithNextStep<AgentInstalled>, String> {
    let mut args = Map::new();
    args.insert("image".into(), Value::from(image));
    if let Some(p) = pull {
        args.insert("pull".into(), Value::from(p));
    }
    if let Some(r) = replace {
        args.insert("replace".into(), Value::from(r));
    }
    if let Some(v) = validate_only {
        args.insert("validate_only".into(), Value::from(v));
    }
    let out = crate::web::api_agents::agent_install_as(state, &args, by_device).await?;
    let id = &out.report.id;
    let next_step = format!(
        "lmgw__agent_get id={id} for the manifest that was installed and the config form \
         it declares; the owner fills the config in on the Run tab"
    );
    Ok(WithNextStep::new(out, next_step))
}

/// `lmgw__agent_run` — start a **read-only** phase of a batch run.
///
/// `apply` is refused here rather than being absent from the enum, so a model
/// that asks for it is told *why* instead of getting a schema error: the review
/// table is the gate, and nothing an agent writes happens without a person
/// (agent-catalog §1 principle 4, §5). A run a paired device starts
/// (`by_device`) calls lmgw's admin tools as that device, at what its admin
/// tools may do (client-apps design L5's note, 2026-10-07).
pub async fn agent_run(
    state: &SharedState,
    id: &str,
    phase: &str,
    by_device: Option<i64>,
) -> Result<WithNextStep<AgentRunStarted>, String> {
    if !matches!(phase, "list" | "classify") {
        return Err(format!(
            "lmgw__agent_run starts 'list' or 'classify' only; '{phase}' writes or depends on a \
             review, and applying an agent's work is a human action on the dashboard"
        ));
    }
    let mut args = Map::new();
    args.insert("id".into(), Value::from(id));
    args.insert("phase".into(), Value::from(phase));
    let out = crate::web::api_agents::agent_run_as(state, &args, by_device).await?;
    let next_step = format!(
        "lmgw__agent_get id={id} shows run #{} while it runs (live_job) and its outcome \
         after (last_run); its rows are on the agent's dashboard page, where the owner \
         applies them",
        out.job_id
    );
    Ok(WithNextStep::new(out, next_step))
}
