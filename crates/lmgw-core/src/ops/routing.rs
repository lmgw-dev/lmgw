//! Alias routing: `upstream_set` / `model_set`

use serde::Deserialize;
use serde_json::{json, Value};

use crate::ir::Params;
use crate::state::SharedState;
use crate::store::{self, NewAlias, NewUpstream};

use super::*;

/// Sparse patch for an upstream. `action` selects the verb; the remaining
/// fields are the record, all optional (see the module note on sparseness).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
// The advertised inputSchema is closed (`additionalProperties: false`), so an
// argument we don't know is an error, not something to drop on the floor: a
// caller reaching for a field that doesn't exist (`self_admin` on a settings
// patch, say) must be told, never silently reported success.
#[serde(default, deny_unknown_fields)]
pub struct UpstreamPatch {
    pub action: String,
    pub id: Option<i64>,
    pub name: Option<String>,
    pub protocol: Option<String>,
    pub kind: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout_ms: Option<u64>,
    pub enabled: Option<bool>,
    pub expose_all: Option<bool>,
    pub expose_prefix: Option<String>,
    pub supports_responses: Option<bool>,
}

pub async fn upstream_set(state: &SharedState, p: UpstreamPatch) -> Result<Value, String> {
    let id_of = |p: &UpstreamPatch| p.id.ok_or_else(|| format!("'{}' requires id", p.action));

    match p.action.as_str() {
        "create" => {
            require_all(&[
                ("name", p.name.is_some()),
                ("base_url", p.base_url.is_some()),
                (
                    "protocol (openai|anthropic|gemini|llama_cpp)",
                    p.protocol.is_some(),
                ),
            ])?;
            let name = opt(&p.name).ok_or("name must not be empty")?;
            let base_url = opt(&p.base_url).ok_or("base_url must not be empty")?;
            let protocol =
                sent_protocol(p.protocol.as_deref())?.ok_or("protocol must not be empty")?;
            let settled = settle(
                Some(protocol),
                sent_kind(p.kind.as_deref())?,
                p.supports_responses,
                UpstreamShape::NEW_ROW,
            )?;
            let new = NewUpstream {
                name: name.clone(),
                protocol: settled.shape.protocol,
                kind: settled.shape.kind,
                base_url,
                api_key: opt(&p.api_key),
                extra_headers: vec![],
                timeout_ms: p.timeout_ms.unwrap_or(120_000),
                enabled: p.enabled.unwrap_or(true),
                expose_all: p.expose_all.unwrap_or(false),
                expose_prefix: opt(&p.expose_prefix).unwrap_or_default(),
                supports_responses: settled.shape.supports_responses,
            };
            let id = store::insert_upstream(&state.db, &new)
                .await
                .map_err(|e| e.to_string())?;
            state.reload_snapshot().await.map_err(|e| e.to_string())?;
            state.catalog.invalidate(id).await;
            state.llama_facts.invalidate(id);
            let message = settled.message(format!("upstream '{name}' created"));
            Ok(json!({ "ok": true, "id": id, "message": message }))
        }
        "update" | "enable" | "disable" => {
            let id = id_of(&p)?;
            let cur = store::get_upstream(&state.db, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no upstream with id {id}"))?;
            let enabled = match p.action.as_str() {
                "enable" => true,
                "disable" => false,
                _ => p.enabled.unwrap_or(cur.enabled),
            };
            // From what was sent, not from the merged row (design §5).
            let settled = settle(
                sent_protocol(p.protocol.as_deref())?,
                sent_kind(p.kind.as_deref())?,
                p.supports_responses,
                UpstreamShape::of(&cur),
            )?;
            // Only rewrite the key when one was actually supplied — otherwise a
            // partial update would wipe it (or persist the `<set>` placeholder).
            let new_key = opt(&p.api_key);
            let update_key = new_key.is_some();
            let new = NewUpstream {
                name: opt(&p.name).unwrap_or(cur.name),
                protocol: settled.shape.protocol,
                kind: settled.shape.kind,
                base_url: opt(&p.base_url).unwrap_or(cur.base_url),
                api_key: new_key,
                extra_headers: cur.extra_headers,
                timeout_ms: p.timeout_ms.unwrap_or(cur.timeout_ms),
                enabled,
                expose_all: p.expose_all.unwrap_or(cur.expose_all),
                expose_prefix: opt(&p.expose_prefix).unwrap_or(cur.expose_prefix),
                supports_responses: settled.shape.supports_responses,
            };
            store::update_upstream(&state.db, id, &new, update_key)
                .await
                .map_err(|e| e.to_string())?;
            state.reload_snapshot().await.map_err(|e| e.to_string())?;
            state.catalog.invalidate(id).await;
            state.llama_facts.invalidate(id);
            let message = settled.message(format!("upstream {id} updated"));
            Ok(json!({ "ok": true, "id": id, "message": message }))
        }
        "delete" => {
            let id = id_of(&p)?;
            // SQL `DELETE` on a missing row affects zero rows and succeeds. The
            // dashboard only ever deletes rows it just rendered, but here the id
            // comes from the caller — so reporting "deleted" for an id that was
            // never there would confirm work that didn't happen.
            let cur = store::get_upstream(&state.db, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no upstream with id {id}"))?;
            store::delete_upstream(&state.db, id)
                .await
                .map_err(|e| e.to_string())?;
            state.reload_snapshot().await.map_err(|e| e.to_string())?;
            state.catalog.invalidate(id).await;
            state.llama_facts.invalidate(id);
            Ok(json!({
                "ok": true, "id": id,
                "message": format!("upstream '{}' deleted", cur.name),
            }))
        }
        other => Err(format!(
            "unknown action '{other}' (create|update|delete|enable|disable)"
        )),
    }
}

/// Sparse patch for a model alias.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
// The advertised inputSchema is closed (`additionalProperties: false`), so an
// argument we don't know is an error, not something to drop on the floor: a
// caller reaching for a field that doesn't exist (`self_admin` on a settings
// patch, say) must be told, never silently reported success.
#[serde(default, deny_unknown_fields)]
pub struct AliasPatch {
    pub action: String,
    pub id: Option<i64>,
    pub alias: Option<String>,
    /// Upstream by **name or numeric id** — a name is what a caller reading
    /// `lmgw__upstreams` already has in hand.
    pub upstream: Option<String>,
    pub upstream_model: Option<String>,
    pub enabled: Option<bool>,
    /// Owner override of the derived `/v1/models` capability facts
    /// (model-capabilities design §7): a JSON object, or a JSON string
    /// containing one (MCP arguments are flat scalars) — see
    /// [`parse_capabilities_override`]. `null`/empty clears.
    pub capabilities_override: Option<Value>,
    /// Field names to reset to unset, comma- or space-separated. Only
    /// `capabilities_override` is clearable here today.
    pub clear: Option<String>,
}

/// Resolve an upstream reference that may be a numeric id or a name.
async fn resolve_upstream(state: &SharedState, r: &str) -> Result<i64, String> {
    if let Ok(id) = r.parse::<i64>() {
        if store::get_upstream(&state.db, id)
            .await
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Ok(id);
        }
        return Err(format!("no upstream with id {id}"));
    }
    store::get_upstream_by_name(&state.db, r)
        .await
        .map_err(|e| e.to_string())?
        .map(|u| u.id)
        .ok_or_else(|| format!("no upstream named '{r}'"))
}

pub async fn model_set(state: &SharedState, p: AliasPatch) -> Result<Value, String> {
    let snap = state.snapshot();
    let find = |id: Option<i64>, alias: Option<String>, all: Vec<crate::config::ModelAlias>| {
        if let Some(id) = id {
            return all
                .into_iter()
                .find(|a| a.id == id)
                .ok_or_else(|| format!("no alias with id {id}"));
        }
        let alias = alias.ok_or("this action requires id or alias")?;
        all.into_iter()
            .find(|a| a.alias.eq_ignore_ascii_case(&alias))
            .ok_or_else(|| format!("no alias named '{alias}'"))
    };

    match p.action.as_str() {
        "create" => {
            require_all(&[
                ("alias", p.alias.is_some()),
                ("upstream (name or id)", p.upstream.is_some()),
                ("upstream_model", p.upstream_model.is_some()),
            ])?;
            let alias = opt(&p.alias).ok_or("alias must not be empty")?;
            refuse_if_candidate_alias_name(&snap, &alias)?;
            let upstream = opt(&p.upstream).ok_or("upstream must not be empty")?;
            let upstream_id = resolve_upstream(state, &upstream).await?;
            let upstream_model =
                opt(&p.upstream_model).ok_or("upstream_model must not be empty")?;
            let capabilities_override = match p.capabilities_override.as_ref() {
                Some(v) => parse_capabilities_override(v)?,
                None => None,
            };
            let new = NewAlias {
                alias: alias.clone(),
                upstream_id,
                upstream_model_id: upstream_model,
                param_overrides: Params::default(),
                enabled: p.enabled.unwrap_or(true),
                capabilities_override,
            };
            let id = store::insert_alias(&state.db, &new)
                .await
                .map_err(|e| e.to_string())?;
            state.reload_snapshot().await.map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true, "id": id, "message": format!("alias '{alias}' created") }))
        }
        "update" | "enable" | "disable" => {
            let all = store::list_aliases(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            let cur = find(p.id, opt(&p.alias), all)?;
            let enabled = match p.action.as_str() {
                "enable" => true,
                "disable" => false,
                _ => p.enabled.unwrap_or(cur.enabled),
            };
            let upstream_id = match opt(&p.upstream) {
                Some(r) => resolve_upstream(state, &r).await?,
                None => cur.upstream_id,
            };
            // `alias` doubles as the lookup key, so on update it only renames
            // when the caller also passed an explicit id to select the row.
            let new_alias = match (p.id, opt(&p.alias)) {
                (Some(_), Some(a)) => a,
                _ => cur.alias.clone(),
            };
            // Only a rename can newly collide with a candidate alias — an
            // unrelated save (enable/disable, upstream change, …) of a row
            // whose name a *later*-created candidate alias happens to shadow
            // must not be refused forever over a name it never chose to
            // change (review finding X4).
            if new_alias != cur.alias {
                refuse_if_candidate_alias_name(&snap, &new_alias)?;
            }
            for name in clear_names(p.clear.as_deref()) {
                if name != "capabilities_override" {
                    return Err(format!("clear: '{name}' is not a clearable field name"));
                }
            }
            let capabilities_override = if clear_has(p.clear.as_deref(), "capabilities_override") {
                None
            } else {
                match p.capabilities_override.as_ref() {
                    Some(v) => parse_capabilities_override(v)?,
                    None => cur.capabilities_override.clone(),
                }
            };
            let new = NewAlias {
                alias: new_alias,
                upstream_id,
                upstream_model_id: opt(&p.upstream_model).unwrap_or(cur.upstream_model_id),
                param_overrides: cur.param_overrides,
                enabled,
                capabilities_override,
            };
            store::update_alias(&state.db, cur.id, &new)
                .await
                .map_err(|e| e.to_string())?;
            state.reload_snapshot().await.map_err(|e| e.to_string())?;
            Ok(
                json!({ "ok": true, "id": cur.id, "message": format!("alias '{}' updated", new.alias) }),
            )
        }
        "delete" => {
            let all = store::list_aliases(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            let cur = find(p.id, opt(&p.alias), all)?;
            store::delete_alias(&state.db, cur.id)
                .await
                .map_err(|e| e.to_string())?;
            state.reload_snapshot().await.map_err(|e| e.to_string())?;
            Ok(
                json!({ "ok": true, "id": cur.id, "message": format!("alias '{}' deleted", cur.alias) }),
            )
        }
        other => Err(format!(
            "unknown action '{other}' (create|update|delete|enable|disable)"
        )),
    }
}
