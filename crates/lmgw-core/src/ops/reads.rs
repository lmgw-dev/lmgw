//! Reads

use serde_json::{json, Value};

use crate::candidates;
use crate::capabilities::{self};
use crate::state::SharedState;
use crate::store::{self, LogFilter};

use super::*;

/// Gateway health in one call: uptime, live request stats, the per-model
/// container runtime, the GPU ledger, southbound MCP connections, and
/// config-object counts.
pub async fn status(state: &SharedState) -> Result<Value, String> {
    let snap = state.snapshot();
    let stats = state.telemetry.stats();
    let vram = state.vram.view(state).await;
    let mcp = state.mcp.status_views(&snap).await;

    let mut out = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "bind_addr": snap.settings.bind_addr,
        "auth_enabled": snap.settings.auth_enabled,
        "self_admin": snap.settings.self_admin.as_str(),
        "requests": {
            "total": stats.total_requests,
            "errors": stats.total_errors,
            "active": stats.active_requests,
            "last_minute": stats.req_last_minute,
            "errors_last_minute": stats.err_last_minute,
            "prompt_tokens": stats.prompt_tokens,
            "completion_tokens": stats.completion_tokens,
        },
        // One row per model lmgw believes is running (per-model-containers
        // §3.2/§8) — replaces the old fixed three-container list now that a
        // "container" is a per-model, not per-class, concept. Candidate
        // aliases (§4.4) are visible here too: an entry's `owner` names
        // "background" while a background alias's start owns it and nobody
        // has claimed it since, and `draining_for_owner` while an owner
        // admission is waiting on it — both absent (the pre-phase-4 shape)
        // on every other entry.
        "runtime": state.runtime().list(),
        // The GPU those containers share (§9b): what is resident, what it is
        // measured to cost, and anything queued behind it. Reported even when
        // admission is inactive — "inactive, and here is why" is the answer an
        // owner needs on a host where NVML did not load.
        "vram": vram,
        "mcp_servers": mcp,
        "counts": {
            "upstreams": snap.upstreams.len(),
            "aliases": snap.aliases.len(),
            "local_models": snap.local_models.len(),
            "mcp_servers": snap.mcp_servers.len(),
            "api_keys": snap.api_keys.len(),
        },
    });

    // Candidate aliases (§6): only when at least one is configured, so an
    // install without any reads exactly as it did before this key existed —
    // no empty array, no key at all.
    if !snap.candidate_aliases.is_empty() {
        let deferral_counts = candidates::deferrals::deferrals_24h(&state.db)
            .await
            .map_err(|e| e.to_string())?;
        let mut candidate_aliases: Vec<Value> = Vec::new();
        for c in snap.candidate_aliases.values() {
            let d = candidates::derive::derive(state, &snap, c).await;
            candidate_aliases.push(json!({
                "name": c.alias,
                "mode": if c.background { "background" } else { "owner" },
                "primary": c.primary(),
                "alternates": c.candidates.iter().skip(1).collect::<Vec<_>>(),
                "enabled_facets": d.enabled.names(),
                "problems": d.problems,
                "deferrals_24h": candidates::deferrals::for_alias(&deferral_counts, &c.alias),
            }));
        }
        candidate_aliases.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        out["candidate_aliases"] = json!(candidate_aliases);
    }

    Ok(out)
}

