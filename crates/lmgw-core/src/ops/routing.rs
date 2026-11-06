//! Alias routing: `upstream_set` / `model_set`

use lmgw_api_types as dto;
use serde::Deserialize;
use serde_json::Value;

use crate::config::HopKey;
use crate::detached::detached;
use crate::ir::Params;
use crate::state::SharedState;
use crate::store::{self, NewAlias, NewUpstream};

use super::credential_move::{self, RowWriter};
use super::*;

/// Sparse patch for an upstream. `action` selects the verb; the remaining
/// fields are the record, all optional (a field left out keeps its value).
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
    /// The API the upstream speaks. `openai`: the OpenAI platform (api.openai.com)
    /// only. `openai_compatible`: every other OpenAI-shaped provider (OpenRouter,
    /// vLLM, an external audio.cpp server…). `anthropic`: Anthropic's API.
    /// `gemini`: Google's Gemini API. `llama_cpp`: llama.cpp's own server.
    pub protocol: Option<String>,
    /// What the upstream is, tied to its protocol: `openai` is `generic` only;
    /// `openai_compatible` is `generic` or `audio_cpp` (an external audio.cpp
    /// server); `llama_cpp` is `llama_server` only; `anthropic` is `generic` or
    /// `llama_server`; `gemini` is `generic`. A pairing outside this is refused.
    pub kind: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout_ms: Option<u64>,
    pub enabled: Option<bool>,
    pub expose_all: Option<bool>,
    pub expose_prefix: Option<String>,
    /// Forward `/v1/responses` natively instead of lmgw's own loop. Applies to
    /// `openai_compatible` upstreams only; on any other protocol it is refused
    /// when turned on and switched off when the row leaves `openai_compatible`.
    pub supports_responses: Option<bool>,
    /// Ask the catalog for every output modality (`GET …/models?output_modalities=all`),
    /// so OpenRouter lists its decision, embedding, rerank, speech,
    /// transcription, image and video models too. On unless turned off; only
    /// an `openai_compatible` row of kind `generic` sends it. Off for an
    /// endpoint that refuses the parameter, such as Gemini's
    /// OpenAI-compatible one.
    pub catalog_all_modalities: Option<bool>,
}

/// `writer` is who asks: a self-admin tool's move of a keyed row to another
/// address must restate the key ([`RowWriter`]).
/// `upstream_quota_clear { id }`.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpstreamQuotaClear {
    /// Upstream id, from `/api/upstreams`.
    pub id: i64,
}

