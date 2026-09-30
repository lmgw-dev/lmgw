//! Agents — the catalog (agent-catalog design §6.1).
//!
//! One card per installed agent: what it is, what model it will ask for, which
//! tool labels it attaches, whether those labels resolve on this gateway right
//! now, and how much it has been used. An agent is a manifest, so the two ways
//! to get one are Import (a file or a paste, both the same endpoint) and New (a
//! starter manifest you then edit on the Definition tab).
//!
//! Dense, like every tool surface (UX plan §4): the cards are read all day,
//! and a description folds to three lines with a "more" beside it rather than
//! stretching its card.
//!
//! The rule the card exists to show: **a missing MCP server is a warning, not a
//! refusal** (§5). An agent imports before its server is wired, the way a
//! server is registered before its image is pulled; the card says what is
//! missing and links the MCP page instead of hiding the agent.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;
use lmgw_api_types::{AgentCard, AgentImportReport};
use serde_json::{json, Value};
use wasm_bindgen::JsValue;

use super::agent_detail::unbound_line;
use crate::scope::Scope;
use crate::widgets::{
    use_slash_focus, use_toasts, ClampText, Density, Modal, ModalFooter, PageFrame, Select,
};

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested; there is no DOM harness in this crate)
// ---------------------------------------------------------------------------

/// Does this card match what was typed in the search box?
///
/// Everything the card shows is searchable — id, name, description, model and
/// tool labels — because "the one that uses gws" is as likely a way to look for
/// an agent as its name.
pub fn matches_query(c: &AgentCard, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let hay = [
        c.id.as_str(),
        c.name.as_str(),
        c.description.as_str(),
        c.kind.as_str(),
        c.effective_model.as_str(),
    ];
    hay.iter().any(|h| h.to_lowercase().contains(&q))
        || c.labels.iter().any(|l| l.to_lowercase().contains(&q))
}

/// The card's activity line: what this agent has actually done.
///
/// A `chat` agent counts threads, a `batch` agent names its last run. Neither
/// is invented: an agent that has done nothing says so, rather than showing a
/// zero that reads like a failure.
pub fn activity_line(c: &AgentCard) -> String {
    if c.kind == "chat" {
        return match c.threads {
            0 => "no threads yet".to_string(),
            1 => "1 thread".to_string(),
            n => format!("{n} threads"),
        };
    }
    match &c.last_run {
        Some(r) => format!(
            "last run: {} {}",
            r.status,
            short_ts(r.finished_at.as_deref().unwrap_or(&r.created_at))
        ),
        None => "never run".to_string(),
    }
}

/// `2026-09-18 09:12:07` → `2026-09-18 09:12`. SQLite writes seconds nobody
/// reads; where they might matter the caller keeps the stored value in a
/// `title` attribute (the Runs and Threads tables do).
pub fn short_ts(ts: &str) -> String {
    ts.get(0..16).unwrap_or(ts).to_string()
}