/// Model names the gateway can route, by source. `kind` is one of
/// `all | alias | local | aux | audio` (`embed` is accepted for `aux`);
/// `search` is a case-insensitive substring filter over the client-facing name.
///
/// Statically-known names only — passthrough catalogs (`expose_all` upstreams)
/// require live upstream HTTP calls and are reported as a per-upstream note
/// instead of being fetched here (their catalogs are still fetched once, in
/// the [`capabilities::exposed::exposed_entries`] call below, so an
/// expose-all upstream that is reachable pays the same cached HTTP cost that
/// `GET /v1/models` does — see the tool description).
pub async fn models(
    state: &SharedState,
    kind: Option<&str>,
    search: Option<&str>,
) -> Result<Value, String> {
    let snap = state.snapshot();
    // One pass over the same builder `/v1/models` renders from (design §8
    // item 9): every entry below is joined onto its capabilities by name
    // rather than re-derived, so the two views of "what this model can do"
    // cannot drift apart. Passthrough-catalog rows are not in this map either
    // (`exposed_entries` only lists them when a name isn't already claimed by
    // an alias/local/aux/audio row, and those are exactly the rows this
    // function itself does not list), so the "not listed" behaviour is
    // unchanged.
    let exposed: std::collections::HashMap<String, capabilities::exposed::ExposedEntry> =
        capabilities::exposed::exposed_entries(state)
            .await
            .into_iter()
            .map(|e| (e.name.clone(), e))
            .collect();
    let enrich = |mut v: Value| {
        if let Some(name) = v["name"].as_str() {
            if let Some(e) = exposed.get(name) {
                let obj = v.as_object_mut().expect("model entry is an object");
                if let Some(ctx) = e.context_length {
                    obj.insert("context_length".to_string(), json!(ctx));
                }
                if let Some(n) = e.max_output_tokens {
                    obj.insert("max_output_tokens".to_string(), json!(n));
                }
                if let Some(caps) = &e.capabilities {
                    obj.insert(
                        "capabilities".to_string(),
                        serde_json::to_value(caps).unwrap_or(Value::Null),
                    );
                }
                if !e.notes.is_empty() {
                    obj.insert("notes".to_string(), json!(e.notes));
                }
            }
        }
        v
    };
    // `embed` is the pre-rename spelling of the aux container's kind.
    let kind = match kind.unwrap_or("all") {
        "embed" => "aux",
        k => k,
    };
    if !matches!(kind, "all" | "alias" | "local" | "aux" | "audio" | "image") {
        return Err(format!(
            "unknown kind '{kind}' (expected all, alias, local, aux, audio or image)"
        ));
    }
    let needle = search.map(|s| s.to_lowercase());
    let matches = |name: &str| {
        needle
            .as_ref()
            .is_none_or(|n| name.to_lowercase().contains(n.as_str()))
    };

    let mut out: Vec<Value> = Vec::new();

    if kind == "all" || kind == "alias" {
        for a in snap.aliases.values() {
            if !matches(&a.alias) {
                continue;
            }
            let upstream = snap.upstreams.get(&a.upstream_id);
            out.push(json!({
                "name": a.alias,
                "kind": "alias",
                "enabled": a.enabled,
                "upstream": upstream.map(|u| u.name.clone()),
                "upstream_model": a.upstream_model_id,
            }));
        }
        // Candidate aliases (candidate-aliases design §4.1) share the
        // `alias`/`all` bucket rather than a fifth `kind` value — a caller
        // asking "what are the names I can send as `model`" wants both kinds
        // of alias together — but each entry carries its own `"kind":
        // "candidate_alias"` so the two never look interchangeable, and
        // every plain-alias entry above stays byte-identical.
        //
        // The deferral count (§6) is one grouped query for every alias, read
        // once here rather than once per alias below, and skipped entirely
        // when there are none configured.
        let deferral_counts = if snap.candidate_aliases.is_empty() {
            std::collections::HashMap::new()
        } else {
            candidates::deferrals::deferrals_24h(&state.db)
                .await
                .map_err(|e| e.to_string())?
        };
        for c in snap.candidate_aliases.values() {
            if !matches(&c.alias) {
                continue;
            }
            let d = candidates::derive::derive(state, &snap, c).await;
            out.push(json!({
                "name": c.alias,
                "kind": "candidate_alias",
                "id": c.id,
                "enabled": c.enabled,
                "candidates": c.candidates,
                "background": c.background,
                "fallback_mode": c.fallback_mode.as_str(),
                "fallback": c.fallback,
                "enabled_facets": d.enabled.names(),
                "common_facets": d.common.names(),
                "routable": d.routable,
                "problems": d.problems,
                "advisories": d.advisories,
                "fallback_usable": d.fallback_usable,
                "deferrals_24h": candidates::deferrals::for_alias(&deferral_counts, &c.alias),
            }));
        }
    }
    if kind == "all" || kind == "local" {
        for m in &snap.local_models {
            let name = snap.local_public_name(&m.model_id);
            if !matches(&name) {
                continue;
            }
            out.push(json!({
                "name": name,
                "kind": "local",
                "enabled": m.enabled,
                "public": m.public,
                "gguf_path": m.gguf_path,
                "id": m.id,
            }));
        }
    }
    if kind == "all" || kind == "aux" {
        for m in &snap.aux_models {
            let name = snap.aux_public_name(&m.model_id);
            if !matches(&name) {
                continue;
            }
            out.push(json!({
                "name": name, "kind": "aux", "aux_kind": m.kind.as_str(),
                "enabled": m.enabled, "id": m.id,
                "gguf_path": m.gguf_path, "pooling": m.pooling,
            }));
        }
    }
    if kind == "all" || kind == "audio" {
        for m in store::list_audio_models(&state.db)
            .await
            .map_err(|e| e.to_string())?
        {
            let prefix = snap.settings.audio.public_prefix.trim_matches('/');
            let name = if prefix.is_empty() {
                m.model_id.clone()
            } else {
                format!("{prefix}/{}", m.model_id)
            };
            if !matches(&name) {
                continue;
            }
            out.push(json!({
                "name": name, "kind": "audio", "enabled": m.enabled,
                "task": m.task, "family": m.family, "id": m.id,
            }));
        }
    }

    // The image class (image-generation design §8): a row is a set of files
    // rather than one path, so the listing names the pipeline's loader and
    // what it says it does — the whole map is `lmgw__local_model_get`'s.
    if kind == "all" || kind == "image" {
        for m in &snap.image_models {
            let name = snap.image_public_name(&m.model_id);
            if !matches(&name) {
                continue;
            }
            out.push(json!({
                "name": name, "kind": "image", "enabled": m.enabled, "id": m.id,
                "modes": m.modes(), "edit": m.edit,
                "loads": m.files.get("model").or_else(|| m.files.get("diffusion_model")),
            }));
        }
    }

    let mut out: Vec<Value> = out.into_iter().map(enrich).collect();
    out.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });

    // Passthrough upstreams serve names we can't enumerate without calling them.
    let passthrough: Vec<Value> = snap
        .upstreams
        .values()
        .filter(|u| u.enabled && u.expose_all)
        .map(|u| {
            json!({
                "upstream": u.name,
                "prefix": u.prefix(),
                "note": "expose_all upstream — request <prefix>/<model>; \
                         its full catalog is only known by calling the upstream",
            })
        })
        .collect();

    Ok(json!({ "models": out, "passthrough_upstreams": passthrough }))
}

