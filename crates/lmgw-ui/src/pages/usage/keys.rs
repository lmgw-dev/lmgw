//! Keys & budgets (`/usage/keys/:id?`): every credential and internal
//! identity with its policy and spend, and the paired devices — the one
//! place a key is created, edited, rotated or revoked (Settings → Network &
//! access links here). A list beside the selected key on the shared split
//! view ([`crate::widgets::nav_split`]); the selection is the path, so
//! `/usage/keys/<id>` selects its row.
//!
//! The list ([`list`]) has a row per key under its kind's heading. With
//! nothing selected the detail is the status page ([`status`]): the global
//! budget, the form that creates a key, pairing a device, the link strip
//! and the transfers between devices. A selected key's detail ([`detail`])
//! is its hero, its spend, its policy as preference rows with the save
//! toast and Ctrl+S, a device's connection and hosted tools, and what ends
//! it (rotate, revoke, disable, delete) as rows of their own.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;
use lmgw_api_types::{KeyRow, KeysResponse, MessageAck, PricesResponse};
use serde_json::json;

use crate::live::use_live;
use crate::widgets::list_pane::{query_keeping, ListRefs, Nav};
use crate::widgets::nav_split::{provide_save_key, NavSplit, SplitConf, SplitState};
use crate::widgets::{use_toasts, Modal, ModalFooter};

use super::devices::{device_acts, DeviceActs};
use super::*;

/// A key's detail: hero, spend, policy, device, what ends it.
mod detail;
/// The list pane: the header, the views, the search, the rows by kind.
mod list;
/// Rotating an owner key: the question, and the new key shown before it is
/// used for anything.
mod rotate;
/// Nothing selected: the global budget, a new key, pairing.
mod status;

/// Below this pane width the view is one pane at a time (a 1024 px window at
/// 125 % leaves the pane about 770 px), as Models'.
const NARROW_BELOW: f64 = 800.0;

const CONF: SplitConf = SplitConf {
    class: "keys-split",
    width_key: "keys.list.width",
    list_width: (260, 32, 380),
    detail_min: 440,
    narrow_below: NARROW_BELOW,
    handle_label: "Resize the list",
};

/// The plaintext a create just returned, for its key's detail. It lives at
/// the page, not in the list: the list refetches (after the create itself,
/// and as spend lands), and for a client key this is the only time it is
/// ever legible (principals §3.12).
#[derive(Clone, PartialEq)]
pub(super) struct Fresh {
    pub id: i64,
    pub name: String,
    pub key: String,
}

/// What the list, the status page and the detail share.
#[derive(Clone, Copy)]
pub(super) struct KeysCtx {
    pub keys: Src<KeysResponse>,
    /// The Prices view's count, for the views' switch.
    pub prices: Src<PricesResponse>,
    pub refresh: RwSignal<u32>,
    pub fresh: RwSignal<Option<Fresh>>,
    /// Ask before rotating an owner key.
    pub rotate: Callback<KeyRow>,
    /// Ask before revoking a key.
    pub revoke: Callback<KeyRow>,
    pub devices: DeviceActs,
    /// Bumped to put the focus into the new key's name on the status page.
    pub create_focus: RwSignal<u32>,
    nav: Nav,
}

impl KeysCtx {
    /// Read `/api/usage/keys` again.
    pub fn reload(self) {
        self.refresh.update(|v| *v = v.wrapping_add(1));
    }

    /// Go to `to`, unless unsaved edits it would lose put the question up
    /// first; `false` then.
    pub fn go(self, to: &str, replace: bool) -> bool {
        self.nav.go(to, replace)
    }

    /// Go without asking: what would be asked about is gone already.
    pub fn go_unasked(self, to: &str, replace: bool) {
        self.nav.go_unasked(to, replace);
    }

    /// The status page with its new-key form, the focus in its name.
    pub fn new_key(self) {
        let q = list_query(&window().location().search().unwrap_or_default());
        if self.go(&format!("/usage/keys{q}"), false) {
            self.create_focus.update(|v| *v = v.wrapping_add(1));
        }
    }
}

/// The list's own query keys, carried from one selection to the next.
const LIST_KEYS: &[&str] = &["q"];

/// The list's own query (`q`) out of a search string.
pub fn list_query(search: &str) -> String {
    query_keeping(search, LIST_KEYS)
}