/// The one-line warning under a card whose tools do not resolve here.
pub fn requires_warning(c: &AgentCard) -> Option<String> {
    if c.requires_ok {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for r in &c.requires {
        if !r.registered {
            parts.push(format!("needs MCP server '{}'", r.label));
        } else if !r.missing_tools.is_empty() {
            parts.push(format!(
                "'{}' is missing {}",
                r.label,
                r.missing_tools.join(", ")
            ));
        }
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// Every warning line under one card: the tool gaps first (their own sentence,
/// with the MCP link), then §4.3's — podman missing, an image that is not on
/// the box, a `localhost/` image that arrived in an import.
///
/// One list rather than two renders, because to the owner they are the same
/// question: is there anything stopping this agent from running here.
pub fn card_warnings(c: &AgentCard) -> Vec<String> {
    c.warnings.iter().map(|w| w.message.clone()).collect()
}

/// The starter manifest New writes. A `chat` agent with a model picker and an
/// empty system prompt: the smallest thing that is valid, runnable and obvious
/// to edit. A batch agent starts from this one too — the Definition tab is
/// where `run.kind` changes, and `agent_set` validates it there.
pub fn starter_manifest(id: &str, name: &str) -> String {
    let id = Value::String(id.to_string());
    let name = Value::String(name.to_string());
    format!(
        r#"{{
  "schema_version": 1,
  "id": {id},
  "name": {name},
  "description": "",
  "model": {{ "alias": "{{{{config.model}}}}" }},
  "config": {{
    "schema": {{
      "type": "object",
      "properties": {{
        "model": {{ "type": "string", "format": "model_alias", "title": "Model" }}
      }},
      "required": ["model"]
    }}
  }},
  "tools": [],
  "run": {{
    "kind": "chat",
    "system": "You are a helpful assistant."
  }}
}}"#
    )
}

// ---------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------

#[component]
pub fn Agents() -> impl IntoView {
    let toasts = use_toasts();
    let cards = RwSignal::new(None::<Vec<AgentCard>>);
    let error = RwSignal::new(None::<String>);
    let query = RwSignal::new(String::new());
    let import_open = RwSignal::new(false);
    let install_open = RwSignal::new(false);
    let new_open = RwSignal::new(false);
    let busy = RwSignal::new(false);

    let scope = Scope::new();
    let reload = move || {
        scope.spawn(async move {
            match crate::api::get::<Vec<AgentCard>>("/api/agents").await {
                Ok(list) => {
                    error.set(None);
                    cards.set(Some(list));
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
    reload();

    let restore = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/op/agents_restore", &json!({})).await;
            busy.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("restored")
                        .to_string());
                    reload();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let shown = move || {
        let q = query.get();
        cards
            .get()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| matches_query(c, &q))
            .collect::<Vec<_>>()
    };
    let loaded_empty = move || cards.get().is_some_and(|c| c.is_empty());
    let searching = move || !query.with(|q| q.trim().is_empty());
    let shown_count = move || {
        let total = cards.with(|c| c.as_ref().map_or(0, Vec::len));
        format!(
            "{} of {}",
            shown().len(),
            crate::fmt::count_of(total, "agents")
        )
    };
    let q_box: NodeRef<leptos::html::Input> = NodeRef::new();
    use_slash_focus(q_box);

    view! {
        <PageFrame
            title="Agents"
            sub="A manifest each — model, prompts, tools, a config form"
            density=Density::Dense
            actions=move || {
                let restore = restore.clone();
                view! {
                    <input
                        class="input w-sm"
                        type="search"
                        node_ref=q_box
                        data-slash
                        placeholder="Search…"
                        title="Filter · / focuses, Esc clears"
                        prop:value=move || query.get()
                        on:input=move |ev| query.set(event_target_value(&ev))
                        on:keydown=move |ev| {
                            if ev.key() == "Escape" {
                                query.set(String::new());
                            }
                        }
                    />
                    // What the search holds back is said (the hard rule):
                    // "1 of 3 agents".
                    <Show when=searching>
                        <span class="filter-count">{shown_count}</span>
                    </Show>
                    <button class="btn ghost" disabled=move || busy.get() on:click=restore>
                        "Restore shipped"
                    </button>
                    <button class="btn ghost" on:click=move |_| install_open.set(true)>
                        "Install from image…"
                    </button>
                    <button class="btn ghost" on:click=move |_| import_open.set(true)>
                        "Import…"
                    </button>
                    <button class="btn primary" on:click=move |_| new_open.set(true)>
                        "New"
                    </button>
                }
            }
        >

            <Show when=move || error.get().is_some()>
                <div class="notice err" style="margin-bottom:14px">
                    {move || error.get().unwrap_or_default()}
                </div>
            </Show>

            <Show when=loaded_empty>
                <div class="card empty">
                    "No agents yet. " <b>"Install from image"</b>
                    " an agent package, " <b>"Import"</b>
                    " a manifest someone exported, press " <b>"New"</b>
                    " to write one, or " <b>"Restore shipped"</b>
                    " to put the ones lmgw ships with back. An agent reaches its tools through "
                    <a href="/mcp-servers">"MCP servers"</a> "."
                </div>
            </Show>
            <Show when=move || cards.get().is_none() && error.get().is_none()>
                <div class="card dim">"Loading…"</div>
            </Show>

            <div class="auto-grid agent-grid">
                <For each=shown key=|c| (c.id.clone(), c.updated_key()) let:c>
                    <Card c=c on_change=Callback::new(move |()| reload())/>
                </For>
            </div>
            <Show when=move || {
                !query.get().trim().is_empty() && shown().is_empty() && !loaded_empty()
            }>
                <div class="card empty">"Nothing matches that."</div>
            </Show>

            <ImportModal open=import_open on_change=Callback::new(move |()| reload())/>
            <InstallModal open=install_open on_change=Callback::new(move |()| reload())/>
            <NewAgentModal open=new_open/>
        </PageFrame>
    }
}

/// The key a card is re-rendered on: everything the card draws that an op can
/// change, so a toggle redraws its own row and nothing else.
trait CardKey {
    fn updated_key(&self) -> String;
}

impl CardKey for AgentCard {
    fn updated_key(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.enabled,
            self.requires_ok,
            self.threads,
            self.last_run.as_ref().map(|r| r.job_id).unwrap_or(0)
        )
    }
}

#[component]
fn Card(c: AgentCard, on_change: Callback<()>) -> impl IntoView {
    let toasts = use_toasts();
    let id = StoredValue::new(c.id.clone());
    let enabled = RwSignal::new(c.enabled);
    let busy = RwSignal::new(false);
    let warning = requires_warning(&c);
    let extra_warnings = card_warnings(&c);
    let labels = c.labels.join(" · ");
    let activity = activity_line(&c);
    let href = format!("/agents/{}", c.id);

    let toggle = move |ev| {
        let want = event_target_checked(&ev);
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        enabled.set(want);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                "/api/op/agent_enable",
                &json!({ "id": id.get_value(), "enabled": want }),
            )
            .await;
            busy.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("saved")
                        .to_string());
                    on_change.try_run(());
                }
                Err(e) => {
                    enabled.set(!want);
                    toasts.err(e.to_string());
                }
            }
        });
    };

    view! {
        <article class="card agent-card" class:off=move || !enabled.get()>
            <Show when={
                let err = c.error.clone();
                move || err.is_some()
            }>
                <div class="notice err">{c.error.clone().unwrap_or_default()}</div>
            </Show>
            <div class="row">
                <A href=href.clone() attr:class="agent-name">
                    {if c.name.is_empty() { c.id.clone() } else { c.name.clone() }}
                </A>
                <span class="spacer" style="flex:1"></span>
                // A service-mode agent serves a UI of its own
                // (container-runtime §3.3): the chip is the link to it.
                {c
                    .app
                    .then(|| {
                        view! {
                            <A
                                href=format!("{}/ui", href.clone())
                                attr:class="chip"
                                attr:title="this agent serves its own app, on demand"
                            >
                                "app"
                            </A>
                        }
                    })}
                <span class="type-badge">{c.kind.clone()}</span>
            </div>
            <ClampText text=c.description.clone() lines=3 class="agent-desc dim"/>
            <div class="row mono-sm dim">
                <span title="the alias this agent asks for, config already applied">
                    {if c.effective_model.is_empty() {
                        "no model".to_string()
                    } else {
                        c.effective_model.clone()
                    }}
                </span>
                {(!labels.is_empty()).then(|| view! { <span>"·"</span> <span>{labels}</span> })}
                {(!c.version.is_empty())
                    .then(|| {
                        view! { <span>"·"</span> <span>{format!("v{}", c.version)}</span> }
                    })}
            </div>
            {warning
                .map(|w| {
                    view! {
                        <div class="notice warn agent-warn">
                            <b>{w}</b>
                            <span class="detail">
                                "The agent is installed; it cannot run until the server is "
                                <a href="/mcp-servers">"registered"</a> "."
                            </span>
                        </div>
                    }
                })}
            <For each=move || extra_warnings.clone() key=|w| w.clone() let:w>
                <div class="notice warn agent-warn">
                    <b>{w}</b>
                </div>
            </For>
            <div class="row agent-foot">
                <span class="dim mono-sm">{activity}</span>
                <span class="spacer" style="flex:1"></span>
                {(c.source == "builtin").then(|| view! { <span class="type-badge">"shipped"</span> })}
                <label class="check" title="A disabled agent stays in the catalog and refuses to run.">
                    <input
                        type="checkbox"
                        prop:checked=move || enabled.get()
                        disabled=move || busy.get()
                        on:change=toggle
                    />
                    {move || if enabled.get() { "on" } else { "off" }}
                </label>
            </div>
        </article>
    }
}

