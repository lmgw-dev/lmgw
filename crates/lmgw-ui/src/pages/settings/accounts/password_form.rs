//! Setting a password on the Accounts card (headless server design §5.2,
//! ruling 21): one's own with the current one, account 1's sign-in name with
//! its first password, or another account's.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::AccountDone;
use serde_json::json;

use crate::widgets::use_toasts;

/// Set a password here: one's own (with the current one when there is
/// one, and account 1's sign-in name with its first password), or another
/// account's, which ends its sessions (ruling 21).
#[component]
pub(super) fn PasswordForm(
    id: i64,
    you: bool,
    has: bool,
    first: bool,
    refresh: Callback<()>,
) -> impl IntoView {
    let toasts = use_toasts();
    let current = RwSignal::new(String::new());
    let login = RwSignal::new(String::new());
    let a = RwSignal::new(String::new());
    let b = RwSignal::new(String::new());
    let save = move || {
        let (pa, pb) = (a.get_untracked(), b.get_untracked());
        if pa != pb {
            toasts.err("the two passwords differ".to_string());
            return;
        }
        let mut args = json!({ "id": id, "password": pa });
        if you && has {
            args["current"] = json!(current.get_untracked());
        }
        let l = login.get_untracked();
        if first && !l.trim().is_empty() {
            args["login"] = json!(l.trim());
        }
        spawn_local(async move {
            match super::op::<AccountDone>("account_password_set", args).await {
                Ok(d) => {
                    toasts.ok(d.message);
                    a.set(String::new());
                    b.set(String::new());
                    refresh.run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let input = |sig: RwSignal<String>, ph: &'static str, kind: &'static str| {
        view! {
            <input
                class="input"
                type=kind
                placeholder=ph
                prop:value=move || sig.get()
                on:input=move |ev| sig.set(event_target_value(&ev))
            />
        }
    };
    view! {
        <div class="row" style="gap:8px;align-items:end;flex-wrap:wrap">
            {(you && has).then(|| input(current, "current password", "password"))}
            {first.then(|| input(login, "new sign-in name (optional)", "text"))}
            {input(a, "new password (at least 12 characters)", "password")}
            {input(b, "again", "password")}
            <button class="btn sm primary" on:click=move |_| save()>"Save password"</button>
        </div>
        <p class="dim">
            {if you {
                "Your other sessions end; this browser stays signed in."
            } else {
                "That account's sessions end."
            }}
        </p>
    }
}
