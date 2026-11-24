//! Rotating an owner key. Rotation has no grace window and the dashboard's
//! own key takes every browser with it (principals §3.12), so it is never
//! one click: a question that says what this particular key's rotation
//! costs, then the new plaintext on screen before anything is attempted
//! with it.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{KeyRow, KeySecret};
use serde_json::json;

use crate::widgets::{use_toasts, CopyBtn, Modal, ModalFooter};

/// The question's opener and the dialogs, for the page to place.
pub(super) fn rotate_dialogs(refresh: RwSignal<u32>) -> (Callback<KeyRow>, impl IntoView) {
    let toasts = use_toasts();
    // The owner row waiting on the rotate confirm.
    let rotating = RwSignal::new(None::<KeyRow>);
    let rotate_open = RwSignal::new(false);
    // The row name and the plaintext a rotate just returned. That response is
    // the only place the key is legible without a session, and rotating
    // `owner:dashboard` is quite capable of taking this tab's — so it goes on
    // screen for every owner row, before anything is attempted with it.
    let rotated = RwSignal::new(None::<(String, String)>);
    let rotated_open = RwSignal::new(false);
    // The re-login after a dashboard rotate did not land: this tab is holding a
    // cookie minted from a key that no longer exists.
    let stranded = RwSignal::new(false);
    let rotate = move |_| {
        let Some(k) = rotating.get_untracked() else {
            return;
        };
        rotate_open.set(false);
        spawn_local(async move {
            let res =
                crate::api::post::<KeySecret, _>("/api/op/key_rotate", &json!({ "id": k.id }))
                    .await;
            match res {
                Ok(v) => {
                    let (key, msg) = (v.key, v.message);
                    stranded.set(false);
                    if !key.is_empty() {
                        rotated.set(Some((k.name.clone(), key.clone())));
                        rotated_open.set(true);
                    }
                    // The cookie this tab is holding was minted from the key
                    // that just stopped existing. Present the new one now, or
                    // the very next call on this page lands on the login card.
                    if k.name == "owner:dashboard" && !key.is_empty() {
                        if let Err(e) = crate::session::sign_in(&key).await {
                            stranded.set(true);
                            // Two ways back, because the modal is only as
                            // durable as the page under it: the key is on
                            // screen, and the login card is holding it too.
                            crate::session::carry_key(&key);
                            toasts.err(format!("key rotated, but this tab did not re-log-in: {e}"));
                        }
                    }
                    toasts.ok(msg);
                    // A refetch on a dead cookie answers 401, which raises the
                    // gate and takes this modal — and the readable copy of the
                    // key — down with it. Signing back in re-runs the page
                    // anyway. (And a page that is gone has nothing to refetch.)
                    if stranded.try_get_untracked() == Some(false) {
                        refresh.update(|v| *v = v.wrapping_add(1));
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let ask = Callback::new(move |r: KeyRow| {
        rotating.set(Some(r));
        rotate_open.set(true);
    });

    let dialogs = view! {
        // What rotating this particular key costs, before the click: the
        // dashboard key is every browser's session, the others are
        // whatever config presents them.
        <Modal open=rotate_open title="Rotate this owner key">
            <p class="dim">
                "The key is replaced in one write — there is no grace window. Anything "
                "holding the old one is locked out at once."
            </p>
            {move || {
                let dashboard = rotating.get().is_some_and(|k| k.name == "owner:dashboard");
                if dashboard {
                    view! {
                        <p class="notice warn">
                            "This is the key every browser's session is minted from. Every "
                            "other open tab lands on the login card; this one is signed back "
                            "in with the new key and carries on. The key is shown to you "
                            "either way, before that is attempted."
                        </p>
                    }
                    .into_any()
                } else {
                    view! {
                        <p class="notice warn">
                            "Whatever presents this key — an MCP client config, a curl in a "
                            "note — keeps sending the old one until you update it. The new "
                            "key is shown as soon as it exists, and stays copyable from its "
                            "page."
                        </p>
                    }
                    .into_any()
                }
            }}
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| rotate_open.set(false)>
                    "Cancel"
                </button>
                <button class="btn danger" on:click=rotate>
                    "Rotate the key"
                </button>
            </ModalFooter>
        </Modal>

        // The result: the plaintext itself, shown before it is used for
        // anything. `key_reveal` can read it back later — but only for a
        // browser that still has a session, which is exactly what rotating
        // the dashboard key takes away.
        <Modal open=rotated_open title="The new owner key">
            {move || {
                let Some((name, key)) = rotated.get() else { return ().into_any() };
                let copy_text = key.clone();
                let carried = key.clone();
                let stranded_note = move || {
                    if !stranded.get() {
                        return ().into_any();
                    }
                    let k = carried.clone();
                    view! {
                        <div class="notice err" style="margin-top:10px">
                            "sign back in with this key: the tab's session was rotated away"
                        </div>
                        <div class="row" style="justify-content:flex-end; margin-top:8px">
                            <button
                                class="btn primary"
                                title="opens the login card with this key already in the box"
                                on:click=move |_| crate::session::lock_with_key(&k)
                            >
                                "Open the login card"
                            </button>
                        </div>
                    }
                    .into_any()
                };
                view! {
                    <p class="dim">
                        "The new plaintext for "
                        <span class="mono-sm">{name}</span>
                        ". It is stored, so "
                        <b>"Copy the key"</b>
                        " on its page gets it back — for as long as this browser has a session."
                    </p>
                    <div class="wiz-ok" style="margin-top:10px">
                        <span class="mono-sm">{key}</span>
                        " "
                        <CopyBtn text=copy_text title="Copy the new key"/>
                    </div>
                    {stranded_note}
                }
                .into_any()
            }}
            <ModalFooter>
                <button class="btn primary" on:click=move |_| rotated_open.set(false)>
                    "Done"
                </button>
            </ModalFooter>
        </Modal>
    };
    (ask, dialogs)
}
