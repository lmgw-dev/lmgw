//! The `chat` run kind: turning an agent into a Chat thread (§2.5).
//!
//! "Open in Chat" creates a `chat_threads` row seeded from the manifest — the
//! model alias rendered against the stored config, the system prompt rendered
//! the same way, the `tools[]` labels as the thread's `mcp_tools`, `kind =
//! "chat"` — and stamps `agent_id` on it so the thread lists under the agent.
//! From there it is an ordinary tool-enabled thread (`web/agentchat.rs`): same
//! sidebar, same persistence, same Logs rows.
//!
//! **Why the seeding is server-side.** `POST /chat/api/threads` takes only
//! `{model_alias, kind}` and everything else is patched afterwards, so a client
//! doing this would need two calls with a live thread in between — a thread
//! that exists with no prompt and no tools, and that stays that way if the
//! second call fails. One op, one row, one state.
//!
//! **What refuses and what only warns.** A wrong run kind, a disabled agent and
//! a config that does not satisfy its own schema all refuse: none of them can
//! produce a usable thread. A model alias the gateway cannot resolve *right
//! now*, and manifest sampling knobs a thread has no column for, are
//! **warnings** — the same rule §5 applies to a missing MCP server, for the
//! same reason. The thread opens, the Chat page's own model picker is right
//! there, and the caller is told.

use serde_json::{json, Value};

use crate::state::SharedState;
use crate::store::{self, ThreadMcp};

use super::manifest::{self, RunSpec};
use super::{template, Agent, ToolSurface};

/// The sampling fields a manifest can carry that a `chat_threads` row cannot.
///
/// `temperature` is the one the table has a column for; the rest belong to a
/// request, not to a conversation. Naming them is the point: a manifest that
/// sets `seed` and gets a thread that ignores it should say so once, here,
/// rather than have the author wonder why runs differ.
fn dropped_params(m: &manifest::Manifest) -> Vec<&'static str> {
    let mut out = Vec::new();
    if m.model.top_p.is_some() {
        out.push("top_p");
    }
    if m.model.top_k.is_some() {
        out.push("top_k");
    }
    if m.model.seed.is_some() {
        out.push("seed");
    }
    if m.model.reasoning.is_some() {
        out.push("reasoning");
    }
    out
}

/// `agent_open_chat` — materialize a `chat` agent into a new thread and return
/// its id, plus the `/chat?t=<id>` location the dashboard navigates to.
///
/// `values` is the Run tab's form as it stands at the click — a sparse,
/// **unsaved** patch over the stored config that this thread is seeded from.
/// Picking a model and opening a thread with it is one gesture, and it does
/// not change the agent's defaults; Save config is what does that.
pub async fn open(
    state: &SharedState,
    id: &str,
    values: &serde_json::Map<String, Value>,
) -> Result<Value, String> {
    let row = store::get_agent(&state.db, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no agent with id '{id}'"))?;
    let mut agent = Agent::from_row(row)?;
    agent.override_config(values)?;
    let m = &agent.manifest;

    let RunSpec::Chat { system } = &m.run else {
        return Err(format!(
            "'{id}' is a {} agent, not a chat one; agent_open_chat only opens the chat kind",
            m.kind()
        ));
    };
    if !agent.row.enabled {
        return Err(format!(
            "agent '{id}' is disabled; enable it on the catalog before opening a thread"
        ));
    }

    // The config has to be satisfied before a thread is worth opening: the
    // model alias is usually `{{config.model}}`, so an unset required field
    // would produce a thread pointed at nothing. `override_config` has already
    // said so for a caller that sent values; this is the same check for the one
    // that sent none and is relying on what is stored.
    let fields = m.fields().map_err(|e| e.join("; "))?;
    manifest::validate_values(&fields, &agent.config_values()).map_err(|e| {
        format!("agent '{id}' is not configured yet: {e}. Fill it in on the Run tab.")
    })?;

    let ctx = template::Ctx {
        config: agent.effective_config(),
        ..Default::default()
    }
    .with_identity(&m.id, &m.name, None);
    let alias = template::render_text(&m.model.alias, &ctx)
        .trim()
        .to_string();
    if alias.is_empty() {
        return Err(format!(
            "agent '{id}' has no model: '{}' rendered empty against the stored config. Pick one \
             on the Run tab.",
            m.model.alias
        ));
    }

    let mut warnings: Vec<String> = Vec::new();
    // A candidate alias serves through the gate, which picks its model per
    // request — it has no one route for `resolve` to find (candidate-aliases
    // §4.1).
    let unserved = {
        let snap = state.snapshot();
        snap.resolve(&alias).is_err() && snap.candidate_alias(&alias).is_none()
    };
    if unserved {
        warnings.push(format!(
            "this gateway does not currently serve the model '{alias}'; the thread opened anyway \
             — pick a model in the thread header, or fix the agent's config."
        ));
    }
    // The same gap the card and the import report show (§5), said again here
    // because this is the moment it starts to matter: the thread is opening
    // with `mcp_tools` naming a server that will not resolve at send time. A
    // warning, not a refusal — the server is usually registered minutes later,
    // and the thread is still a usable conversation without its tools.
    let surface = ToolSurface::load(state).await;
    for r in surface.check(&m.tools) {
        if !r.registered {
            warnings.push(format!(
                "this thread attaches '{}', which is not a registered MCP server here (available: \
                 {}); its tools will not resolve until it is.",
                r.label,
                surface.labels().join(", ")
            ));
        } else if !r.missing_tools.is_empty() {
            warnings.push(format!(
                "the '{}' toolset does not currently list: {}; the thread attaches what it has.",
                r.label,
                r.missing_tools.join(", ")
            ));
        }
    }
    let dropped = dropped_params(m);
    if !dropped.is_empty() {
        warnings.push(format!(
            "a chat thread carries the model alias, the system prompt, the tools and the \
             temperature; the manifest's {} {} not applied.",
            dropped.join(", "),
            if dropped.len() == 1 { "is" } else { "are" }
        ));
    }

    let prompt = system
        .as_deref()
        .map(|s| template::render_text(s, &ctx).trim().to_string())
        .unwrap_or_default();
    // The label goes across as written, `allowed` and all: `mcp::exec::resolve`
    // is what turns either into tools, for a thread and for a run alike, so
    // narrowing here would be a second rule to keep in step.
    let mcp_tools: Vec<ThreadMcp> = m
        .tools
        .iter()
        .map(|t| ThreadMcp {
            server_label: t.label.clone(),
            allowed_tools: t.allowed.clone(),
        })
        .collect();

    // The title is left at the table's default so the Chat page still names the
    // thread from its first message. Five threads all called "Docs librarian"
    // would be a worse list than five named after what was asked.
    let thread_id = store::create_agent_chat_thread(
        &state.db,
        &m.id,
        &alias,
        &prompt,
        m.model.temperature,
        &mcp_tools,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(json!({
        "ok": true,
        "id": m.id,
        "thread_id": thread_id,
        "model": alias,
        "tools": m.labels(),
        "url": format!("/chat?t={thread_id}"),
        "warnings": warnings,
        "message": format!("opened a thread for '{}'", m.name),
    }))
}