/// Configured upstreams. API keys are reported as set/unset, never returned.
pub async fn upstreams(state: &SharedState) -> Result<Value, String> {
    let rows = store::list_upstreams(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let out: Vec<Value> = rows
        .iter()
        .map(|u| {
            json!({
                "id": u.id,
                "name": u.name,
                "protocol": u.protocol.as_str(),
                "kind": u.kind.as_str(),
                "base_url": u.base_url,
                "api_key": u.api_key.as_ref().map(|_| REDACTED),
                "extra_headers": redact_pairs(&u.extra_headers, ": "),
                "timeout_ms": u.timeout_ms,
                "enabled": u.enabled,
                "expose_all": u.expose_all,
                "expose_prefix": u.expose_prefix,
            })
        })
        .collect();
    Ok(json!({ "upstreams": out }))
}

/// Registered MCP servers: their config (secrets redacted) joined with the live
/// connection status and discovered tool count from the manager.
pub async fn mcp_servers(state: &SharedState) -> Result<Value, String> {
    let snap = state.snapshot();
    let views = state.mcp.status_views(&snap).await;
    let rows = store::list_mcp_servers(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    // A `dev_url` row is served by a process lmgw never started, so "the app
    // container is not running" is not what is going on and the page must not
    // say it (container-runtime §3.4).
    let dev = store::agent_dev_urls(&state.db).await.unwrap_or_default();
    let out: Vec<Value> = rows
        .iter()
        .map(|s| {
            let live = views.iter().find(|v| v.id == s.id);
            json!({
                "id": s.id,
                "name": s.name,
                "enabled": s.enabled,
                "transport": s.transport.as_str(),
                "command": s.command,
                "args": s.args.join("\n"),
                "env": redact_pairs(&s.env, "="),
                "cwd": s.cwd,
                "container_image": s.container_image,
                "extra_run_args": s.extra_run_args.join("\n"),
                "url": s.url,
                "headers": redact_pairs(&s.headers, ": "),
                "tool_prefix": s.tool_prefix,
                "timeout_ms": s.timeout_ms,
                "autostart": s.autostart,
                "idle_seconds": s.idle_seconds,
                "allow_sampling": s.allow_sampling,
                "sampling_alias": s.sampling_alias,
                // Set for an agent's own registration (container-runtime
                // §3.3), so the MCP page can say who owns a row it must not
                // offer to edit rather than leaving it looking hand-made.
                "agent_id": s.agent_id,
                "status": live.map(|v| v.status).unwrap_or("stopped"),
                "tool_count": live.map(|v| v.tool_count).unwrap_or(0),
                // An agent's row whose app container is not running is
                // **sleeping**, not failed (container-runtime §3.3): the
                // aggregate deliberately does not connect it, because
                // connecting means `podman run`. Said here rather than left to
                // read as a server that could not be reached.
                "status_detail": match s.agent_id.as_deref() {
                    Some(agent) if dev.contains_key(agent) => Some(format!(
                        "served from a dev server at {} — this agent's app is overridden by a \
                         dev_url, so lmgw starts nothing for it and its tools are listed \
                         whenever that server answers.",
                        dev[agent]
                    )),
                    Some(agent) if state.agent_services.get(agent).is_none() => Some(
                        "sleeping — the app container is not running. Listing its tools would \
                         start one, which an aggregate tools/list never does; a tools/call on \
                         one of them, a chat thread attaching this agent's label, or the App \
                         tab starts it."
                            .to_string(),
                    ),
                    _ => live.and_then(|v| v.detail.clone()),
                },
            })
        })
        .collect();
    Ok(json!({ "mcp_servers": out }))
}

/// One server's tools, in the **exposed** spelling a run resolves against.
///
/// The aggregate is the single source of truth for what a server offers right
/// now — hidden and renamed tools are already applied — so the thread picker
/// offers exactly the names `mcp::exec::resolve` will match, instead of a list
/// assembled from a second, staler place. Listing connects the server lazily,
/// like any `tools/list` does; a server that cannot connect returns no tools
/// and the caller shows its status.
pub async fn mcp_server_tools(state: &SharedState, id: i64) -> Result<Value, String> {
    let snap = state.snapshot();
    let agg = state.mcp.list_tools(&snap).await;
    let out: Vec<Value> = agg
        .tools
        .iter()
        // A tool the owner switched off is not offered to anyone, so the picker
        // must not show a tick for it either.
        .filter(|t| !snap.tool_disabled(t.name.as_ref()))
        .filter_map(|t| {
            let (server_id, upstream) = agg.reverse.get(t.name.as_ref())?;
            (*server_id == id).then(|| {
                json!({
                    "name": t.name,
                    "upstream_name": upstream,
                    "description": t.description,
                })
            })
        })
        .collect();
    Ok(json!({ "tools": out }))
}

/// Everything this gateway can serve northbound, with its source and state.
///
/// The MCP page lists *servers*; this is the tool-level view under it, and the
/// only place the built-in `lmgw__*` / `docs__*` toolsets are visible at all.
pub async fn tools(state: &SharedState) -> Result<Value, String> {
    let inv = crate::mcp::inventory::list(state).await;
    serde_json::to_value(inv).map_err(|e| e.to_string())
}

/// Flip the owner's per-tool switch.
///
/// Disabling requires the tool to be offered right now: a typo would otherwise
/// persist a switch for a name that does not exist, which is exactly the
/// invisible state the inventory's stale rows exist to prevent. Enabling never
/// checks — that is how a stale row is cleared.
pub async fn tool_set(state: &SharedState, name: &str, enabled: bool) -> Result<Value, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("pass the fully-qualified tool name".into());
    }
    if enabled {
        let cleared = store::enable_tool(&state.db, name)
            .await
            .map_err(|e| e.to_string())?;
        state.reload_snapshot().await.map_err(|e| e.to_string())?;
        state.mcp.notify_tools_changed_now();
        return Ok(json!({
            "ok": true,
            "message": if cleared > 0 {
                format!("'{name}' is offered again")
            } else {
                format!("'{name}' was not disabled")
            },
        }));
    }
    let inv = crate::mcp::inventory::list(state).await;
    let source = inv
        .tools
        .iter()
        .find(|t| t.name == name && !t.stale)
        .map(|t| t.source_label.clone())
        .ok_or_else(|| format!("no tool named '{name}' is offered by this gateway right now"))?;
    store::disable_tool(&state.db, name, &source)
        .await
        .map_err(|e| e.to_string())?;
    state.reload_snapshot().await.map_err(|e| e.to_string())?;
    // Northbound clients hold their own copy of `tools/list`; nudge them to
    // re-list rather than letting them keep offering a tool that now refuses.
    state.mcp.notify_tools_changed_now();
    Ok(json!({
        "ok": true,
        "message": format!("'{name}' is disabled — it is no longer listed or callable"),
    }))
}

