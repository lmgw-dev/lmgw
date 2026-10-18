//! The agent ops' and tools' answers as the `lmgw-api-types` types, built field
//! by field: what the `/api/op/agent_*` ops and the `lmgw__agent*` tools of
//! `api_agents.rs` send. A source struct is destructured without `..`, so a field
//! added on either side fails the build instead of going missing from the wire.

use lmgw_api_types as dto;
use lmgw_api_types::self_admin::{AgentList, AgentListRow};
use serde::Serialize;
use serde_json::Value;

use crate::agents::package;
use crate::agents::service::Stopped;
use crate::agents::token;
use crate::config::ScopeMode;
use crate::principal::Refusal as ApiRefusal;

/// `lmgw__agents`.
pub(super) fn agent_list(cards: Vec<dto::AgentCard>) -> AgentList {
    let agents: Vec<AgentListRow> = cards.into_iter().map(list_row).collect();
    let next_step = match agents.first() {
        Some(a) => format!(
            "lmgw__agent_get id={} for the manifest and the config form",
            a.id
        ),
        None => "no agents yet: lmgw__agent_install or lmgw__agent_set adds one".to_string(),
    };
    AgentList {
        count: agents.len(),
        agents,
        next_step,
    }
}

fn list_row(card: dto::AgentCard) -> AgentListRow {
    let dto::AgentCard {
        id,
        name,
        description,
        version: _,
        kind,
        model_alias: _,
        effective_model,
        labels,
        enabled,
        source,
        requires_ok,
        requires: _,
        warnings: _,
        last_run: _,
        threads: _,
        app: _,
        error,
    } = card;
    AgentListRow {
        id,
        name,
        description,
        kind,
        model: (!effective_model.is_empty()).then_some(effective_model),
        enabled,
        source,
        tools: labels,
        requires_ok,
        error,
    }
}

/// `agent_install`: the report, and where the document came from. `digest` is
/// `None` when podman could not report one.
pub(super) fn installed(
    report: dto::AgentImportReport,
    image: &str,
    digest: Option<String>,
    pulled: bool,
) -> dto::AgentInstalled {
    let id = &report.id;
    let message = if report.validate_only {
        format!("'{id}' is what '{image}' would install — nothing written")
    } else if report.replaced {
        format!("installed '{id}' from '{image}', keeping its config")
    } else {
        format!("installed '{id}' from '{image}'")
    };
    dto::AgentInstalled {
        report,
        image: image.to_string(),
        digest,
        pulled,
        manifest_path: package::MANIFEST_INSIDE.to_string(),
        message,
    }
}

/// `agent_delete`: `threads_kept` chat threads lost their agent and stayed.
/// `shipped` is whether the agent is one lmgw ships.
pub(super) fn deleted(id: &str, threads_kept: u64, shipped: bool) -> dto::AgentDeleted {
    dto::AgentDeleted {
        ok: true,
        id: id.to_string(),
        threads_kept,
        message: match threads_kept {
            0 => format!("deleted agent '{id}'"),
            1 => format!("deleted agent '{id}'; its 1 chat thread was kept, on the Chat page"),
            n => format!("deleted agent '{id}'; its {n} chat threads were kept, on the Chat page"),
        },
        hint: if shipped {
            "this was a shipped agent; 'Restore shipped agents' (agents_restore) brings it back"
        } else {
            "gone; import the exported file to bring it back"
        }
        .to_string(),
    }
}

/// `agent_run`: `job_id` is the run that was started, or the one in flight;
/// `phase` is that run's phase, which is not the one asked for when a run of
/// another phase was already in flight.
pub(super) fn run_started(
    id: &str,
    job_id: i64,
    phase: &str,
    already_running: bool,
) -> dto::AgentRunStarted {
    dto::AgentRunStarted {
        ok: true,
        id: id.to_string(),
        job_id,
        phase: phase.to_string(),
        already_running,
        message: if already_running {
            format!("a run of '{id}' is already in flight; showing that one")
        } else {
            format!("started the {phase} run of '{id}'")
        },
    }
}

/// `POST /api/agents/{id}/runs` while a run of the agent is in flight.
pub(super) fn run_conflict(id: &str, job_id: i64) -> dto::AgentRunConflict {
    dto::AgentRunConflict {
        code: "already_running".to_string(),
        message: format!("'{id}' already has a run in flight (#{job_id}); one live run per agent"),
        run: job_id,
    }
}

/// A typed answer as the `Value` the dispatcher hands back — `to_json` for the
/// ops that can refuse with a code of their own.
pub(super) fn coded<T: Serialize>(r: Result<T, ApiRefusal>) -> Result<Value, ApiRefusal> {
    serde_json::to_value(r?).map_err(|e| format!("serializing the answer: {e}").into())
}

/// `agent_pull`: did the image change? Both digests known: they differ. None
/// before, one now: it arrived. None after: unknown.
pub(super) fn moved(before: &Option<String>, after: &Option<String>) -> Option<bool> {
    match (before, after) {
        (Some(b), Some(a)) => Some(b != a),
        (None, Some(_)) => Some(true),
        (_, None) => None,
    }
}

