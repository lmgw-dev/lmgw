//! Southbound MCP server patch: `McpServerPatch` and `mcp_server_set`.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::McpTransport;
use crate::runtime::argv;
use crate::state::SharedState;
use crate::store::{self, NewMcpServer};

use super::credential_move::{self, RowWriter};
use super::*;

mod device_row;
pub(crate) use device_row::SAMPLING_REFUSAL as DEVICE_SAMPLING_REFUSAL;

/// Sparse patch for a southbound MCP server. List-valued fields arrive as
/// newline-delimited text (`args`, `extra_run_args`), `KEY=VALUE` lines
/// (`env`), or `Name: Value` lines (`headers`) — flat strings rather than
/// nested JSON, so a small local model can fill them in.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
// The advertised inputSchema is closed (`additionalProperties: false`), so an
// argument we don't know is an error, not something to drop on the floor: a
// caller reaching for a field that doesn't exist (`self_admin` on a settings
// patch, say) must be told, never silently reported success.
#[serde(default, deny_unknown_fields)]
pub struct McpServerPatch {
    pub action: String,
    pub id: Option<i64>,
    pub name: Option<String>,
    pub transport: Option<String>,
    pub command: Option<String>,
    pub args: Option<String>,
    /// `KEY=value` lines — where a stdio server's credentials go.
    #[schemars(transform = lmgw_api_types::openapi_ext::secret)]
    pub env: Option<String>,
    pub cwd: Option<String>,
    pub container_image: Option<String>,
    pub extra_run_args: Option<String>,
    pub url: Option<String>,
    /// `Name: value` lines — where an HTTP server's bearer goes.
    #[schemars(transform = lmgw_api_types::openapi_ext::secret)]
    pub headers: Option<String>,
    pub tool_prefix: Option<String>,
    pub timeout_ms: Option<u64>,
    pub autostart: Option<bool>,
    pub idle_seconds: Option<i64>,
    pub allow_sampling: Option<bool>,
    pub sampling_alias: Option<String>,
    pub enabled: Option<bool>,
}

/// The `lmgw__` namespace is reserved for the built-in self-admin tools, so a
/// southbound server may not claim it as its `tool_prefix` (see
/// [`crate::mcp::RESERVED_TOOL_PREFIX`]). Rejecting it here gives a clear error
/// at configuration time instead of silently-skipped tools at list time.
pub(super) fn validate_tool_prefix(prefix: &str) -> Result<(), String> {
    if prefix.is_empty() {
        return Ok(());
    }
    if !prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("tool prefix may contain only letters, digits, '_' and '-'".into());
    }
    if crate::mcp::RESERVED_NAMESPACES
        .iter()
        .any(|(p, _)| prefix == *p)
    {
        return Err(format!(
            "tool prefix '{prefix}' is reserved for one of lmgw's own built-in toolsets"
        ));
    }
    Ok(())
}

/// A paired device's hosting label is its tools' prefix (client-apps design
/// §1.5), so no server may take it as its name — the same uniqueness the
/// label was checked for when it was granted — nor a name whose spelling as
/// a prefix (`mcp::names::name_qualifier`, what a collision prefixes its
/// tools with) runs into the label's namespace: `phone.` and `phone_` are
/// `phone_`, whose `phone___…` would fall among `phone__…`. Nor a prefix
/// whose namespace runs into the label's ([`reject_device_namespace`]).
pub(crate) fn reject_device_label(
    snap: &crate::config::Snapshot,
    word: &str,
) -> Result<(), String> {
    let q = crate::mcp::names::name_qualifier(word);
    reject_device_word(snap, word, |l| {
        l.eq_ignore_ascii_case(word.trim()) || crate::devices::namespaces_overlap(l, &q)
    })
}