/// Tail the unified request log. `limit` is the caller's own bound — passed to
/// SQL verbatim, with no second ceiling silently clamping it.
pub async fn logs(
    state: &SharedState,
    limit: Option<i64>,
    errors_only: Option<bool>,
    alias: Option<&str>,
    before_id: Option<i64>,
) -> Result<Value, String> {
    let filter = LogFilter {
        alias: alias.map(str::to_string),
        upstream_name: None,
        errors_only: errors_only.unwrap_or(false),
        limit: limit.unwrap_or(50),
        before_id,
        ..Default::default()
    };
    let rows = store::query_logs(&state.db, &filter)
        .await
        .map_err(|e| e.to_string())?;
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "ts": r.ts,
                "ingress": r.ingress_proto,
                "alias": r.requested_alias,
                "upstream": r.upstream_name,
                "upstream_model": r.upstream_model,
                "mcp_tool": r.mcp_tool,
                "status": r.status,
                "ttfb_ms": r.ttfb_ms,
                "total_ms": r.total_ms,
                "prompt_tokens": r.prompt_tokens,
                "completion_tokens": r.completion_tokens,
                "streamed": r.streamed,
                "error_kind": r.error_kind,
                "error_msg": r.error_msg,
                "fallback_reason": r.fallback_reason,
                "rung": r.rung,
            })
        })
        .collect();
    Ok(json!({ "logs": out, "count": out.len() }))
}

