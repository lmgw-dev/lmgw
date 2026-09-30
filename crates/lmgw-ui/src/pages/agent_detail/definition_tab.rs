use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;
use lmgw_api_types::{AgentDetail, AgentField, AgentImportReport};
use serde_json::{json, Value};
use wasm_bindgen::JsCast;

use super::*;
use crate::pages::agents::short_ts;
use crate::widgets::schema_form::{access_of, label_of, RELABEL_NOTE, RELABEL_SUMMARY};
use crate::widgets::{use_toasts, Explain, MenuItem, Modal, ModalFooter, RowMenu};

// ---------------------------------------------------------------------------
// Definition
// ---------------------------------------------------------------------------

/// Start a download the way a click on `<a download>` does. The export is an
/// attachment on the gateway's own origin, so the session rides along; the
/// router leaves a `download` link alone.
fn download(href: &str, filename: &str) {
    let doc = document();
    let Ok(el) = doc.create_element("a") else {
        return;
    };
    let _ = el.set_attribute("href", href);
    let _ = el.set_attribute("download", filename);
    let Ok(a) = el.dyn_into::<web_sys::HtmlElement>() else {
        return;
    };
    if let Some(body) = doc.body() {
        let _ = body.append_child(&a);
        a.click();
        let _ = body.remove_child(&a);
    }
}

