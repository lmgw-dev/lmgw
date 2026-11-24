//! The agent's own acts, apart from any tab: the header's menu (Enable or
//! Disable, the two exports, Duplicate, the agent's token, Reset to shipped,
//! Delete last) and the dialogs it opens. Delete is never next to a Save:
//! it is the menu's last item and the Run tab's danger row.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{
    AgentDeleted, AgentDetail, AgentDuplicated, AgentEnabled, AgentReset, AgentTokenIssued,
    AgentTokenRead,
};
use serde_json::json;

use super::*;
use crate::pages::agents::AgentsCtx;
use crate::pages::docs::download;
use crate::widgets::{use_toasts, MenuItem, Modal, ModalFooter};

/// The dialogs the menu opens, and whether an op it started is out. Owned
/// by the detail, so a re-read of the agent does not close a dialog.
#[derive(Clone, Copy)]
pub(super) struct AgentActs {
    pub dup_open: RwSignal<bool>,
    pub rotate_open: RwSignal<bool>,
    pub del_open: RwSignal<bool>,
    pub busy: RwSignal<bool>,
    toasts: crate::widgets::Toasts,
}

impl AgentActs {
    pub fn new() -> Self {
        Self {
            dup_open: RwSignal::new(false),
            rotate_open: RwSignal::new(false),
            del_open: RwSignal::new(false),
            busy: RwSignal::new(false),
            toasts: use_toasts(),
        }
    }