/// Current settings, with every secret redacted.
pub async fn settings(state: &SharedState) -> Result<Value, String> {
    let s = &state.snapshot().settings;
    // One shape per class (§6): the image every model of the class inherits,
    // where its GGUFs live, the extra `podman run` flags, and the prefix its
    // models are exposed under. Container names are derived (§3.3) and ports
    // are dynamic (§3.5), so neither is a setting any more.
    let class = |r: &crate::config::RouterSettings| {
        json!({
            "image": r.image,
            "models_dir": r.models_dir,
            "extra_run_args": r.extra_run_args.join("\n"),
            "public_prefix": r.public_prefix,
        })
    };
    let secret = |v: &str| if v.is_empty() { None } else { Some(REDACTED) };
    let mut out = json!({
        "bind_addr": s.bind_addr,
        "auth_enabled": s.auth_enabled,
        "retention_days": s.retention_days,
        "retention_max_rows": s.retention_max_rows,
        "jobs_retention_days": s.jobs_retention_days,
        "jobs_max_rows": s.jobs_max_rows,
        "max_body_mb": s.max_body_mb,
        "chat_archive_days": s.chat_archive_days,
        "chat_purge_days": s.chat_purge_days,
        "docs_ingest_reply_tokens": s.docs_ingest_reply_tokens,
        "docs_embed_batch": s.docs_embed_batch,
        "docs_fetch_delay_ms": s.docs_fetch_delay_ms,
        "docs_search": s.docs_search,
        "docs_rerank_model": s.docs_rerank_model,
        "sampling_alias": s.sampling_alias,
        "self_admin": s.self_admin.as_str(),
        "update_check_enabled": s.update_check_enabled,
        "hf_token": secret(&s.hf_token),
        "update_token": secret(&s.update_token),
        "container_prefix": s.container_prefix,
        "agent_origin_suffix": s.agent_origin_suffix,
        "agent_script_image": s.agent_script_image,
        // `active` is reported but never patchable here — see
        // `SettingsPatch::hold_fallback_alias` and `ops::hold_set` (gpu-hold
        // design §3.1): engaging the hold runs the sweep, a side effect a
        // generic settings save must not grow.
        "hold": { "active": s.hold.active, "fallback_alias": s.hold.fallback_alias },
        // Next to the hold: both decide when a fallback answers instead of
        // the local model. The rest of `VramSettings` is not exposed over
        // MCP yet — only the field a fallback decision reads (unified-KV
        // spec §12).
        "vram": { "fallback_on_external": s.vram.fallback_on_external },
        "router": class(&s.router),
        "aux_router": class(&s.aux_router),
        "audio": {
            "image": s.audio.image,
            "models_dir": s.audio.models_dir,
            "backend": s.audio.backend,
            "device": s.audio.device,
            "threads": s.audio.threads,
            "lazy_load": s.audio.lazy_load,
            // The four bounds and the voice library go into every model's
            // `server.json`; 0 is "no bound" on each, and a reader that cannot
            // see them cannot explain a 503 server_busy or a reloaded model.
            "busy_timeout_ms": s.audio.busy_timeout_ms,
            "idle_unload_ms": s.audio.idle_unload_ms,
            "min_free_memory_mb": s.audio.min_free_memory_mb,
            "max_request_body_mb": s.audio.max_request_body_mb,
            "voice_dir": s.audio.voice_dir,
            "extra_run_args": s.audio.extra_run_args.join("\n"),
            "public_prefix": s.audio.public_prefix,
        },
        // The image class has no engine fields at all (image-generation design
        // §4): sd-server has no config file, and everything per-process is a
        // flag in some row's `args`.
        "image": {
            "image": s.image.image,
            "models_dir": s.image.models_dir,
            "extra_run_args": s.image.extra_run_args.join("\n"),
            "public_prefix": s.image.public_prefix,
        },
    });
    // Container builds (container-builds §5, §7, §8) — assigned after the
    // macro, which is at rustc's recursion limit (as in
    // `web::api_settings::settings_full`). The configured dir and the one
    // builds actually use are both shown, since "unset" means a default that
    // differs between prod and a dev instance.
    // The default Chat prompt, after the macro for the same reason: the one
    // in force, and whether it is the built-in one (which then shows it).
    out["chat_system_prompt"] = json!(s.default_chat_prompt());
    out["chat_system_prompt_is_builtin"] = json!(s.chat_system_prompt.is_none());
    out["chat_pdf_mode"] = json!(s.chat_pdf_mode);
    out["chat_stt_alias"] = json!(s.chat_stt_alias);
    out["chat_kb_budget_tokens"] = json!(s.chat_kb_budget_tokens);
    let builds_dir = state.builds_dir();
    out["builds_dir"] = json!(s.builds_dir);
    out["builds_dir_effective"] = json!(builds_dir.display().to_string());
    out["builds_dir_warning"] = json!(crate::backends::paths::tmpfs_refusal(&builds_dir));
    out["forge_tokens"] = redact_map(&s.forge_tokens);
    out["build_update_check_hours"] = json!(s.build_update_check_hours);
    Ok(out)
}