/// A tool prefix whose names would fall in a device's namespace, or whose
/// namespace a device's would fall in: `desktop`, `desktop_` (names
/// `desktop___…`), `desktop__x` beside label `desktop`.
pub(crate) fn reject_device_namespace(
    snap: &crate::config::Snapshot,
    prefix: &str,
) -> Result<(), String> {
    reject_device_word(snap, prefix, |l| {
        crate::devices::namespaces_overlap(l, prefix)
    })
}

fn reject_device_word(
    snap: &crate::config::Snapshot,
    word: &str,
    clashes: impl Fn(&str) -> bool,
) -> Result<(), String> {
    if word.is_empty() {
        return Ok(());
    }
    match snap
        .api_keys
        .iter()
        .find(|k| k.hosts_label.as_deref().is_some_and(&clashes))
    {
        Some(k) => {
            let label = k.hosts_label.as_deref().unwrap_or_default();
            Err(format!(
                "'{word}' clashes with device '{}''s hosting label '{label}' — its tools are \
                 named '{label}__…', and no other server's name or tool names may fall among \
                 them — pick another",
                crate::devices::short_name(&k.name)
            ))
        }
        None => Ok(()),
    }
}

/// `writer` is who asks: a self-admin tool's move of a row with headers to
/// another address must restate them ([`RowWriter`]).
pub async fn mcp_server_set(
    state: &SharedState,
    p: McpServerPatch,
    writer: RowWriter,
) -> Result<Value, String> {
    let snap = state.snapshot();
    let id_of = || p.id.ok_or_else(|| format!("'{}' requires id", p.action));

    match p.action.as_str() {
        "create" => {
            require_all(&[
                ("name", p.name.is_some()),
                ("transport (stdio|http|sse)", p.transport.is_some()),
            ])?;
            let name = opt(&p.name).ok_or("name must not be empty")?;
            reject_agent_name(&name)?;
            let transport = {
                let t = opt(&p.transport).ok_or("transport must not be empty")?;
                McpTransport::parse(&t)
                    .ok_or_else(|| format!("invalid transport '{t}' (stdio|http|sse)"))?
            };
            if transport == McpTransport::Device {
                return Err(device_row::create_refusal());
            }
            let command = opt(&p.command);
            let container_image = opt(&p.container_image);
            let url = opt(&p.url);
            match transport {
                McpTransport::Stdio if command.is_none() && container_image.is_none() => {
                    return Err(
                        "stdio servers need a container_image (isolated) or a command (bare)"
                            .into(),
                    );
                }
                McpTransport::Http | McpTransport::Sse => {
                    let u = url.as_deref().ok_or("http/sse servers need a url")?;
                    reject_self_loop(&snap, u)?;
                }
                _ => {}
            }
            let tool_prefix = opt(&p.tool_prefix).unwrap_or_default();
            validate_tool_prefix(&tool_prefix)?;
            reject_device_namespace(&snap, &tool_prefix)?;
            reject_device_label(&snap, &name)?;

            let new = NewMcpServer {
                name: name.clone(),
                enabled: p.enabled.unwrap_or(true),
                transport,
                command,
                args: argv::parse_args_text(p.args.as_deref().unwrap_or_default())?,
                env: parse_env(p.env.as_deref().unwrap_or_default())?,
                cwd: opt(&p.cwd),
                container_image,
                extra_run_args: argv::parse_args_text(
                    p.extra_run_args.as_deref().unwrap_or_default(),
                )?,
                url,
                headers: parse_headers(p.headers.as_deref().unwrap_or_default())?,
                tool_prefix,
                timeout_ms: p.timeout_ms.unwrap_or(DEFAULT_MCP_TIMEOUT_MS),
                autostart: p.autostart.unwrap_or(true),
                idle_seconds: p.idle_seconds.unwrap_or(0),
                allow_sampling: p.allow_sampling.unwrap_or(false),
                sampling_alias: opt(&p.sampling_alias),
                // Never through this op: an agent's own row is written by the
                // agent lifecycle, and the name check above is what keeps an
                // owner from creating one by hand (§3.3).
                agent_id: None,
            };
            let id = store::insert_mcp_server(&state.db, &new)
                .await
                .map_err(|e| e.to_string())?;
            state.reload_snapshot().await.map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true, "id": id, "message": format!("MCP server '{name}' created") }))
        }
        "update" | "enable" | "disable" => {
            let id = id_of()?;
            let cur = store::get_mcp_server(&state.db, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no MCP server with id {id}"))?;
            if cur.is_device() {
                return device_row::update(state, &p, &cur).await;
            }
            let enabled = match p.action.as_str() {
                "enable" => true,
                "disable" => false,
                _ => p.enabled.unwrap_or(cur.enabled),
            };
            let transport = match opt(&p.transport) {
                Some(t) => match McpTransport::parse(&t) {
                    Some(McpTransport::Device) => return Err(device_row::create_refusal()),
                    Some(t) => t,
                    None => return Err(format!("invalid transport '{t}'")),
                },
                None => cur.transport,
            };
            let url = match opt(&p.url) {
                Some(u) => {
                    // An agent's row is dialled with the `owner:dashboard`
                    // bearer, because it points at lmgw's own
                    // `/agents/<id>/mcp` — an `Admin` route (principals §10
                    // Part 1). Re-pointing it is therefore not an edit to a
                    // URL, it is a request to post the owner's door key to an
                    // address of the caller's choosing, and an `Admin` caller
                    // includes a model driving `/mcp/admin`. The URL is
                    // derived from the manifest and `bind_addr` — refused
                    // here, and the dial checks the address again anyway.
                    if let Some(agent) = cur.agent_id.as_deref() {
                        return Err(format!(
                            "'{}' is agent '{agent}''s own MCP registration (container-runtime \
                             §3.3); its URL is derived from the agent's service and this \
                             gateway's bind address, and lmgw keeps it in step with both, so it \
                             cannot be set here.",
                            cur.name
                        ));
                    }
                    reject_self_loop(&snap, &u)?;
                    Some(u)
                }
                None => cur.url.clone(),
            };
            // Whichever action this is, a move of the row's address checks
            // the stored headers against the call's (V-1); `null` restates
            // nothing, it keeps the stored ones (V-2).
            if let Some(why) = credential_move::mcp_server_refusal(
                writer,
                &cur,
                url.as_deref(),
                p.headers.as_deref(),
            ) {
                return Err(why);
            }
            let tool_prefix = opt(&p.tool_prefix).unwrap_or(cur.tool_prefix.clone());
            validate_tool_prefix(&tool_prefix)?;
            if tool_prefix != cur.tool_prefix {
                reject_device_namespace(&snap, &tool_prefix)?;
            }

            // Redacted-on-read fields are only rewritten when supplied, so a
            // read→partial-write round trip can't persist the `<set>` marker.
            let env = match p.env.as_deref() {
                Some(raw) => parse_env(raw)?,
                None => cur.env,
            };
            let headers = match p.headers.as_deref() {
                Some(raw) => parse_headers(raw)?,
                None => cur.headers,
            };
            let renamed = opt(&p.name);
            // Renaming *into* the reserved space is the same problem as
            // creating there; renaming an agent's own row *out* of it would
            // orphan the registration the agent lifecycle looks for by name.
            if let Some(n) = renamed.as_deref().filter(|n| *n != cur.name) {
                reject_device_label(&snap, n)?;
            }
            if let Some(n) = renamed.as_deref() {
                reject_agent_name(n)?;
            }
            if cur.agent_id.is_some() && renamed.is_some() {
                return Err(format!(
                    "'{}' is agent '{}''s own MCP registration (container-runtime §3.3); lmgw \
                     keeps it in step with the manifest, so it cannot be renamed here. Edit the \
                     agent's `run.provides.mcp` instead.",
                    cur.name,
                    cur.agent_id.as_deref().unwrap_or_default()
                ));
            }
            let new = NewMcpServer {
                name: renamed.unwrap_or(cur.name),
                enabled,
                transport,
                command: opt(&p.command).or(cur.command),
                args: match p.args.as_deref() {
                    Some(raw) => argv::parse_args_text(raw)?,
                    None => cur.args,
                },
                env,
                cwd: opt(&p.cwd).or(cur.cwd),
                container_image: opt(&p.container_image).or(cur.container_image),
                extra_run_args: match p.extra_run_args.as_deref() {
                    Some(raw) => argv::parse_args_text(raw)?,
                    None => cur.extra_run_args,
                },
                url,
                headers,
                tool_prefix,
                timeout_ms: p.timeout_ms.unwrap_or(cur.timeout_ms),
                autostart: p.autostart.unwrap_or(cur.autostart),
                idle_seconds: p.idle_seconds.unwrap_or(cur.idle_seconds),
                allow_sampling: p.allow_sampling.unwrap_or(cur.allow_sampling),
                sampling_alias: opt(&p.sampling_alias).or(cur.sampling_alias),
                // Kept, not editable: an agent's row stays its agent's.
                agent_id: cur.agent_id,
            };
            store::update_mcp_server(&state.db, id, &new)
                .await
                .map_err(|e| e.to_string())?;
            // Reconciles the live connection (start/stop/restart) as a side
            // effect, and fires tools/list_changed to every /mcp subscriber.
            state.reload_snapshot().await.map_err(|e| e.to_string())?;
            Ok(
                json!({ "ok": true, "id": id, "message": format!("MCP server '{}' updated", new.name) }),
            )
        }
        "delete" => {
            let id = id_of()?;
            // As with upstreams: confirm the row exists rather than reporting a
            // no-op delete as done.
            let cur = store::get_mcp_server(&state.db, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no MCP server with id {id}"))?;
            if cur.is_device() {
                return Err(device_row::delete_refusal(&cur));
            }
            store::delete_mcp_server(&state.db, id)
                .await
                .map_err(|e| e.to_string())?;
            // Its MCP tasks ended with the row: the results enter their
            // threads (MCP Tasks design §1.6).
            crate::web::chat_tasks::servers_gone(state).await;
            state.reload_snapshot().await.map_err(|e| e.to_string())?;
            Ok(json!({
                "ok": true, "id": id,
                "message": format!("MCP server '{}' deleted", cur.name),
            }))
        }
        "test" => {
            let id = id_of()?;
            let server = store::get_mcp_server(&state.db, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no MCP server with id {id}"))?;
            match state.mcp.test_connection(&server).await {
                Ok(n) => Ok(json!({
                    "ok": true, "id": id, "tool_count": n,
                    "message": format!("connected to '{}' — {n} tools", server.name),
                })),
                Err(e) => Err(format!("test failed for '{}': {e}", server.name)),
            }
        }
        other => Err(format!(
            "unknown action '{other}' (create|update|delete|enable|disable|test)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiKey, ApiKeyKind, KeyPolicy, Snapshot};

    /// A server's name whose spelling as a prefix runs into a device's
    /// label is refused, as the label itself is: a collision would put its
    /// tools among the device's.
    #[test]
    fn a_name_spelt_into_a_device_s_namespace_is_refused() {
        let mut snap = Snapshot::default();
        snap.api_keys.push(ApiKey {
            id: 7,
            name: "device:phone".into(),
            key_hash: String::new(),
            enabled: true,
            kind: ApiKeyKind::Device,
            key_plain: None,
            agent_id: None,
            policy: KeyPolicy::default(),
            note: String::new(),
            hosts_label: Some("phone".into()),
            self_admin: crate::config::DeviceAdmin::Off,
        });
        for name in ["phone", "PHONE", "phone.", "phone_", "phone!"] {
            let e = reject_device_label(&snap, name).expect_err(name);
            assert!(e.contains("hosting label 'phone'"), "{name}: {e}");
        }
        for name in ["phones", "my phone", "phon", "phone x"] {
            assert_eq!(reject_device_label(&snap, name), Ok(()), "{name}");
        }
    }
}