// ---------------------------------------------------------------------------
// Import (§5) — file or paste, one endpoint
// ---------------------------------------------------------------------------

#[component]
fn ImportModal(open: RwSignal<bool>, on_change: Callback<()>) -> impl IntoView {
    let toasts = use_toasts();
    let file = RwSignal::new(None::<web_sys::File>);
    let pasted = RwSignal::new(String::new());
    let replace = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let report = RwSignal::new(None::<AgentImportReport>);
    let error = RwSignal::new(None::<String>);

    let send = move |validate_only: bool| {
        if busy.get_untracked() {
            return;
        }
        let picked = file.get_untracked();
        let text = pasted.get_untracked();
        if picked.is_none() && text.trim().is_empty() {
            toasts.err("pick a file or paste a manifest first");
            return;
        }
        busy.set(true);
        report.set(None);
        error.set(None);
        let url = format!(
            "/api/agents/import?replace={}&validate_only={validate_only}",
            replace.get_untracked()
        );
        spawn_local(async move {
            // A file and a paste are the same body; the endpoint has one path
            // for both, so there is nothing to keep in step here either.
            let res = match picked {
                Some(f) => crate::api::post_raw::<AgentImportReport>(url, f).await,
                None => {
                    crate::api::post_raw::<AgentImportReport>(url, JsValue::from_str(&text)).await
                }
            };
            busy.set(false);
            match res {
                Ok(r) => {
                    let committed = !r.validate_only;
                    let id = r.id.clone();
                    report.set(Some(r));
                    if committed {
                        toasts.ok(format!("imported '{id}'"));
                        on_change.try_run(());
                    }
                }
                // Verbatim: an invalid manifest names the field, which is the
                // whole reason to validate before writing.
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    let pick = move |ev: web_sys::Event| {
        let el: web_sys::HtmlInputElement = event_target(&ev);
        file.set(el.files().and_then(|l| l.get(0)));
        report.set(None);
        error.set(None);
        if file.get_untracked().is_some() {
            send(true);
        }
    };

    view! {
        <Modal open=open title="Import an agent" guard=true>
            <div class="wiz">
                <p class="dim mini-note">
                    "An agent file is the manifest someone exported from this or another gateway. "
                    "It is checked first — schema version, templates, and whether the MCP servers "
                    "it names are registered here — and a missing server is a warning, not a "
                    "refusal."
                </p>
                <div class="field" style="margin-top:10px">
                    <label>"Agent file (.agent.json)"</label>
                    <input class="input file" type="file" accept=".json" on:change=pick/>
                </div>
                <div class="field wide" style="margin-top:10px">
                    <label>"…or paste the manifest"</label>
                    <textarea
                        class="input ta mono"
                        placeholder="{ \"schema_version\": 1, … }"
                        prop:value=move || pasted.get()
                        on:input=move |ev| {
                            pasted.set(event_target_value(&ev));
                            report.set(None);
                            error.set(None);
                        }
                    ></textarea>
                </div>
                <div class="row" style="margin-top:8px">
                    <label class="check">
                        <input
                            type="checkbox"
                            prop:checked=move || replace.get()
                            on:change=move |ev| replace.set(event_target_checked(&ev))
                        />
                        "replace an agent that already has this id (its config is kept)"
                    </label>
                </div>

                {move || error.get().map(|e| view! { <div class="wiz-err">{e}</div> })}
                {move || {
                    report
                        .get()
                        .map(|r| {
                            let head = if r.validate_only {
                                format!("'{}' checks out — nothing written yet", r.id)
                            } else if r.replaced {
                                format!("replaced '{}', keeping its config", r.id)
                            } else {
                                format!("imported '{}'", r.id)
                            };
                            // mounts §5.2: a file names its slots and never a
                            // host path, so this is what is left to do — read
                            // before the For, which takes the report with it.
                            let unbound = unbound_line(&r.config_unbound);
                            view! {
                                <div class="wiz-ok" style="margin-top:12px">{head}</div>
                                <For each=move || r.warnings.clone() key=|w| w.clone() let:w>
                                    <div class="notice warn" style="margin-top:8px">{w}</div>
                                </For>
                                {unbound
                                    .map(|line| {
                                        view! {
                                            <div class="notice warn" style="margin-top:8px">
                                                {line}
                                            </div>
                                        }
                                    })}
                            }
                        })
                }}

                <ModalFooter>
                    <button class="btn ghost" disabled=move || busy.get() on:click=move |_| send(true)>
                        {move || if busy.get() { "Checking…" } else { "Check" }}
                    </button>
                    <button class="btn primary" disabled=move || busy.get() on:click=move |_| send(false)>
                        "Import"
                    </button>
                </ModalFooter>
            </div>
        </Modal>
    }
}

// ---------------------------------------------------------------------------
// Install from an image (container-runtime §3.4)
// ---------------------------------------------------------------------------

/// The pull policy picker's three words, with what each one does spelled out —
/// the choice is never implicit (§4.1), and "never" is the default because a
/// multi-gigabyte download is not something an install should start on its own.
pub fn pull_note(pull: &str) -> &'static str {
    match pull {
        "missing" => "download it if it is not on this box",
        "always" => "download it every time, even if it is already here",
        _ => "never download: if the image is not on this box, say so and stop",
    }
}

/// The pull policy's three choices, as the install dialog offers them.
fn pull_options() -> Vec<(String, String)> {
    [
        ("never", "never — do not download"),
        ("missing", "missing — download it once"),
        ("always", "always — download every time"),
    ]
    .into_iter()
    .map(|(v, l)| (v.to_string(), l.to_string()))
    .collect()
}

/// Install-from-image: an image reference, a visible pull policy, and the same
/// report an import produces.
///
/// No manifest is pasted anywhere — that is the whole point of a package. The
/// image carries its manifest at `/lmgw/agent.json`, lmgw reads it without
/// starting the image, and the install goes through the ordinary import path,
/// so every warning here is one the import would have raised too.
#[component]
fn InstallModal(open: RwSignal<bool>, on_change: Callback<()>) -> impl IntoView {
    let toasts = use_toasts();
    let image = RwSignal::new(String::new());
    let pull = RwSignal::new("never".to_string());
    let replace = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let report = RwSignal::new(None::<AgentImportReport>);
    let error = RwSignal::new(None::<String>);

    let send = move |validate_only: bool| {
        if busy.get_untracked() {
            return;
        }
        let ref_ = image.get_untracked().trim().to_string();
        if ref_.is_empty() {
            toasts.err("type an image reference first");
            return;
        }
        busy.set(true);
        report.set(None);
        error.set(None);
        spawn_local(async move {
            let body = json!({
                "image": ref_,
                "pull": pull.get_untracked(),
                "replace": replace.get_untracked(),
                "validate_only": validate_only,
            });
            // The answer carries more than the import report (image, digest,
            // whether anything was downloaded); the extra keys are ignored
            // here, and the Run tab prints them from the row's provenance.
            let res =
                crate::api::post::<AgentImportReport, _>("/api/op/agent_install", &body).await;
            busy.set(false);
            match res {
                Ok(r) => {
                    let committed = !r.validate_only;
                    let id = r.id.clone();
                    report.set(Some(r));
                    if committed {
                        toasts.ok(format!("installed '{id}'"));
                        on_change.try_run(());
                    }
                }
                // Verbatim: "the image carries no /lmgw/agent.json" and "the
                // image is not on this box and pull is never" are the two
                // answers this dialog exists to give.
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    view! {
        <Modal open=open title="Install an agent from an image" guard=true>
            <div class="wiz">
                <p class="dim mini-note">
                    "An agent package is an OCI image carrying its manifest at "
                    <code>"/lmgw/agent.json"</code>
                    ". lmgw reads it without starting the image (podman create, cp, rm) and "
                    "installs it through the same path an imported file takes — so the same "
                    "checks, and the same warnings for MCP servers this gateway does not have."
                </p>
                // A grid, so the image box and the policy take the dialog's
                // width (ux:U-17: a 170px box clipped its own placeholder), and
                // the app's Select rather than the one native <select> left,
                // which WebKitGTK draws light on the dark theme.
                <div class="field-grid" style="margin-top:10px">
                    <div class="field wide">
                        <label>"Image"</label>
                        <input
                            class="input mono-sm"
                            placeholder="localhost/mail-labeler:1"
                            prop:value=move || image.get()
                            on:input=move |ev| {
                                image.set(event_target_value(&ev));
                                report.set(None);
                                error.set(None);
                            }
                        />
                    </div>
                    <div class="field wide">
                        <label>"If the image is not on this box"</label>
                        <Select value=pull options=Signal::derive(pull_options)/>
                        <p class="dim mini-note" style="margin-top:6px">
                            {move || pull_note(&pull.get())}
                        </p>
                    </div>
                </div>
                <div class="row" style="margin-top:8px">
                    <label class="check">
                        <input
                            type="checkbox"
                            prop:checked=move || replace.get()
                            on:change=move |ev| replace.set(event_target_checked(&ev))
                        />
                        "replace an agent that already has the id inside the image (its config \
                         is kept)"
                    </label>
                </div>

                {move || error.get().map(|e| view! { <div class="wiz-err">{e}</div> })}
                {move || {
                    report
                        .get()
                        .map(|r| {
                            let head = if r.validate_only {
                                format!("'{}' checks out — nothing written yet", r.id)
                            } else if r.replaced {
                                format!("re-installed '{}', keeping its config", r.id)
                            } else {
                                format!("installed '{}'", r.id)
                            };
                            let unbound = unbound_line(&r.config_unbound);
                            view! {
                                <div class="wiz-ok" style="margin-top:12px">{head}</div>
                                <For each=move || r.warnings.clone() key=|w| w.clone() let:w>
                                    <div class="notice warn" style="margin-top:8px">{w}</div>
                                </For>
                                {unbound
                                    .map(|line| {
                                        view! {
                                            <div class="notice warn" style="margin-top:8px">
                                                {line}
                                            </div>
                                        }
                                    })}
                            }
                        })
                }}

                <ModalFooter>
                    <button
                        class="btn ghost"
                        disabled=move || busy.get()
                        on:click=move |_| send(true)
                    >
                        {move || if busy.get() { "Reading…" } else { "Check" }}
                    </button>
                    <button
                        class="btn primary"
                        disabled=move || busy.get()
                        on:click=move |_| send(false)
                    >
                        "Install"
                    </button>
                </ModalFooter>
            </div>
        </Modal>
    }
}

// ---------------------------------------------------------------------------
// New
// ---------------------------------------------------------------------------

#[component]
fn NewAgentModal(open: RwSignal<bool>) -> impl IntoView {
    let toasts = use_toasts();
    // `StoredValue`, not a local: a `<Modal>`'s children are re-rendered, so a
    // handler that moved a cloned navigator out of its environment would be
    // `FnOnce` and the whole child tree with it.
    let navigate = StoredValue::new(use_navigate());
    let id = RwSignal::new(String::new());
    let name = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let create = move |_| {
        if busy.get_untracked() {
            return;
        }
        let id_v = id.get_untracked().trim().to_string();
        let name_v = name.get_untracked().trim().to_string();
        if id_v.is_empty() || name_v.is_empty() {
            error.set(Some("an agent needs an id and a name".into()));
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let body = json!({
                // As a string, never an object: an object is parsed into a
                // sorted map on the way in and the config form comes back
                // alphabetized (see `AgentDetail::manifest`).
                "manifest": starter_manifest(&id_v, &name_v),
                "replace": false,
            });
            let res = crate::api::post::<Value, _>("/api/op/agent_set", &body).await;
            busy.set(false);
            match res {
                Ok(_) => {
                    open.set(false);
                    toasts.ok(format!("created '{id_v}' — edit its manifest here"));
                    if let Some(go) = navigate.try_get_value() {
                        go(&format!("/agents/{id_v}"), Default::default());
                    }
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    view! {
        <Modal open=open title="New agent" guard=true>
            <div class="wiz">
                <p class="dim mini-note">
                    "This writes the smallest valid manifest — a chat agent with a model picker — "
                    "and opens the new agent. Everything else, including turning it into a batch "
                    "agent, is an edit to that document on its Definition tab."
                </p>
                <div class="field" style="margin-top:10px">
                    <label>
                        "Id " <span class="fname">"a-z, 0-9 and dashes; the catalog key"</span>
                    </label>
                    <input
                        class="input mono"
                        style="width:100%"
                        placeholder="receipt-filer"
                        prop:value=move || id.get()
                        on:input=move |ev| id.set(event_target_value(&ev))
                    />
                </div>
                <div class="field" style="margin-top:10px">
                    <label>"Name"</label>
                    <input
                        class="input"
                        style="width:100%"
                        placeholder="Receipt filer"
                        prop:value=move || name.get()
                        on:input=move |ev| name.set(event_target_value(&ev))
                    />
                </div>
                {move || error.get().map(|e| view! { <div class="wiz-err">{e}</div> })}
                <ModalFooter>
                    <button class="btn ghost" on:click=move |_| open.set(false)>
                        "Cancel"
                    </button>
                    <button class="btn primary" disabled=move || busy.get() on:click=create>
                        {move || if busy.get() { "Creating…" } else { "Create" }}
                    </button>
                </ModalFooter>
            </div>
        </Modal>
    }
}

// ---------------------------------------------------------------------------
// `/workflows` → `/agents`
// ---------------------------------------------------------------------------

/// The old Workflows deep link. The mail workflow becomes the first catalog
/// entry (§7), so the path lands on the catalog rather than 404ing — and says
/// so for the half-second before the router moves.
///
/// The navigate runs in an effect rather than during render: writing the
/// router's location while the route that owns it is still rendering is the
/// kind of thing that works until it doesn't.
#[component]
pub fn WorkflowsMoved() -> impl IntoView {
    let navigate = use_navigate();
    Effect::new(move |_| navigate("/agents", Default::default()));
    view! {
        <PageFrame title="Agents" density=Density::Dense>
            <div class="card dim">"Workflows are now Agents — taking you there…"</div>
        </PageFrame>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::{AgentRequirement, AgentRunSummary};

    fn card() -> AgentCard {
        AgentCard {
            id: "mail-labeler".into(),
            name: "Mail labeler".into(),
            description: "Classifies unread Gmail into labels.".into(),
            kind: "batch".into(),
            effective_model: "gemma4-e4b".into(),
            labels: vec!["gws".into()],
            enabled: true,
            requires_ok: true,
            ..Default::default()
        }
    }

    /// Search covers what the card shows, so "the one that uses gws" and "the
    /// one on gemma" both find it.
    #[test]
    fn search_matches_every_field_the_card_shows() {
        let c = card();
        assert!(matches_query(&c, ""));
        assert!(matches_query(&c, "  "));
        assert!(matches_query(&c, "MAIL"));
        assert!(matches_query(&c, "gws"));
        assert!(matches_query(&c, "gemma4"));
        assert!(matches_query(&c, "batch"));
        assert!(matches_query(&c, "unread"));
        assert!(!matches_query(&c, "calendar"));
    }

    /// A chat agent counts threads, a batch agent names its last run, and
    /// neither invents activity it has not had.
    #[test]
    fn the_activity_line_follows_the_run_kind() {
        let mut c = card();
        assert_eq!(activity_line(&c), "never run");
        c.last_run = Some(AgentRunSummary {
            job_id: 4,
            status: "done".into(),
            finished_at: Some("2026-09-18 09:12:07".into()),
            ..Default::default()
        });
        assert_eq!(activity_line(&c), "last run: done 2026-09-18 09:12");

        c.kind = "chat".into();
        assert_eq!(activity_line(&c), "no threads yet");
        c.threads = 1;
        assert_eq!(activity_line(&c), "1 thread");
        c.threads = 3;
        assert_eq!(activity_line(&c), "3 threads");
    }

    /// The gap is named on the card: which server, or which tools of a server
    /// that is registered but does not list them.
    #[test]
    fn the_warning_names_the_gap_and_stays_quiet_when_there_is_none() {
        let mut c = card();
        assert_eq!(requires_warning(&c), None);

        c.requires_ok = false;
        c.requires = vec![AgentRequirement {
            label: "gws".into(),
            registered: false,
            ..Default::default()
        }];
        assert_eq!(
            requires_warning(&c).as_deref(),
            Some("needs MCP server 'gws'")
        );

        c.requires = vec![AgentRequirement {
            label: "gws".into(),
            registered: true,
            missing_tools: vec!["gws__gmail_batchModify".into()],
            ..Default::default()
        }];
        assert_eq!(
            requires_warning(&c).as_deref(),
            Some("'gws' is missing gws__gmail_batchModify"),
        );
    }

    /// What New writes has to be a manifest the server accepts, with the two
    /// values quoted rather than pasted — an id is validated server-side, but a
    /// name with a quote in it must not produce broken JSON on the way there.
    #[test]
    fn the_starter_manifest_is_json_with_the_fields_the_schema_requires() {
        let text = starter_manifest("receipt-filer", "Alice's \"filer\"");
        let v: Value = serde_json::from_str(&text).expect("starter manifest is not JSON");
        assert_eq!(v["schema_version"], json!(1));
        assert_eq!(v["id"], json!("receipt-filer"));
        assert_eq!(v["name"], json!("Alice's \"filer\""));
        assert_eq!(v["run"]["kind"], json!("chat"));
        // The model comes from the config form, so a fresh agent is runnable on
        // a machine that has never heard of whatever alias was authored.
        assert_eq!(v["model"]["alias"], json!("{{config.model}}"));
        assert_eq!(
            v["config"]["schema"]["properties"]["model"]["format"],
            json!("model_alias")
        );
    }
}