/// Forget that the upstream `p.id` is out of quota (openai egress design
/// §14.6, the Upstreams page's Check now): the next request goes to it
/// again.
pub async fn upstream_quota_clear(
    state: &SharedState,
    p: UpstreamQuotaClear,
) -> Result<dto::RowSaved, String> {
    let u = store::get_upstream(&state.db, p.id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no upstream with id {}", p.id))?;
    let message = if state
        .upstream_quota
        .clear(u.id, &u.name, "the owner asked to check now")
    {
        format!(
            "upstream '{}' is no longer remembered out of quota: the next request goes to it",
            u.name
        )
    } else {
        format!("upstream '{}' was not remembered out of quota", u.name)
    };
    Ok(dto::RowSaved::new(u.id, message))
}

pub async fn upstream_set(
    state: &SharedState,
    p: UpstreamPatch,
    writer: RowWriter,
) -> Result<dto::RowSaved, String> {
    let id_of = |p: &UpstreamPatch| p.id.ok_or_else(|| format!("'{}' requires id", p.action));
    // From the snapshot the checks read to the commit (§14.2).
    let routing = state.routing_write().await;

    match p.action.as_str() {
        "create" => {
            require_all(&[
                ("name", p.name.is_some()),
                ("base_url", p.base_url.is_some()),
                (
                    "protocol (openai|openai_compatible|anthropic|gemini|llama_cpp)",
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
            )?
            .with_host_note(&base_url);
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
                catalog_all_modalities: p.catalog_all_modalities.unwrap_or(true),
            };
            let message = settled.message(format!("upstream '{name}' created"));
            // An `expose_all` upstream answers the names under its prefix,
            // which a dangling fallback may hold: a dry run (§14.2).
            let mut tx = store::begin_write(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            let dry = store::insert_upstream_in(&mut tx, &new)
                .await
                .map_err(|e| e.to_string())?;
            super::fallback::dry_run(
                state,
                &state.snapshot(),
                tx,
                &[super::fallback::Touch::Upstream(dry)],
                None,
            )
            .await?;
            // The write and everything after it are a task of their own
            // (*changed 2026-10-24*, see [`crate::detached::detached`]): a
            // dropped request would leave the snapshot without the new
            // upstream and its catalog and facts caches unflushed.
            let st = state.clone();
            detached("creating the upstream", async move {
                let id = store::insert_upstream(&st.db, &new)
                    .await
                    .map_err(|e| e.to_string())?;
                // Published first, then the caches flushed, then MCP reconciled:
                // the reconcile can take seconds, and a request meanwhile must not
                // read the replaced upstream's catalog or facts.
                let snap = st.publish_snapshot().await.map_err(|e| e.to_string())?;
                st.catalog.invalidate(id).await;
                st.llama_facts.invalidate(id);
                drop(routing);
                st.mcp.reconcile(&snap).await;
                Ok(dto::RowSaved::new(id, message))
            })
            .await
            .map_err(|r| r.message)?
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
            // Whichever action this is, a move of the row's address checks
            // the stored key against the one the call sends.
            let base_url = opt(&p.base_url).unwrap_or_else(|| cur.base_url.clone());
            // From what was sent, not from the merged row (design §5).
            let settled = settle(
                sent_protocol(p.protocol.as_deref())?,
                sent_kind(p.kind.as_deref())?,
                p.supports_responses,
                UpstreamShape::of(&cur),
            )?
            .with_host_note(&base_url);
            // Only rewrite the key when one was actually supplied — otherwise a
            // partial update would wipe it (or persist the `<set>` placeholder).
            let new_key = opt(&p.api_key);
            let update_key = new_key.is_some();
            if let Some(why) =
                credential_move::upstream_refusal(writer, &cur, &base_url, new_key.as_deref())
            {
                return Err(why);
            }
            let new = NewUpstream {
                name: opt(&p.name).unwrap_or(cur.name),
                protocol: settled.shape.protocol,
                kind: settled.shape.kind,
                base_url,
                api_key: new_key,
                extra_headers: cur.extra_headers,
                timeout_ms: p.timeout_ms.unwrap_or(cur.timeout_ms),
                enabled,
                expose_all: p.expose_all.unwrap_or(cur.expose_all),
                expose_prefix: opt(&p.expose_prefix).unwrap_or(cur.expose_prefix),
                supports_responses: settled.shape.supports_responses,
                // Unsent keeps what the row has: the snapshot names the rows
                // where it is off.
                catalog_all_modalities: p
                    .catalog_all_modalities
                    .unwrap_or(!state.snapshot().catalog_bare.contains(&id)),
            };
            let snap = state.snapshot();
            // A new prefix renames every passthrough id of this upstream:
            // the fallbacks naming one follow it (decision 27).
            let rewrites =
                super::fallback::upstream_prefix_rewrites(state, &snap, id, &new.expose_prefix)
                    .await;
            let message =
                settled.message(format!("upstream '{}' {}", new.name, saved_verb(&p.action)));
            // What a prefix, an enable or an `expose_all` change does to the
            // fallbacks naming this upstream's models, in a dry run (§14.2).
            let mut tx = store::begin_write(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            store::update_upstream_in(&mut tx, id, &new, update_key)
                .await
                .map_err(|e| e.to_string())?;
            store::rewrite_fallbacks_in(&mut tx, &rewrites.rows)
                .await
                .map_err(|e| e.to_string())?;
            super::fallback::dry_run(
                state,
                &snap,
                tx,
                &[super::fallback::Touch::Upstream(id)],
                None,
            )
            .await?;
            // The write and everything after it are a task of their own
            // (*changed 2026-10-24*, see [`crate::detached::detached`]): a
            // dropped request would leave the snapshot serving the old row
            // and its catalog and facts caches unflushed.
            let st = state.clone();
            detached("updating the upstream", async move {
                let mut tx = store::begin_write(&st.db)
                    .await
                    .map_err(|e| e.to_string())?;
                store::update_upstream_in(&mut tx, id, &new, update_key)
                    .await
                    .map_err(|e| e.to_string())?;
                store::rewrite_fallbacks_in(&mut tx, &rewrites.rows)
                    .await
                    .map_err(|e| e.to_string())?;
                tx.commit().await.map_err(|e| e.to_string())?;
                // Published first, then the caches flushed, then MCP reconciled:
                // the reconcile can take seconds, and a request meanwhile must not
                // read the replaced upstream's catalog or facts.
                let snap = st.publish_snapshot().await.map_err(|e| e.to_string())?;
                st.catalog.invalidate(id).await;
                st.llama_facts.invalidate(id);
                drop(routing);
                // The key may have changed (openai egress design §14.6).
                st.upstream_quota.clear(id, &new.name, "its row was edited");
                st.mcp.reconcile(&snap).await;
                Ok(dto::RowSaved::new(id, message).with_fallback_notes(rewrites.notes))
            })
            .await
            .map_err(|r| r.message)?
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
            // Its aliases go with it: the rows whose fallback names one of
            // them, or one of its passthrough ids, are named ("confirm, then
            // do", §14.2).
            let fallback_notes = super::fallback::upstream_delete_notes(&state.snapshot(), id);
            // The delete and everything after it are a task of their own
            // (*changed 2026-10-24*, see [`crate::detached::detached`]): a
            // dropped request would leave the snapshot serving the deleted
            // row and its catalog and facts caches unflushed.
            let st = state.clone();
            detached("deleting the upstream", async move {
                store::delete_upstream(&st.db, id)
                    .await
                    .map_err(|e| e.to_string())?;
                // Published first, then the caches flushed, then MCP reconciled:
                // the reconcile can take seconds, and a request meanwhile must not
                // read the replaced upstream's catalog or facts.
                let snap = st.publish_snapshot().await.map_err(|e| e.to_string())?;
                st.catalog.invalidate(id).await;
                st.llama_facts.invalidate(id);
                drop(routing);
                st.upstream_quota
                    .clear(id, &cur.name, "its row was deleted");
                st.mcp.reconcile(&snap).await;
                Ok(
                    dto::RowSaved::new(id, format!("upstream '{}' deleted", cur.name))
                        .with_fallback_notes(fallback_notes),
                )
            })
            .await
            .map_err(|r| r.message)?
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
    /// Override of the derived `/v1/models` capability facts: a JSON object,
    /// or a JSON string containing one (MCP arguments are flat scalars).
    /// `null`/empty clears.
    pub capabilities_override: Option<Value>,
    /// The model or alias that answers when this alias cannot: its upstream
    /// out of quota or unreachable, the alias or its upstream disabled
    /// (openai egress design §14.2). A local model id, a passthrough id or
    /// another plain alias of the same model type; not a candidate alias.
    /// Chains are allowed; a loop is refused. `clear: "fallback"` removes it.
    pub fallback: Option<String>,
    /// Field names to reset to unset, comma- or space-separated:
    /// `capabilities_override` and `fallback`.
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

/// Create, update or delete an alias. Each write is followed, in the task
/// it runs as, by the reload and by `/mcp` sessions told to list again: the
/// image and audio tools list the rows and aliases that serve them
/// (chat-media-tools design §3.2).
pub async fn model_set(state: &SharedState, p: AliasPatch) -> Result<dto::RowSaved, String> {
    // From the snapshot the checks read to the commit (§14.2).
    let routing = state.routing_write().await;
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
            let save = super::fallback::settle_alias(
                state,
                &snap,
                None,
                &alias,
                upstream_id,
                &upstream_model,
                &Params::default(),
                capabilities_override.as_ref(),
                p.clear.as_deref(),
                &p.fallback,
            )
            .await?;
            let fallback_notes = save.notes;
            let new = NewAlias {
                alias: alias.clone(),
                upstream_id,
                upstream_model_id: upstream_model,
                param_overrides: Params::default(),
                enabled: p.enabled.unwrap_or(true),
                capabilities_override,
                fallback: save.fallback,
            };
            // A fallback that holds the new name reaches this alias from now
            // on, and an alias shadows a local name of its spelling: a dry
            // run (§14.2, `fallback::referrers`).
            let mut tx = store::begin_write(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            let dry = store::insert_alias_in(&mut tx, &new)
                .await
                .map_err(|e| e.to_string())?;
            let own = (store::FallbackTable::Models, dry);
            super::fallback::dry_run(
                state,
                &snap,
                tx,
                &[super::fallback::Touch::Row(own.0, dry)],
                Some(own),
            )
            .await?;
            // The write and everything after it are a task of their own
            // (*changed 2026-10-24*, see [`crate::detached::detached`]): a
            // dropped request would leave the snapshot without the new alias.
            let st = state.clone();
            detached("creating the alias", async move {
                let id = store::insert_alias(&st.db, &new)
                    .await
                    .map_err(|e| e.to_string())?;
                st.reload_snapshot_releasing(routing)
                    .await
                    .map_err(|e| e.to_string())?;
                st.mcp.notify_tools_changed();
                let mut notes = fallback_notes;
                if let Some(f) = &new.fallback {
                    notes.extend(super::fallback::facet_notes(&st, &alias, f).await);
                }
                Ok(dto::RowSaved::new(id, format!("alias '{alias}' created"))
                    .with_fallback_notes(notes))
            })
            .await
            .map_err(|r| r.message)?
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
                if !matches!(name, "capabilities_override" | "fallback") {
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
            let upstream_model_id = opt(&p.upstream_model).unwrap_or(cur.upstream_model_id.clone());
            let save = super::fallback::settle_alias(
                state,
                &snap,
                Some(&cur),
                &new_alias,
                upstream_id,
                &upstream_model_id,
                &cur.param_overrides,
                capabilities_override.as_ref(),
                p.clear.as_deref(),
                &p.fallback,
            )
            .await?;
            let (fallback_notes, rewrites, fallback_changed, checked) =
                (save.notes, save.rewrites, save.changed, save.checked);
            let new = NewAlias {
                alias: new_alias,
                upstream_id,
                upstream_model_id,
                param_overrides: cur.param_overrides,
                enabled,
                capabilities_override,
                fallback: save.fallback,
            };
            // What a rename, a retarget, an enable or a disable does to the
            // fallbacks naming this alias's name, in a dry run (§14.2).
            let mut tx = store::begin_write(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            store::update_alias_in(&mut tx, cur.id, &new)
                .await
                .map_err(|e| e.to_string())?;
            store::rewrite_fallbacks_in(&mut tx, &rewrites.rows)
                .await
                .map_err(|e| e.to_string())?;
            let own = (store::FallbackTable::Models, cur.id);
            super::fallback::dry_run(
                state,
                &snap,
                tx,
                &[super::fallback::Touch::Row(own.0, own.1)],
                checked.then_some(own),
            )
            .await?;
            let message = format!("alias '{}' {}", new.alias, saved_verb(&p.action));
            // The write and everything after it are a task of their own
            // (*changed 2026-10-24*, see [`crate::detached::detached`]): a
            // dropped request would leave the snapshot serving the old alias.
            let st = state.clone();
            detached("updating the alias", async move {
                let mut tx = store::begin_write(&st.db)
                    .await
                    .map_err(|e| e.to_string())?;
                store::update_alias_in(&mut tx, cur.id, &new)
                    .await
                    .map_err(|e| e.to_string())?;
                store::rewrite_fallbacks_in(&mut tx, &rewrites.rows)
                    .await
                    .map_err(|e| e.to_string())?;
                tx.commit().await.map_err(|e| e.to_string())?;
                st.reload_snapshot_releasing(routing)
                    .await
                    .map_err(|e| e.to_string())?;
                st.mcp.notify_tools_changed();
                let mut notes = fallback_notes;
                if let (Some(f), true) = (&new.fallback, fallback_changed) {
                    notes.extend(super::fallback::facet_notes(&st, &new.alias, f).await);
                }
                Ok(dto::RowSaved::new(cur.id, message).with_fallback_notes(notes))
            })
            .await
            .map_err(|r| r.message)?
        }
        "delete" => {
            let all = store::list_aliases(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            let cur = find(p.id, opt(&p.alias), all)?;
            let fallback_notes = super::fallback::delete_notes(&snap, &HopKey::Alias(cur.id));
            // The delete and everything after it are a task of their own
            // (*changed 2026-10-24*, see [`crate::detached::detached`]): a
            // dropped request would leave the snapshot serving the deleted
            // alias.
            let st = state.clone();
            detached("deleting the alias", async move {
                store::delete_alias(&st.db, cur.id)
                    .await
                    .map_err(|e| e.to_string())?;
                st.reload_snapshot_releasing(routing)
                    .await
                    .map_err(|e| e.to_string())?;
                st.mcp.notify_tools_changed();
                Ok(
                    dto::RowSaved::new(cur.id, format!("alias '{}' deleted", cur.alias))
                        .with_fallback_notes(fallback_notes),
                )
            })
            .await
            .map_err(|r| r.message)?
        }
        other => Err(format!(
            "unknown action '{other}' (create|update|delete|enable|disable)"
        )),
    }
}
