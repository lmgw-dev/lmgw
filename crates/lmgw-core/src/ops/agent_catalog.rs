//! The agent catalog (agent-catalog design §5). Thin, like `docs_corpora`: the
//! dashboard's own handlers do the work, so the tool plane and the human plane
//! cannot drift into two different import rules. An agent holding the admin
//! token can install an agent, which is the point of the catalog being data.

use serde_json::{json, Map, Value};

use crate::state::SharedState;

pub async fn agents(state: &SharedState) -> Result<Value, String> {
    crate::web::api_agents::tool_list(state).await
}

pub async fn agent_get(state: &SharedState, id: &str) -> Result<Value, String> {
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
) -> Result<Value, String> {
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
    let mut out = serde_json::to_value(&report).map_err(|e| e.to_string())?;
    if let Some(obj) = out.as_object_mut() {
        obj.insert(
            "next_step".into(),
            json!(if report.validate_only {
                "nothing was written; call again without validate_only to install it"
            } else if missing {
                "register the missing MCP server (lmgw__mcp_server_set), then \
                 lmgw__agent_get to confirm requires_ok"
            } else {
                "lmgw__agent_get id=<id> to see the config form it needs filled in"
            }),
        );
    }
    Ok(out)
}

/// `by_device` is the paired device that called, if one did: it deletes only
/// an agent it created (client-apps design L5's note).
pub async fn agent_delete(
    state: &SharedState,
    id: &str,
    by_device: Option<i64>,
) -> Result<Value, String> {
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
) -> Result<Value, String> {
    let mut args = Map::new();
    args.insert("image".into(), json!(image));
    if let Some(p) = pull {
        args.insert("pull".into(), json!(p));
    }
    if let Some(r) = replace {
        args.insert("replace".into(), json!(r));
    }
    if let Some(v) = validate_only {
        args.insert("validate_only".into(), json!(v));
    }
    let mut out = crate::web::api_agents::agent_install_as(state, &args, by_device).await?;
    if let Some(obj) = out.as_object_mut() {
        let id = obj
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        obj.insert(
            "next_step".into(),
            json!(format!(
                "lmgw__agent_get id={id} for the manifest that was installed and the config form \
                 it declares; the owner fills the config in on the Run tab"
            )),
        );
    }
    Ok(out)
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
) -> Result<Value, String> {
    if !matches!(phase, "list" | "classify") {
        return Err(format!(
            "lmgw__agent_run starts 'list' or 'classify' only; '{phase}' writes or depends on a \
             review, and applying an agent's work is a human action on the dashboard"
        ));
    }
    let mut args = Map::new();
    args.insert("id".into(), json!(id));
    args.insert("phase".into(), json!(phase));
    let mut out = crate::web::api_agents::agent_run_as(state, &args, by_device).await?;
    if let Some(obj) = out.as_object_mut() {
        obj.insert(
            "next_step".into(),
            json!(format!(
                "GET /api/agents/runs/{} for the rows and the run's usage; the owner applies it \
                 from the dashboard",
                obj.get("job_id")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
            )),
        );
    }
    Ok(out)
}
