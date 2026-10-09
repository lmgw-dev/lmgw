//! `mcp_server_set` on a paired device's hosted-tools row (client-apps design
//! §5.2): the grant owns it, so only its `enabled` flag and its call timeout
//! are the MCP page's to change. Every other field is the grant's (the
//! prefix, the name), unused (URL, command, headers) or fixed (`idle_seconds`
//! 0, never reaped; `autostart` off, lmgw never dials it; `allow_sampling`
//! off, R26). A value sent back as it is stored is no change, so the MCP
//! page's whole form saves.

use serde_json::{json, Value};

use crate::config::{McpServer, McpTransport};
use crate::runtime::argv;
use crate::state::SharedState;
use crate::store;

use super::super::{opt, parse_env, parse_headers};
use super::McpServerPatch;

/// Why sampling is refused on a device row, wherever it is asked for.
pub(crate) const SAMPLING_REFUSAL: &str =
    "a device-hosted server cannot sample lmgw's models: sampling would spend under \
     internal:mcp-sampling, outside the device's own key";

/// The refusal of a device row's create or delete by hand.
fn owned_by_grant(what: &str) -> String {
    format!(
        "a device-hosted MCP server is {what} with its device's hosting grant: set or clear \
         the device's hosting label on Usage → Keys"
    )
}

/// `create` with `transport: "device"`.
pub(super) fn create_refusal() -> String {
    owned_by_grant("created")
}

/// `delete` of a device row.
pub(super) fn delete_refusal(cur: &McpServer) -> String {
    format!("'{}': {}", cur.name, owned_by_grant("deleted"))
}

/// `update`, `enable` or `disable` of a device row `cur`.
pub(super) async fn update(
    state: &SharedState,
    p: &McpServerPatch,
    cur: &McpServer,
) -> Result<Value, String> {
    if let Some(why) = refusal(p, cur)? {
        return Err(format!("'{}': {why}", cur.name));
    }
    let enabled = match p.action.as_str() {
        "enable" => true,
        "disable" => false,
        _ => p.enabled.unwrap_or(cur.enabled),
    };
    let timeout_ms = p.timeout_ms.unwrap_or(cur.timeout_ms);
    if timeout_ms == 0 {
        return Err("timeout_ms must be above 0 — it is when lmgw stops waiting for a call".into());
    }
    store::update_device_server(&state.db, cur.id, enabled, timeout_ms)
        .await
        .map_err(|e| e.to_string())?;
    state.reload_snapshot().await.map_err(|e| e.to_string())?;
    Ok(json!({
        "ok": true, "id": cur.id,
        "message": format!("MCP server '{}' updated", cur.name),
    }))
}

/// The first field `p` would change that a device row does not let change.
fn refusal(p: &McpServerPatch, cur: &McpServer) -> Result<Option<String>, String> {
    if p.allow_sampling == Some(true) {
        return Ok(Some(SAMPLING_REFUSAL.to_string()));
    }
    if opt(&p.sampling_alias).is_some() {
        return Ok(Some(SAMPLING_REFUSAL.to_string()));
    }
    if let Some(n) = opt(&p.name).filter(|n| *n != cur.name) {
        return Ok(Some(format!(
            "it is named after its device and cannot be renamed (asked: '{n}')"
        )));
    }
    if let Some(t) = opt(&p.transport).filter(|t| McpTransport::parse(t) != Some(cur.transport)) {
        return Ok(Some(format!(
            "its transport is the device's host link and cannot become '{t}'"
        )));
    }
    if opt(&p.tool_prefix).is_some_and(|t| t != cur.tool_prefix) {
        return Ok(Some(
            "its tool prefix is the device's hosting label: change it on Usage → Keys".into(),
        ));
    }
    if p.idle_seconds.is_some_and(|s| s != 0) {
        return Ok(Some(
            "it is never idle-reaped: the device holds its link open, and closing it is the \
             device's"
                .into(),
        ));
    }
    if p.autostart == Some(true) {
        return Ok(Some(
            "lmgw never dials it: the device connects to /mcp/host itself".into(),
        ));
    }
    let unused = opt(&p.command).is_some()
        || opt(&p.cwd).is_some()
        || opt(&p.container_image).is_some()
        || opt(&p.url).is_some()
        || p.args
            .as_deref()
            .map(argv::parse_args_text)
            .transpose()?
            .is_some_and(|a| !a.is_empty())
        || p.extra_run_args
            .as_deref()
            .map(argv::parse_args_text)
            .transpose()?
            .is_some_and(|a| !a.is_empty())
        || p.env
            .as_deref()
            .map(parse_env)
            .transpose()?
            .is_some_and(|e| !e.is_empty())
        || p.headers
            .as_deref()
            .map(parse_headers)
            .transpose()?
            .is_some_and(|h| !h.is_empty());
    Ok(unused.then(|| {
        "the device serves its tools over its own link: a command, a container, a URL or \
         headers do not apply to it"
            .to_string()
    }))
}