#[component]
pub(super) fn DefinitionTab(
    d: AgentDetail,
    /// The editor's text, owned by the page so a tab switch does not discard
    /// an unsaved manifest.
    text: RwSignal<String>,
    /// The manifest as stored; the editor is unsaved while it differs.
    base: RwSignal<String>,
    /// The inline report of the last Validate or Save, owned by the page for
    /// the same reason: a save re-reads the agent, which rebuilds this tab.
    lines: RwSignal<Vec<String>>,
    reload: Callback<()>,
) -> impl IntoView {
    let toasts = use_toasts();
    let navigate = StoredValue::new(use_navigate());
    let id = StoredValue::new(d.id.clone());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let dup_open = RwSignal::new(false);
    let del_open = RwSignal::new(false);
    let resettable = d.resettable;
    let portability = d.portability.clone();
    // What this manifest asks the owner for (mounts §5.8). The definition is
    // where the slots are declared; the Run tab is where they are bound.
    let mount_rows = mount_fields(&d.fields);
    // Every declared slot, which is what an export names — bound or not, the
    // receiving box has to fill all of them in.
    let export_unbound = unbound_line(
        &mount_rows
            .iter()
            .map(|f| f.name.clone())
            .collect::<Vec<_>>(),
    );
    let scope_note = StoredValue::new(d.token.scope_note.clone());
    // The one warning that is about the token rather than about the manifest
    // (principals §3.10): while *require gateway API keys* is off, an anonymous
    // caller holds `Inference`, so this token's scope, allow-list and budget
    // bind only a container that bothers to present it. Non-blocking, and it
    // belongs with the token rather than in the Run tab's list.
    let token_advisory = d
        .warnings
        .iter()
        .find(|w| w.code == "token_scope_advisory")
        .map(|w| w.message.clone())
        .filter(|m| !m.is_empty());
    // What is holding the token right now, for the rotate confirm.
    let live_run = d
        .live_job
        .as_ref()
        .map(|j| {
            format!(
                "Run #{} is in flight. It was handed the current token in its secrets.json and \
                 cannot be given the new one, so its next gateway call fails.",
                j.job_id
            )
        })
        .unwrap_or_default();
    let live_app = d
        .service
        .as_ref()
        .filter(|s| s.running)
        .map(|s| {
            format!(
                "The app container {} is running and holding the current token. Rotating stops \
                 it; the next request to the App tab starts it again with the new one.",
                s.container
            )
        })
        .unwrap_or_default();

    // The agent's own token (container-runtime §3.1). Copy, not reveal-once:
    // lmgw stores the plaintext because it has to hand it to a container on the
    // *second* run too. The value is fetched on click rather than carried in
    // the page payload, so it is on screen for as long as it takes to paste it
    // and no longer.
    let copy_token = move || {
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                "/api/op/agent_token_get",
                &json!({ "id": id.get_value() }),
            )
            .await;
            match res {
                Ok(v) => {
                    let token = v.get("token").and_then(Value::as_str).unwrap_or_default();
                    let ok = v
                        .get("scope_note")
                        .and_then(Value::as_str)
                        .map(|s| format!("token copied — {s}"))
                        .unwrap_or_else(|| "token copied".into());
                    crate::widgets::copy_secret(token, toasts, ok);
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    // Rotation is not undoable and it is not free: there is exactly one
    // `(key_hash, key_plain)` pair per agent and rotation replaces both in one
    // write, so anything holding the old token starts getting 401s at once
    // (container-runtime §12). The confirm names what that is *right now* —
    // the live run, the running app container — instead of letting the owner
    // find out from a failed run.
    let rotate_open = RwSignal::new(false);
    let rotate = move |_| {
        rotate_open.set(false);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                "/api/op/agent_token_rotate",
                &json!({ "id": id.get_value() }),
            )
            .await;
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("token rotated")
                        .to_string());
                    reload.try_run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let send = move |validate_only: bool| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        lines.set(Vec::new());
        let sent = text.get_untracked();
        spawn_local(async move {
            let body = json!({
                // As text. An object argument arrives already sorted and the
                // author's field order is gone (§2.6).
                "manifest": sent,
                "replace": true,
                "validate_only": validate_only,
            });
            let res = crate::api::post::<AgentImportReport, _>("/api/op/agent_set", &body).await;
            busy.set(false);
            match res {
                Ok(r) => {
                    lines.set(report_lines(&r));
                    if !r.validate_only {
                        toasts.ok("manifest saved");
                        // What was sent is stored: the re-read may replace
                        // it with the canonical form. Typing since stays.
                        base.set(body["manifest"].as_str().unwrap_or_default().to_string());
                        reload.try_run(());
                    }
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    let reset = move || {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                "/api/op/agent_reset",
                &json!({ "id": id.get_value() }),
            )
            .await;
            busy.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("reset")
                        .to_string());
                    reload.try_run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let delete = move || {
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                "/api/op/agent_delete",
                &json!({ "id": id.get_value() }),
            )
            .await;
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("deleted")
                        .to_string());
                    del_open.set(false);
                    if let Some(go) = navigate.try_get_value() {
                        go("/agents", Default::default());
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    // The file-level acts, apart from the editing flow: Delete is never next
    // to Save, and it is the last thing in the menu.
    let file_menu = Signal::derive(move || {
        let file = format!("{}.agent.json", id.get_value());
        let with_config = file.clone();
        let mut v = vec![
            MenuItem::new("Export manifest", move || {
                download(&export_href(&id.get_value(), false), &file)
            })
            .title("the manifest only — never the token, the provenance or the dev_url"),
            MenuItem::new("Export with config values", move || {
                download(&export_href(&id.get_value(), true), &with_config)
            })
            .title("the manifest plus the non-secret config values; secrets never leave"),
            MenuItem::new("Duplicate…", move || dup_open.set(true)),
            MenuItem::new("Copy agent token", copy_token).title(format!(
                "the agent's own bearer token, for a container or a dev process — {}",
                scope_note.get_value()
            )),
            MenuItem::new("Rotate token…", move || rotate_open.set(true))
                .title("replace the token; the old one stops working at once"),
        ];
        if resettable {
            // Any edit made to the manifest is gone after this, so it takes
            // the second pick.
            v.push(
                MenuItem::new("Reset to shipped", reset)
                    .danger()
                    .disabled(busy.get())
                    .title("put the shipped manifest back, keeping the config"),
            );
        }
        v.push(
            MenuItem::new("Delete agent…", move || del_open.set(true))
                .title("remove it from the catalog; its chat threads are kept"),
        );
        v
    });

    let head = move || lines.with(|l| l.first().cloned());
    let rest = move || lines.with(|l| l.iter().skip(1).cloned().collect::<Vec<_>>());
    let not_portable = !portability.portable;
    let notes = StoredValue::new(portability.notes.clone());
    let mount_rows = StoredValue::new(mount_rows);
    let has_mounts = mount_rows.with_value(|m| !m.is_empty());

    view! {
        <div class="def-bar">
            <span class="dim mono-sm">{format!("updated {}", short_ts(&d.updated_at))}</span>
            // Which models this agent's bearer can reach (par:PAR-7): it
            // changes with every saved model field, and the token advisory
            // below talks about it.
            <span class="dim mono-sm def-scope" title="the model scope of this agent's own token">
                {scope_note.get_value()}
            </span>
            // The outcome is the green line; the warnings under the bar are
            // amber. Both are the same report (§5), but a missing MCP server
            // printed in the "it worked" colour is the one thing this must not
            // do — warnings do not block a save, they still read as warnings.
            {move || {
                head()
                    .map(|h| {
                        let tip = h.clone();
                        view! { <span class="def-report" title=tip>{h}</span> }
                    })
            }}
            <span class="spacer"></span>
            <Show when=move || text.with(|t| base.with(|b| t != b))>
                <span class="count attn" title="The editor differs from the stored manifest">
                    "unsaved"
                </span>
                <button
                    class="link-btn"
                    title="Put the stored manifest back in the editor"
                    on:click=move |_| text.set(base.get_untracked())
                >
                    "Discard"
                </button>
            </Show>
            <button class="btn" disabled=move || busy.get() on:click=move |_| send(true)>
                "Validate"
            </button>
            <button class="btn primary" disabled=move || busy.get() on:click=move |_| send(false)>
                {move || if busy.get() { "Saving…" } else { "Save" }}
            </button>
            <RowMenu
                items=file_menu
                title="Export, duplicate, the agent's token, reset, delete"
                label=Signal::derive(|| "File ▾".to_string())
            />
        </div>
        {move || error.get().map(|e| view! { <div class="notice err">{e}</div> })}

        // One scroller, the pane (ux:U-2): what needs the owner's eye — the
        // report's warnings, the token advisory, "will not run elsewhere" —
        // comes first, and the editor below is as tall as the manifest, so it
        // never scrolls inside the pane that scrolls it.
        <div class="fill-pane def-pane">
            <For each=rest key=|w| w.clone() let:w>
                <div class="notice warn">{w}</div>
            </For>
            {token_advisory
                .map(|m| {
                    view! {
                        <div class="notice warn">
                            <b>{m}</b>
                            <span class="detail">
                                "The checkbox is "
                                <i>"require gateway API keys on /v1"</i>
                                ", under "
                                <a href=crate::pages::settings_href("auth_enabled")>
                                    "Settings → Network & access"
                                </a> "."
                            </span>
                        </div>
                    }
                })}
            // §3.4: what the export will and will not carry, before it is
            // downloaded. A file that names a `localhost/…` image, or an agent
            // that only works here because of a dev_url, is not portable — and
            // the export says so in the file too, under `portability`.
            {not_portable
                .then(|| {
                    view! {
                        <div class="notice warn">
                            <b>"This export will not run as-is on another box."</b>
                            <ul class="def-notes">
                                <For each=move || notes.get_value() key=|n| n.clone() let:note>
                                    <li>{note}</li>
                                </For>
                            </ul>
                        </div>
                    }
                })}
            <textarea
                class="input ta mono manifest-editor"
                spellcheck="false"
                rows=move || text.with(|t| t.lines().count() + 1)
                prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
            ></textarea>
            {(!not_portable)
                .then(|| {
                    view! {
                        <p class="dim mini-note">
                            "Export: the manifest only — never the token, the provenance or the "
                            "dev_url. Nothing about this agent is local to this machine."
                        </p>
                    }
                })}
            {export_unbound.map(|line| view! { <p class="dim mini-note">{line}</p> })}

            <Show when=move || has_mounts>
                <section class="card">
                    <div class="mini-head">"Mounts"</div>
                    <p class="dim mini-note">
                        "A manifest can name a slot, never a host path. Bind each one under "
                        "Config on the Run tab; the container sees it at "
                        <code>"/lmgw/mounts/<field>"</code>
                        " and never learns yours."
                    </p>
                    <table class="data">
                        <thead>
                            <tr>
                                <th>"Field"</th>
                                <th>"Kind"</th>
                                <th>"Access"</th>
                                <th>"Required"</th>
                            </tr>
                        </thead>
                        <tbody>
                            <For each=move || mount_rows.get_value() key=|f: &AgentField| f.name.clone() let:f>
                                <tr>
                                    <td>
                                        {label_of(&f)} " " <span class="fname">{f.name.clone()}</span>
                                    </td>
                                    <td class="mono-sm">{f.format.clone()}</td>
                                    <td class="mono-sm">{access_of(&f).to_string()}</td>
                                    <td class="mono-sm">
                                        {if f.required { "required" } else { "optional" }}
                                    </td>
                                </tr>
                            </For>
                        </tbody>
                    </table>
                    <Explain summary=RELABEL_SUMMARY persist="agents.relabel">
                        {RELABEL_NOTE}
                    </Explain>
                </section>
            </Show>
        </div>

        <DuplicateModal open=dup_open id=d.id.clone()/>

        // The confirm names what is holding the current token right now
        // (container-runtime §12): rotation has no grace window, and a live run
        // or a running app container is what will start failing the moment this
        // button is pressed.
        <Modal open=rotate_open title="Rotate this agent's token">
            <p class="dim">
                "There is one token per agent and rotating replaces it in one write — there is "
                "no grace window. Anything holding the old one starts getting 401s at once."
            </p>
            {(!live_run.is_empty())
                .then(|| {
                    view! { <p class="notice warn">{live_run.clone()}</p> }
                })}
            {(!live_app.is_empty())
                .then(|| {
                    view! { <p class="notice warn">{live_app.clone()}</p> }
                })}
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| rotate_open.set(false)>
                    "Cancel"
                </button>
                <button class="btn danger" on:click=rotate>
                    "Rotate the token"
                </button>
            </ModalFooter>
        </Modal>

        <Modal open=del_open title="Delete this agent">
            <p class="dim">
                {format!(
                    "'{}' is removed from the catalog. Its chat threads are kept and stay on the \
                     Chat page — a conversation is yours, the agent is only the preset it was had \
                     through.",
                    d.id,
                )}
            </p>
            {resettable
                .then(|| {
                    view! {
                        <p class="dim mini-note">
                            "This is one of the shipped agents. It stays deleted across restarts; "
                            "\"Restore shipped\" on the catalog is the way back."
                        </p>
                    }
                })}
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| del_open.set(false)>
                    "Cancel"
                </button>
                <button class="btn danger" on:click=move |_| delete()>
                    "Delete"
                </button>
            </ModalFooter>
        </Modal>
    }
}

#[component]
fn DuplicateModal(open: RwSignal<bool>, id: String) -> impl IntoView {
    let toasts = use_toasts();
    let navigate = StoredValue::new(use_navigate());
    let source = StoredValue::new(id.clone());
    let new_id = RwSignal::new(copy_id(&id));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let create = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let target = new_id.get_untracked().trim().to_string();
            let res = crate::api::post::<Value, _>(
                "/api/op/agent_duplicate",
                &json!({ "id": source.get_value(), "new_id": target }),
            )
            .await;
            busy.set(false);
            match res {
                Ok(v) => {
                    open.set(false);
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("duplicated")
                        .to_string());
                    if let Some(go) = navigate.try_get_value() {
                        go(&format!("/agents/{target}"), Default::default());
                    }
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    view! {
        <Modal open=open title="Duplicate this agent">
            <p class="dim mini-note">
                "The copy carries the same manifest and the same config — minus every secret "
                "field, which a copy must not silently inherit."
            </p>
            <div class="field">
                <label>"New id"</label>
                <input
                    class="input mono w-md"
                    prop:value=move || new_id.get()
                    on:input=move |ev| new_id.set(event_target_value(&ev))
                />
            </div>
            {move || error.get().map(|e| view! { <div class="wiz-err">{e}</div> })}
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| open.set(false)>
                    "Cancel"
                </button>
                <button class="btn primary" disabled=move || busy.get() on:click=create>
                    "Duplicate"
                </button>
            </ModalFooter>
        </Modal>
    }
}