    /// Switch the agent on or off (`agent_enable`): a disabled agent stays in
    /// the catalog and refuses to run.
    pub fn set_enabled(self, id: String, want: bool, reload: Callback<()>) {
        if self.busy.get_untracked() {
            return;
        }
        let (busy, toasts) = (self.busy, self.toasts);
        busy.set(true);
        spawn_local(async move {
            let res = crate::api::post::<AgentEnabled, _>(
                "/api/op/agent_enable",
                &json!({ "id": id, "enabled": want }),
            )
            .await;
            busy.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v.message);
                    reload.try_run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    }
}

/// The header's menu for one agent.
pub(super) fn menu_items(d: &AgentDetail, acts: AgentActs, reload: Callback<()>) -> Vec<MenuItem> {
    let toasts = acts.toasts;
    let id = StoredValue::new(d.id.clone());
    let enabled = d.enabled;
    let scope_note = d.token.scope_note.clone();
    let file = format!("{}.agent.json", d.id);
    let with_config = file.clone();

    // The agent's own token (container-runtime §3.1). Copy, not reveal-once:
    // lmgw stores the plaintext because it has to hand it to a container on
    // the *second* run too. The value is fetched on click rather than carried
    // in the page payload, so it is on screen for as long as it takes to
    // paste it and no longer.
    let copy_token = move || {
        spawn_local(async move {
            let res = crate::api::post::<AgentTokenRead, _>(
                "/api/op/agent_token_get",
                &json!({ "id": id.get_value() }),
            )
            .await;
            match res {
                Ok(v) => {
                    let ok = if v.scope_note.is_empty() {
                        "token copied".to_string()
                    } else {
                        format!("Token copied. {}", v.scope_note)
                    };
                    crate::widgets::copy_secret(&v.token, toasts, ok);
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let reset = move || {
        if acts.busy.get_untracked() {
            return;
        }
        acts.busy.set(true);
        spawn_local(async move {
            let res = crate::api::post::<AgentReset, _>(
                "/api/op/agent_reset",
                &json!({ "id": id.get_value() }),
            )
            .await;
            acts.busy.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v.message);
                    reload.try_run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let mut v = vec![
        MenuItem::new(if enabled { "Disable" } else { "Enable" }, move || {
            acts.set_enabled(id.get_value(), !enabled, reload)
        })
        .disabled(acts.busy.get_untracked())
        .title(if enabled {
            "A disabled agent stays in the catalog and refuses to run"
        } else {
            "Let it run again"
        }),
        MenuItem::new("Export manifest", move || {
            download(&export_href(&id.get_value(), false), &file)
        })
        .title("the manifest only — never the token, the provenance or the dev_url"),
        MenuItem::new("Export with config values", move || {
            download(&export_href(&id.get_value(), true), &with_config)
        })
        .title("the manifest plus the non-secret config values; secrets never leave"),
        MenuItem::new("Duplicate…", move || acts.dup_open.set(true)),
        MenuItem::new("Copy agent token", copy_token).title(format!(
            "the agent's own bearer token, for a container or a dev process. {scope_note}"
        )),
        MenuItem::new("Rotate token…", move || acts.rotate_open.set(true))
            .title("replace the token; the old one stops working at once"),
    ];
    if d.resettable {
        // Any edit made to the manifest is gone after this, so it takes the
        // second pick.
        v.push(
            MenuItem::new("Reset to shipped", reset)
                .danger()
                .disabled(acts.busy.get_untracked())
                .title("put the shipped manifest back, keeping the config"),
        );
    }
    v.push(
        // Not armed like Reset: it opens a dialog that asks.
        MenuItem::new("Delete agent…", move || acts.del_open.set(true))
            .destructive()
            .title("remove it from the catalog; its chat threads are kept"),
    );
    v
}

/// Duplicate, Rotate token and Delete, as the menu opens them.
#[component]
pub(super) fn AgentDialogs(
    /// The agent as last read: the dialogs are made once per agent, so one
    /// that is open stays open (and keeps what was typed in it) through a
    /// re-read; what they say about the agent follows it.
    d: Memo<AgentDetail>,
    acts: AgentActs,
    reload: Callback<()>,
) -> impl IntoView {
    let toasts = use_toasts();
    let page = use_context::<AgentsCtx>();
    let id = StoredValue::new(d.with_untracked(|d| d.id.clone()));
    let resettable = move || d.with(|d| d.resettable);
    // What is holding the token right now, for the rotate confirm.
    let live_run = move || {
        d.with(|d| {
            d.live_job.as_ref().map(|j| {
                format!(
                    "Run #{} is in flight. It was handed the current token in its secrets.json \
                     and cannot be given the new one, so its next gateway call fails.",
                    j.job_id
                )
            })
        })
    };
    let live_app = move || {
        d.with(|d| {
            d.service.as_ref().filter(|s| s.running).map(|s| {
                format!(
                    "The app container {} is running and holding the current token. Rotating \
                     stops it; the next request to the App tab starts it again with the new one.",
                    s.container
                )
            })
        })
    };

    // Rotation is not undoable and it is not free: there is exactly one
    // `(key_hash, key_plain)` pair per agent and rotation replaces both in one
    // write, so anything holding the old token starts getting 401s at once
    // (container-runtime §12). The confirm names what that is *right now* —
    // the live run, the running app container — instead of letting the owner
    // find out from a failed run.
    let rotate = move |_| {
        acts.rotate_open.set(false);
        spawn_local(async move {
            let res = crate::api::post::<AgentTokenIssued, _>(
                "/api/op/agent_token_rotate",
                &json!({ "id": id.get_value() }),
            )
            .await;
            match res {
                Ok(v) => {
                    toasts.ok(v.message);
                    reload.try_run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let delete = move || {
        spawn_local(async move {
            let res = crate::api::post::<AgentDeleted, _>(
                "/api/op/agent_delete",
                &json!({ "id": id.get_value() }),
            )
            .await;
            match res {
                Ok(deleted) => {
                    toasts.ok(deleted.message);
                    acts.del_open.set(false);
                    // Its unsaved edits have no agent left to go to: back to
                    // the list without asking about them.
                    if let Some(p) = page {
                        p.reload.run(());
                        p.go_unasked("/agents", true);
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    view! {
        <DuplicateModal open=acts.dup_open id=id.get_value()/>

        // The confirm names what is holding the current token right now
        // (container-runtime §12): rotation has no grace window, and a live run
        // or a running app container is what will start failing the moment this
        // button is pressed.
        <Modal open=acts.rotate_open title="Rotate this agent's token">
            <p class="dim">
                "There is one token per agent and rotating replaces it in one write — there is "
                "no grace window. Anything holding the old one starts getting 401s at once."
            </p>
            {move || live_run().map(|t| view! { <p class="notice warn">{t}</p> })}
            {move || live_app().map(|t| view! { <p class="notice warn">{t}</p> })}
            <ModalFooter>
                <button class="btn" on:click=move |_| acts.rotate_open.set(false)>
                    "Cancel"
                </button>
                <button class="btn destructive" on:click=rotate>
                    "Rotate the token"
                </button>
            </ModalFooter>
        </Modal>

        <Modal open=acts.del_open title="Delete this agent">
            <p class="dim">
                {format!(
                    "'{}' is removed from the catalog. Its chat threads are kept and stay on the \
                     Chat page — a conversation is yours, the agent is only the preset it was had \
                     through.",
                    id.get_value(),
                )}
            </p>
            {move || resettable()
                .then(|| {
                    view! {
                        <p class="dim mini-note">
                            "This is one of the shipped agents. It stays deleted across restarts; "
                            "\"Restore shipped agents\" in the list's add menu is the way back."
                        </p>
                    }
                })}
            <ModalFooter>
                <button class="btn" on:click=move |_| acts.del_open.set(false)>
                    "Cancel"
                </button>
                <button class="btn destructive" on:click=move |_| delete()>
                    "Delete"
                </button>
            </ModalFooter>
        </Modal>
    }
}

#[component]
fn DuplicateModal(open: RwSignal<bool>, id: String) -> impl IntoView {
    let toasts = use_toasts();
    let page = use_context::<AgentsCtx>();
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
            let res = crate::api::post::<AgentDuplicated, _>(
                "/api/op/agent_duplicate",
                &json!({ "id": source.get_value(), "new_id": target }),
            )
            .await;
            busy.set(false);
            match res {
                Ok(v) => {
                    open.set(false);
                    toasts.ok(v.message);
                    if let Some(p) = page {
                        p.reload.run(());
                        p.go(&format!("/agents/{target}"), false);
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
                <button class="btn" on:click=move |_| open.set(false)>
                    "Cancel"
                </button>
                <button class="btn suggested" disabled=move || busy.get() on:click=create>
                    "Duplicate"
                </button>
            </ModalFooter>
        </Modal>
    }
}