/// `agent_pull`: the digest before and after the pull as podman reported them
/// (`None` when it could not), what the row had recorded, and how the new
/// image's manifest compared with the stored one (`manifest_differs`, `note`;
/// `None` and empty when nothing was compared).
pub(super) fn pulled(
    id: &str,
    image: &str,
    (before, after): (Option<String>, Option<String>),
    recorded: String,
    manifest_differs: Option<bool>,
    note: String,
) -> dto::AgentPulled {
    let changed = moved(&before, &after);
    let message = match (&before, &after) {
        (Some(b), Some(a)) if b == a => format!("'{image}' is unchanged ({a})"),
        (Some(b), Some(a)) => format!("'{image}' moved: {b} → {a}"),
        (None, Some(a)) => format!("pulled '{image}' ({a}); it was not on this box before"),
        (_, None) => format!("pulled '{image}'; podman could not report a digest for it"),
    };
    dto::AgentPulled {
        ok: true,
        id: id.to_string(),
        image: image.to_string(),
        old_digest: before,
        new_digest: after,
        recorded_digest: (!recorded.is_empty()).then_some(recorded),
        changed,
        manifest_differs,
        note,
        message,
    }
}

/// `agent_reimport`: the report, the image it was read from and whether the
/// manifest differed from the stored one.
pub(super) fn reimported(
    report: dto::AgentImportReport,
    image: &str,
    changed: bool,
) -> dto::AgentReimported {
    let id = &report.id;
    let message = if changed {
        format!("re-imported '{id}' from '{image}', keeping its config")
    } else {
        format!("'{id}' already matches '{image}'; nothing changed")
    };
    dto::AgentReimported {
        report,
        image: image.to_string(),
        changed,
        message,
    }
}

/// `agent_dev_url_set` with no URL: the app is served from its image again.
/// `origin` is where the next request to it lands.
pub(super) fn dev_url_cleared(id: &str, origin: &str) -> dto::AgentDevUrlSet {
    dto::AgentDevUrlSet {
        ok: true,
        id: id.to_string(),
        dev_url: String::new(),
        stopped: None,
        message: format!(
            "'{id}' is served from its image again; the next request to {origin} starts the \
             container"
        ),
    }
}

/// `agent_dev_url_set` with a URL; `stopped` is what stopping the app container
/// that was started from the image did, if one was running.
pub(super) fn dev_url_set(id: &str, url: &str, stopped: Option<Stopped>) -> dto::AgentDevUrlSet {
    let message = match &stopped {
        Some(s) => format!(
            "'{id}' is served from {url}; its app container was started from the image, so it \
             was stopped ({})",
            s.describe()
        ),
        None => format!("'{id}' is served from {url}; no container is started for its app"),
    };
    dto::AgentDevUrlSet {
        ok: true,
        id: id.to_string(),
        dev_url: url.to_string(),
        stopped: stopped.and_then(|s| s.container),
        message,
    }
}

/// `agent_config_set`: what is stored now (`config`, masked), the scope the
/// token carries, the slots that moved (`repointed`) and the app container that
/// was holding the old folder.
pub(super) fn config_saved(
    id: &str,
    token_scope: String,
    repointed: Vec<String>,
    stopped: Option<Stopped>,
    config: Value,
    undeclared: Vec<String>,
) -> dto::AgentConfigSaved {
    let mut message = if undeclared.is_empty() {
        "config saved".to_string()
    } else {
        format!(
            "config saved; {} {} not declared by this manifest and {} removed from the stored \
             config",
            undeclared.join(", "),
            if undeclared.len() == 1 { "is" } else { "are" },
            if undeclared.len() == 1 { "was" } else { "were" },
        )
    };
    if let Some(s) = &stopped {
        message.push_str(&format!(
            "; the app container was holding the mount for {} and was stopped ({})",
            repointed.join(", "),
            s.describe()
        ));
    }
    dto::AgentConfigSaved {
        ok: true,
        id: id.to_string(),
        token_scope,
        mounts_repointed: repointed,
        service_stopped: stopped.and_then(|s| s.container),
        config,
        cleared_undeclared: undeclared,
        message,
    }
}

/// `agent_enable`; `stopped` is what disabling did to the app container.
pub(super) fn enabled(id: &str, enabled: bool, stopped: Option<Stopped>) -> dto::AgentEnabled {
    let mut message = if enabled {
        "agent enabled".to_string()
    } else {
        "agent disabled".to_string()
    };
    if let Some(s) = &stopped {
        message.push_str(&format!("; {}", s.describe()));
    }
    dto::AgentEnabled {
        ok: true,
        id: id.to_string(),
        enabled,
        service_stopped: stopped.and_then(|s| s.container),
        message,
    }
}

