use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{AgentDetail, AgentField, AgentImportReport};
use serde_json::json;

use super::*;
use crate::pages::agents::short_ts;
use crate::widgets::nav_split::SaveKey;
use crate::widgets::schema_form::{access_of, label_of, RELABEL_NOTE, RELABEL_SUMMARY};
use crate::widgets::{use_toasts, Explain, SaveToast, WireName};

// ---------------------------------------------------------------------------
// Definition
// ---------------------------------------------------------------------------

#[component]
pub(super) fn DefinitionTab(
    /// The agent as last read. The tab is made once per agent and only the
    /// parts around the editor follow it, so a re-read (a run's end, a save)
    /// never replaces the editor being typed in.
    d: Memo<AgentDetail>,
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
    let busy = RwSignal::new(false);
    // A refused Save, in the save toast; a refused Validate, over the
    // editor (it is no save that failed).
    let error = RwSignal::new(None::<String>);
    let check_error = RwSignal::new(None::<String>);
    let pane: NodeRef<leptos::html::Div> = NodeRef::new();
    let portability = Memo::new(move |_| d.with(|d| d.portability.clone()));
    // What this manifest asks the owner for (mounts §5.8). The definition is
    // where the slots are declared; the Run tab is where they are bound.
    let mount_rows = Memo::new(move |_| d.with(|d| mount_fields(&d.fields)));
    // Every declared slot, which is what an export names — bound or not, the
    // receiving box has to fill all of them in.
    let export_unbound = move || {
        mount_rows.with(|m| unbound_line(&m.iter().map(|f| f.name.clone()).collect::<Vec<_>>()))
    };
    let scope_models = Memo::new(move |_| d.with(|d| d.token.scope_models.clone()));
    let updated = Memo::new(move |_| d.with(|d| short_ts(&d.updated_at)));
    // The one warning that is about the token rather than about the manifest
    // (principals §3.10): while *require gateway API keys* is off, an anonymous
    // caller holds `Inference`, so this token's scope, allow-list and budget
    // bind only a container that bothers to present it. Non-blocking, and it
    // belongs with the token rather than in the Run tab's list.
    let token_advisory = Memo::new(move |_| {
        d.with(|d| {
            d.warnings
                .iter()
                .find(|w| w.code == "token_scope_advisory")
                .map(|w| w.message.clone())
                .filter(|m| !m.is_empty())
        })
    });
    let send = move |validate_only: bool| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        check_error.set(None);
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
                Err(e) if validate_only => check_error.set(Some(e.to_string())),
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    let unsaved = Memo::new(move |_| text.with(|t| base.with(|b| t != b)));
    // Ctrl+S saves the manifest while this tab is shown.
    SaveKey::install(Callback::new(move |()| {
        if unsaved.get_untracked() {
            send(false);
        }
    }));

    let head = move || lines.with(|l| l.first().cloned());
    let rest = move || lines.with(|l| l.iter().skip(1).cloned().collect::<Vec<_>>());
    let not_portable = Memo::new(move |_| portability.with(|p| !p.portable));
    let has_mounts = move || mount_rows.with(|m| !m.is_empty());
    view! {
        <div class="def-bar">
            <span class="dim mono-sm def-updated">{move || format!("updated {}", updated.get())}</span>
            // Which models this agent's bearer can reach (par:PAR-7): it
            // changes with every saved model field, and the token advisory
            // below talks about it.
            <span class="dim def-scope" title="the model scope of this agent's own token">
                {move || {
                    let models = scope_models.get();
                    if models.is_empty() {
                        "Its token reaches any model".into_any()
                    } else {
                        view! {
                            <span>"Its token reaches"</span>
                            {models.into_iter().map(|m| view! { <span class="chip mono-sm">{m}</span> }).collect_view()}
                        }
                            .into_any()
                    }
                }}
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
            <button
                class="btn flat"
                disabled=move || busy.get()
                title="Check the editor's manifest without writing it"
                on:click=move |_| send(true)
            >
                {move || if busy.get() { "Checking…" } else { "Validate" }}
            </button>
        </div>

        // One scroller, the pane (ux:U-2): what needs the owner's eye — the
        // report's warnings, the token advisory, "will not run elsewhere" —
        // comes first, and the editor below is as tall as the wrapped
        // manifest, so it never scrolls inside the pane that scrolls it.
        <div class="fill-pane def-pane" node_ref=pane>
            {move || check_error.get().map(|e| view! { <div class="notice err">{e}</div> })}
            <For each=rest key=|w| w.clone() let:w>
                <div class="notice warn">{w}</div>
            </For>
            {move || token_advisory
                .get()
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
            {move || not_portable
                .get()
                .then(|| {
                    view! {
                        <div class="notice warn">
                            <b>"This export will not run as-is on another box."</b>
                            <ul class="def-notes">
                                <For each=move || portability.with(|p| p.notes.clone()) key=|n| n.clone() let:note>
                                    <li>{note}</li>
                                </For>
                            </ul>
                        </div>
                    }
                })}
            <ManifestEditor text=text pane=pane/>
            {move || (!not_portable.get())
                .then(|| {
                    view! {
                        <p class="dim mini-note">
                            "Export: the manifest only — never the token, the provenance or the "
                            "dev_url. Nothing about this agent is local to this machine."
                        </p>
                    }
                })}
            {move || export_unbound().map(|line| view! { <p class="dim mini-note">{line}</p> })}

            <Show when=has_mounts>
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
                            <For each=move || mount_rows.get() key=|f: &AgentField| f.name.clone() let:f>
                                <tr>
                                    <td>
                                        {label_of(&f)} " " <WireName name=f.name.clone()/>
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

        <SaveToast
            dirty_count=Signal::derive(move || usize::from(unsaved.get()))
            saving=busy
            error=error
            on_save=Callback::new(move |()| send(false))
            on_discard=Callback::new(move |()| {
                error.set(None);
                text.set(base.get_untracked());
            })
            save_label="Save manifest"
        />
    }
}
