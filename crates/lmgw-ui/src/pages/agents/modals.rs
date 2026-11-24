//! The catalog's three ways in (agent-catalog design §5, container-runtime
//! §3.4): Import a manifest file or paste, Install an agent package from an
//! image, and New, which writes a starter manifest and opens it. They open
//! from the list's add menu and the status page.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{AgentImportReport, AgentInstalled};
use serde_json::{json, Value};
use wasm_bindgen::JsValue;

use super::super::agent_detail::unbound_line;
use super::{pull_note, pull_options, starter_manifest, AgentsCtx};
use crate::widgets::{use_toasts, Modal, ModalFooter, PrefGroup, Select};

// ---------------------------------------------------------------------------
// Import (§5) — file or paste, one endpoint
// ---------------------------------------------------------------------------

#[component]
pub(super) fn ImportModal(open: RwSignal<bool>, on_change: Callback<()>) -> impl IntoView {
    let toasts = use_toasts();
    let file = RwSignal::new(None::<web_sys::File>);
    let file_input: NodeRef<leptos::html::Input> = NodeRef::new();
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
                        toasts.ok(format!("Imported '{id}'"));
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
                <PrefGroup>
                    <div class="prow">
                        <div class="prow-text">
                            <span class="prow-title">"Agent file"</span>
                            <span class="prow-sub">
                                {move || {
                                    file.with(|f| f.as_ref().map(|f| f.name()))
                                        .unwrap_or_else(|| ".agent.json".to_string())
                                }}
                            </span>
                        </div>
                        <input node_ref=file_input type="file" hidden accept=".json" on:change=pick/>
                        <button
                            type="button"
                            class="btn"
                            on:click=move |_| {
                                if let Some(i) = file_input.get() {
                                    i.click();
                                }
                            }
                        >
                            "Choose file…"
                        </button>
                    </div>
                </PrefGroup>
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
                                format!("Checks out: '{}' — nothing written yet", r.id)
                            } else if r.replaced {
                                format!("Replaced '{}', keeping its config", r.id)
                            } else {
                                format!("Imported '{}'", r.id)
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
                    <button class="btn" disabled=move || busy.get() on:click=move |_| send(true)>
                        {move || if busy.get() { "Checking…" } else { "Check" }}
                    </button>
                    <button class="btn suggested" disabled=move || busy.get() on:click=move |_| send(false)>
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

/// Install-from-image: an image reference, a visible pull policy, and the same
/// report an import produces.
///
/// No manifest is pasted anywhere — that is the whole point of a package. The
/// image carries its manifest at `/lmgw/agent.json`, lmgw reads it without
/// starting the image, and the install goes through the ordinary import path,
/// so every warning here is one the import would have raised too.
#[component]
pub(super) fn InstallModal(open: RwSignal<bool>, on_change: Callback<()>) -> impl IntoView {
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
            // whether anything was downloaded); the dialog shows the report,
            // and the Run tab prints the rest from the row's provenance.
            let res = crate::api::post::<AgentInstalled, _>("/api/op/agent_install", &body).await;
            busy.set(false);
            match res {
                Ok(installed) => {
                    let r = installed.report;
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
                                format!("Checks out: '{}' — nothing written yet", r.id)
                            } else if r.replaced {
                                format!("Re-installed '{}', keeping its config", r.id)
                            } else {
                                format!("Installed '{}'", r.id)
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
                        class="btn"
                        disabled=move || busy.get()
                        on:click=move |_| send(true)
                    >
                        {move || if busy.get() { "Reading…" } else { "Check" }}
                    </button>
                    <button
                        class="btn suggested"
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
pub(super) fn NewAgentModal(open: RwSignal<bool>, page: AgentsCtx) -> impl IntoView {
    let toasts = use_toasts();
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
                    page.reload.run(());
                    page.go(&format!("/agents/{id_v}"), false);
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
                    <button class="btn" on:click=move |_| open.set(false)>
                        "Cancel"
                    </button>
                    <button class="btn suggested" disabled=move || busy.get() on:click=create>
                        {move || if busy.get() { "Creating…" } else { "Create" }}
                    </button>
                </ModalFooter>
            </div>
        </Modal>
    }
}