/// `agent_duplicate`: `new_id` is the copy, made from `id`; `omitted` are the
/// secret fields it does not carry.
pub(super) fn duplicated(
    id: &str,
    new_id: String,
    name: String,
    omitted: Vec<String>,
) -> dto::AgentDuplicated {
    dto::AgentDuplicated {
        ok: true,
        message: format!("duplicated '{id}' as '{new_id}'"),
        id: new_id,
        name,
        config_omitted: omitted,
    }
}

/// `agent_reset`: `dropped` are the stored config keys the shipped schema no
/// longer declares.
pub(super) fn reset(id: &str, dropped: Vec<String>) -> dto::AgentReset {
    let message = format!(
        "'{id}' restored to the shipped manifest; its config was kept{}",
        if dropped.is_empty() {
            String::new()
        } else {
            format!(
                " except {}, which the shipped schema no longer declares",
                dropped.join(", ")
            )
        }
    );
    dto::AgentReset {
        ok: true,
        id: id.to_string(),
        dropped_config: dropped,
        message,
    }
}

/// `agent_run_cancel`: `message` is what the job registry said.
pub(super) fn run_cancelled(id: &str, message: String) -> dto::AgentRunCancelled {
    dto::AgentRunCancelled {
        ok: true,
        id: id.to_string(),
        message,
    }
}

/// `agent_token_get`: the token in the clear and the scope it was derived to
/// (`mode`, the aliases it is fenced to one per line).
pub(super) fn token_got(
    id: &str,
    token: String,
    (mode, patterns): (ScopeMode, String),
) -> dto::AgentTokenRead {
    dto::AgentTokenRead {
        ok: true,
        id: id.to_string(),
        name: token::key_name(id),
        token,
        scope_mode: mode.as_str().to_string(),
        scope_note: token::scope_note(mode, &patterns),
        scope_patterns: patterns,
        message: format!("the token for '{id}'"),
    }
}

/// `agent_token_rotate`: the new token in the clear, the scope it was derived
/// to, the container the rotation stopped and the sentence when its scope could
/// not be re-derived.
pub(super) fn token_issued(
    id: &str,
    token: String,
    (mode, patterns): (ScopeMode, String),
    stopped: Option<Stopped>,
    warning: Option<String>,
) -> dto::AgentTokenIssued {
    let message = match stopped.as_ref().map(Stopped::describe) {
        Some(note) => format!(
            "rotated the token for '{id}' — the previous one stopped working now, and its app \
             was holding it: {note}"
        ),
        None => format!("rotated the token for '{id}' — the previous one stopped working now"),
    };
    dto::AgentTokenIssued {
        ok: true,
        id: id.to_string(),
        name: token::key_name(id),
        token,
        scope_mode: mode.as_str().to_string(),
        scope_note: token::scope_note(mode, &patterns),
        scope_patterns: patterns,
        service_stopped: stopped.and_then(|s| s.container),
        warning,
        message,
    }
}

/// `agent_service_start`: the container is up, on `host_port`, and answers at
/// `origin`.
pub(super) fn service_started(
    id: &str,
    container: String,
    host_port: u16,
    origin: String,
) -> dto::AgentServiceStarted {
    dto::AgentServiceStarted {
        ok: true,
        id: id.to_string(),
        running: true,
        message: format!("'{id}' is serving on 127.0.0.1:{host_port} as {container}"),
        container,
        host_port,
        origin,
    }
}

/// `agent_service_stop`: what the stop did, if there was anything to stop.
pub(super) fn service_stopped(id: &str, stopped: Option<Stopped>) -> dto::AgentServiceStopped {
    dto::AgentServiceStopped {
        ok: true,
        id: id.to_string(),
        running: false,
        cancelled_start: stopped.as_ref().is_some_and(|s| s.cancelled_start),
        message: match &stopped {
            Some(s) => s.describe(),
            None => format!("'{id}' had no app container running"),
        },
        stopped: stopped.and_then(|s| s.container),
    }
}

/// `agent_service_log`: `asked` lines were asked for (`0` is the whole log);
/// the answer's `lines` is how many came back. With no app container running
/// there is no log to read.
pub(super) fn service_log(
    id: &str,
    asked: usize,
    running: Option<(String, String)>,
) -> dto::AgentServiceLog {
    match running {
        None => dto::AgentServiceLog {
            ok: true,
            id: id.to_string(),
            lines: 0,
            running: false,
            container: None,
            log: String::new(),
            message: format!("'{id}' has no app container running, so there is no log to read"),
        },
        Some((container, log)) => {
            let lines = log.lines().count();
            let plural = if lines == 1 { "line" } else { "lines" };
            dto::AgentServiceLog {
                ok: true,
                id: id.to_string(),
                lines: lines as u64,
                running: true,
                message: match (asked, lines) {
                    (_, 0) => format!("'{container}' has logged nothing"),
                    (0, n) => format!("the whole log of '{container}' ({n} {plural})"),
                    (_, 1) => format!("the last line of '{container}'"),
                    (_, n) => format!("the last {n} lines of '{container}'"),
                },
                container: Some(container),
                log,
            }
        }
    }
}