#[component]
pub fn UsageKeys() -> impl IntoView {
    let toasts = use_toasts();
    let live = use_live();
    let params = use_params_map();
    let sel = Memo::new(move |_| {
        params
            .with(|p| p.get("id"))
            .and_then(|s| s.trim().parse::<i64>().ok())
    });
    let has_sel = Memo::new(move |_| sel.with(Option::is_some));

    let refresh = RwSignal::new(0u32);
    let keys = src(LocalResource::new(move || {
        refresh.track();
        crate::api::get::<KeysResponse>("/api/usage/keys")
    }));
    // The Prices view's count, once.
    let prices = src(LocalResource::new(|| {
        crate::api::get::<PricesResponse>("/api/usage/prices")
    }));

    // Nothing here polls. Spend and last-used move as requests land: one
    // refetch at most 15 s after the first that landed since the last one.
    // A device's online state and a revocation move without a request: the
    // `keys` frame of `/api/events` says so, and the list is refetched at
    // once (client-apps design §1.6).
    let pending = RwSignal::new(None::<TimeoutHandle>);
    let refetch = move || refresh.update(|v| *v = v.wrapping_add(1));
    Effect::new(move |_| {
        if live.request.get().is_none() || pending.get_untracked().is_some() {
            return;
        }
        let later = set_timeout_with_handle(
            move || {
                pending.set(None);
                refetch();
            },
            Duration::from_secs(15),
        );
        pending.set(later.ok());
    });
    // A `transfer` frame is no change of a key: the status page's Transfers
    // read it (device transfers design §11.1).
    Effect::new(move |_| {
        if live
            .keys
            .with(|k| k.as_ref().is_some_and(|k| k.what != "transfer"))
        {
            refetch();
        }
    });
    on_cleanup(move || {
        if let Some(h) = pending.get_untracked() {
            h.clear();
        }
    });

    let (rotate, rotate_view) = rotate::rotate_dialogs(refresh);
    let (devices, devices_view) = device_acts(keys, refresh);

    // Revoke: one question, then done (a key is not a draft).
    let revoking = RwSignal::new(None::<KeyRow>);
    let revoke_open = RwSignal::new(false);
    let revoke = Callback::new(move |r: KeyRow| {
        revoking.set(Some(r));
        revoke_open.set(true);
    });
    let page = KeysCtx {
        keys,
        prices,
        refresh,
        fresh: RwSignal::new(None),
        rotate,
        revoke,
        devices,
        create_focus: RwSignal::new(0),
        nav: Nav::new(),
    };
    provide_context(page);
    let revoke_busy = RwSignal::new(false);
    let confirm_revoke = move |_| {
        let Some(r) = revoking.get_untracked() else {
            return;
        };
        revoke_busy.set(true);
        spawn_local(async move {
            let res =
                crate::api::post::<MessageAck, _>("/api/op/key_delete", &json!({ "id": r.id }))
                    .await;
            revoke_busy.try_set(false);
            revoke_open.try_set(false);
            match res {
                Ok(_) => {
                    toasts.ok(format!("{} revoked", r.name));
                    if sel.try_get_untracked().flatten() == Some(r.id) {
                        // Its unsaved edits went with it.
                        let q = list_query(&window().location().search().unwrap_or_default());
                        page.go_unasked(&format!("/usage/keys{q}"), true);
                    }
                    if refresh.try_get_untracked().is_some() {
                        page.reload();
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let split = SplitState::new(CONF);
    let narrow = split.narrow;
    let save_key = provide_save_key();
    let refs = ListRefs::new();
    let open_row = Callback::new(move |(id, replace): (i64, bool)| {
        if sel.get_untracked() != Some(id) {
            page.nav
                .open(&format!("/usage/keys/{id}"), LIST_KEYS, replace);
        }
    });
    let back = Callback::new(move |()| page.nav.back("/usage/keys", LIST_KEYS, refs));

    view! {
        <NavSplit
            split=split
            has_sel=has_sel
            save_key=save_key
            search=refs.search
            search_off=Signal::derive(move || narrow.get() && has_sel.get())
            list=ViewFn::from(move || {
                view! { <list::ListPane page=page sel=sel narrow=narrow refs=refs open_row=open_row/> }
            })
            detail=ViewFn::from(move || {
                view! {
                    <Show
                        when=move || has_sel.get()
                        fallback=move || view! { <status::StatusPage page=page narrow=narrow back=back/> }
                    >
                        <detail::KeyDetail page=page sel=sel narrow=narrow back=back/>
                    </Show>
                }
            })
        />
        {rotate_view}
        {devices_view}
        <Modal open=revoke_open title="Revoke this key">
            {move || {
                revoking
                    .get()
                    .map(|r| {
                        view! {
                            <p><b>{format!("Revoke {}?", r.name)}</b></p>
                            <p class="notice warn">
                                "The key is deleted: anything presenting it is refused from now on. "
                                "Its spend stays in Usage."
                            </p>
                        }
                    })
            }}
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| revoke_open.set(false)>
                    "Cancel"
                </button>
                <button class="btn danger" disabled=move || revoke_busy.get() on:click=confirm_revoke>
                    "Revoke the key"
                </button>
            </ModalFooter>
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lists_search_is_carried_and_nothing_else() {
        assert_eq!(list_query("?q=ci&x=1"), "?q=ci");
        assert_eq!(list_query(""), "");
        assert_eq!(list_query("?x=1"), "");
    }
}
