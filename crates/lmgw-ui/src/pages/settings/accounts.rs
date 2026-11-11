//! Settings → Network & access → Accounts (headless server design §5.6,
//! §5.11): on a desktop with accounts off, the switch and a line of what is
//! kept; with accounts on, the no-password banner, the accounts with their
//! sessions, adding one, its set-password link, and on the desktop the
//! switch back off; below them the audit log's list (§5.8). Every change
//! here goes through the account ops; only a session's lifetime and the
//! audit log's retention are rows of the draft.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{AccountDone, AccountLink, AccountList, AccountView, AccountsStatus};
use serde_json::json;

use super::{f, labels, Ctl, Def, Page};
use crate::widgets::{use_toasts, MenuItem, RowMenu};

/// The audit log's list.
mod audit;
/// A minted link on screen.
mod link_shown;
/// Setting a password here.
mod password_form;
/// An account's sessions.
mod sessions;

pub(super) use audit::AuditPanel;
use link_shown::LinkShown;
use password_form::PasswordForm;
use sessions::Sessions;

/// The group's rows, in page order.
pub(super) const ACCOUNTS: &[Def] = &[
    f(
        "network",
        "accounts",
        "accounts.status",
        labels::SIGN_IN_ACCOUNTS,
        Ctl::Accounts,
    )
    .terms("accounts users sign in login password session admin people switch"),
    f(
        "network",
        "accounts",
        "accounts.session_days",
        labels::SESSION_LIFETIME,
        Ctl::Int(1),
    )
    .s()
    .unit("days")
    .hint(
        "how long a browser stays signed in, and how long it counts as a known browser of its \
         account; applies to sign-ins from now on. 1 to 3650",
    )
    .terms("session lifetime days expiry sign in remember"),
    f(
        "network",
        "accounts",
        "accounts.audit_days",
        labels::AUDIT_KEEP,
        Ctl::Int(0),
    )
    .s()
    .unit("days")
    .hint(
        "how long the audit log keeps an entry; 0 keeps every entry. A change is written to the \
         log before it removes anything. 0 to 3650",
    )
    .terms("audit log retention keep days prune history"),
    f(
        "network",
        "accounts",
        "accounts.audit",
        labels::AUDIT_LOG,
        Ctl::AuditLog,
    )
    .terms("audit log history who did what activity changes sign in"),
];

/// The status the settings answer carries.
fn status(page: Page) -> Option<AccountsStatus> {
    page.data.with(|d| {
        d.as_ref()
            .and_then(|v| serde_json::from_value(v["accounts_status"].clone()).ok())
    })
}

/// An RFC 3339 instant in the browser's local time.
fn when(t: &str) -> String {
    let ms = js_sys::Date::new(&leptos::wasm_bindgen::JsValue::from_str(t)).get_time();
    if ms.is_nan() {
        t.to_string()
    } else {
        crate::fmt::local_datetime(ms / 1000.0)
    }
}

/// What turning accounts on says before it does it (§5.11).
pub(super) const TURN_ON: &str = "Everyone who signs in to this dashboard will need an account. \
    Other browsers signed in now sign in again; this one stays signed in.";

/// What turning accounts off says, with the numbers (§5.11).
pub(super) fn turn_off(s: &AccountsStatus) -> String {
    format!(
        "Turning accounts off signs every browser out and pauses the keys of {} other \
         account(s) ({} key(s)). The {} account(s), their passwords and the audit log (its \
         size is under Audit log, below) are kept and come back when you turn accounts on \
         again.",
        s.other_accounts, s.other_keys, s.accounts,
    )
}

/// The line of what is kept while accounts are off (§5.11).
pub(super) fn kept_line(s: &AccountsStatus) -> Option<String> {
    (s.accounts > 1 || s.known_browsers > 0 || s.links > 0 || s.audit_kept).then(|| {
        let log = if s.audit_kept {
            ", the audit log (its size is under Audit log, below)"
        } else {
            ""
        };
        format!(
            "Kept from when accounts were on: {} account(s) with their passwords, {} known \
             browser(s), {} set-password link(s){log}; the keys of the {} other account(s) \
             are paused.",
            s.accounts, s.known_browsers, s.links, s.other_accounts
        )
    })
}

/// The counts could not be read: said, so a zero is not taken as fact.
fn counts_error(s: &AccountsStatus) -> Option<impl IntoView> {
    s.counts_error.clone().map(|e| {
        view! { <div class="notice err">{format!("The numbers on this card {e}")}</div> }
    })
}

async fn op<T: serde::de::DeserializeOwned>(
    name: &str,
    args: serde_json::Value,
) -> crate::api::Result<T> {
    crate::api::post::<T, _>(format!("/api/op/{name}"), &args).await
}

#[component]
pub(super) fn AccountsPanel(page: Page, id: String, hidden: Signal<bool>) -> impl IntoView {
    let toasts = use_toasts();
    let confirming = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let switch = move |enabled: bool| {
        busy.set(true);
        spawn_local(async move {
            let res = op::<AccountDone>("accounts_switch", json!({ "enabled": enabled })).await;
            busy.set(false);
            confirming.set(false);
            match res {
                Ok(d) => {
                    // The cookie changed with the answer: the page re-reads
                    // who it is, and says what the switch did once it has.
                    toasts.ok_after_reload(d.message);
                    let _ = window().location().reload();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        <div class="set-about" id=id hidden=move || hidden.get()>
            {move || {
                let Some(s) = status(page) else {
                    return view! { <p class="dim">"…"</p> }.into_any();
                };
                let transport = (!s.secure_transport).then(|| {
                    view! {
                        <p class="dim">
                            "Passwords from other machines need the TLS listener (above) or a TLS \
                             proxy in front (proxy mode); from this machine they work as they are."
                        </p>
                    }
                });
                let unread = counts_error(&s);
                if !s.on {
                    let kept = kept_line(&s);
                    return view! {
                        {unread}
                        <p>
                            "Accounts are off: this dashboard has one owner, signed in through \
                             the lmgw window or the login link in the log."
                        </p>
                        {kept.map(|k| view! { <p class="dim">{k}</p> })}
                        {transport}
                        <SwitchRow
                            label="Turn accounts on…"
                            sentence=TURN_ON.to_string()
                            confirming=confirming
                            busy=busy
                            go=Callback::new(move |()| switch(true))
                        />
                    }
                    .into_any();
                }
                let banner = s.door_without_password.then(|| {
                    view! {
                        <div class="notice warn">
                            "account 'owner' has no password: set one before signing in from \
                             another browser"
                        </div>
                    }
                });
                let off = (!s.container).then(|| {
                    view! {
                        <SwitchRow
                            label="Turn accounts off…"
                            sentence=turn_off(&s)
                            confirming=confirming
                            busy=busy
                            go=Callback::new(move |()| switch(false))
                        />
                    }
                });
                let always = s.container.then(|| {
                    view! { <p class="dim">"Always on in the server image."</p> }
                });
                view! {
                    {unread}
                    {banner}
                    <p class="dim">
                        "Everyone with an account here administers this gateway and sees its \
                         chats and usage."
                    </p>
                    {transport}
                    <AccountsCard page=page/>
                    {always}
                    {off}
                }
                .into_any()
            }}
        </div>
    }
}

/// A switch button behind its sentence: the first click shows what follows,
/// the second does it.
#[component]
fn SwitchRow(
    label: &'static str,
    sentence: String,
    confirming: RwSignal<bool>,
    busy: RwSignal<bool>,
    go: Callback<()>,
) -> impl IntoView {
    let sentence = StoredValue::new(sentence);
    view! {
        <div class="set-line" style="margin-top:12px">
            <Show
                when=move || confirming.get()
                fallback=move || {
                    view! {
                        <button class="btn" on:click=move |_| confirming.set(true)>
                            {label}
                        </button>
                    }
                }
            >
                <div class="notice warn">{sentence.get_value()}</div>
                <div class="row" style="gap:8px;margin-top:8px">
                    <button
                        class="btn primary"
                        disabled=move || busy.get()
                        on:click=move |_| go.run(())
                    >
                        "Confirm"
                    </button>
                    <button class="btn ghost" on:click=move |_| confirming.set(false)>
                        "Cancel"
                    </button>
                </div>
            </Show>
        </div>
    }
}

/// The accounts, read with `account_list`, and adding one.
#[component]
fn AccountsCard(page: Page) -> impl IntoView {
    let toasts = use_toasts();
    let tick = RwSignal::new(0u32);
    let list = LocalResource::new(move || {
        tick.get();
        async move { op::<AccountList>("account_list", json!({})).await }
    });
    let refresh = Callback::new(move |()| {
        tick.update(|t| *t = t.wrapping_add(1));
        page.settings_written.run(());
    });
    let link = RwSignal::new(None::<AccountLink>);
    let login = RwSignal::new(String::new());
    let display = RwSignal::new(String::new());
    let add = move || {
        let (l, d) = (login.get_untracked(), display.get_untracked());
        spawn_local(async move {
            match op::<AccountLink>("account_create", json!({ "login": l, "display_name": d }))
                .await
            {
                Ok(v) => {
                    login.set(String::new());
                    display.set(String::new());
                    link.set(Some(v));
                    refresh.run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        <Suspense fallback=|| view! { <p class="dim">"reading the accounts…"</p> }>
            {move || {
                list.get()
                    .map(|r| match r {
                        Err(e) => view! { <div class="notice err">{e.to_string()}</div> }.into_any(),
                        Ok(l) => {
                            let limits = format!(
                                "Passwords: at least {} characters, up to {}. After a failed \
                                 sign-in the next waits {} s, doubling up to {} min; {} \
                                 sign-ins are checked at once ({} of them kept for browsers \
                                 that signed in here before, {} of memory together); {} \
                                 entries in the wait list now. Set-password links last {} h, \
                                 one-time sign-in links {} s.",
                                l.limits.password_min_chars,
                                lmgw_api_types::units::fmt_bytes(l.limits.body_limit_bytes),
                                l.limits.backoff_start_s,
                                l.limits.backoff_max_s / 60,
                                l.limits.verifications_at_once,
                                l.limits.kept_for_known_browsers,
                                lmgw_api_types::units::fmt_bytes(l.limits.verification_memory_bytes),
                                l.limits.throttle_entries,
                                l.limits.link_ttl_s / 3600,
                                l.limits.login_link_ttl_s,
                            );
                            view! {
                                <table class="data">
                                    <thead>
                                        <tr>
                                            <th class="nowrap">"Sign-in name"</th>
                                            <th>"Name"</th>
                                            <th>"Password"</th>
                                            <th class="nowrap">"Last sign-in"</th>
                                            <th class="num">"Failed"</th>
                                            <th class="num">"Keys"</th>
                                            <th class="num">"Sessions"</th>
                                            <th></th>
                                        </tr>
                                    </thead>
                                    <tbody>
                                        {l.accounts
                                            .into_iter()
                                            .map(|a| view! { <AccountRow a=a refresh=refresh/> })
                                            .collect_view()}
                                    </tbody>
                                </table>
                                <p class="dim">{limits}</p>
                            }
                            .into_any()
                        }
                    })
            }}
        </Suspense>
        <div class="row" style="gap:8px;margin-top:8px;align-items:end">
            <input
                class="input"
                placeholder="sign-in name"
                autocapitalize="none"
                prop:value=move || login.get()
                on:input=move |ev| login.set(event_target_value(&ev))
            />
            <input
                class="input"
                placeholder="display name"
                prop:value=move || display.get()
                on:input=move |ev| display.set(event_target_value(&ev))
            />
            <button class="btn" on:click=move |_| add()>"Add account"</button>
        </div>
        <LinkShown link=link/>
    }
}

/// One account's row, its actions, and its sessions when opened.
#[component]
fn AccountRow(a: AccountView, refresh: Callback<()>) -> impl IntoView {
    let toasts = use_toasts();
    let id = a.id;
    let mine = a.you;
    let open = RwSignal::new(false);
    let pw_open = RwSignal::new(false);
    let link = RwSignal::new(None::<AccountLink>);
    let done = move |res: crate::api::Result<AccountDone>| match res {
        Ok(d) => {
            toasts.ok(d.message);
            refresh.run(());
        }
        Err(e) => toasts.err(e.to_string()),
    };
    let set_enabled = move |on: bool| {
        spawn_local(
            async move { done(op("account_set", json!({ "id": id, "enabled": on })).await) },
        )
    };
    let delete =
        move || spawn_local(async move { done(op("account_delete", json!({ "id": id })).await) });
    let mint = move || {
        spawn_local(async move {
            match op::<AccountLink>("account_password_link", json!({ "id": id })).await {
                Ok(l) => link.set(Some(l)),
                Err(e) => toasts.err(e.to_string()),
            }
        })
    };
    let everywhere = move || {
        spawn_local(async move {
            let res = op::<AccountDone>("session_end", json!({ "account": id })).await;
            match res {
                Ok(_) if mine => {
                    let _ = crate::session::sign_out().await;
                }
                other => done(other),
            }
        })
    };
    let last = a
        .last_login_at
        .as_deref()
        .map_or_else(|| "never".to_string(), when);
    let you = a.you;
    let door = a.door;
    let first = door && !a.has_password;
    let on = a.enabled;
    let mut items = vec![
        MenuItem::new("Set password…", move || pw_open.update(|o| *o = !*o)).title(if you {
            "change your password; your other sessions end"
        } else {
            "set this account's password; its sessions end"
        }),
    ];
    if !you {
        items.push(
            MenuItem::new("Set-password link", mint)
                .title("a one-time link to hand over, valid 24 h; replaces an earlier one"),
        );
    }
    items.push(
        MenuItem::new("Sessions", move || open.update(|o| *o = !*o))
            .title("where this account is signed in, each with End"),
    );
    items.push(
        MenuItem::new("Sign out everywhere", everywhere)
            .danger()
            .title("end every session of this account"),
    );
    if !you && !door {
        items.push(
            MenuItem::new(if on { "Disable" } else { "Enable" }, move || set_enabled(!on))
                .title(if on {
                    "its sessions end and its owner keys authenticate nothing; the keys it made keep working"
                } else {
                    "it may sign in again"
                }),
        );
        if on {
            items.last_mut().unwrap().danger = true;
        }
        items.push(MenuItem::new("Delete", delete).danger().title(
            "deletes the account with its owner keys and sessions; the keys it made keep working",
        ));
    }
    view! {
        <tr>
            <td class="mono-sm nowrap">
                {a.login.clone()}
                {you.then(|| view! { " " <span class="chip">"you"</span> })}
                {(!a.enabled).then(|| view! { " " <span class="chip off">"disabled"</span> })}
            </td>
            <td class="clip">{a.display_name.clone()}</td>
            <td class="nowrap">{if a.has_password { "set" } else { "none yet" }}</td>
            <td class="dim nowrap">{last}</td>
            <td class="num" title="failed sign-ins since the last good one">{a.failed_logins}</td>
            <td class="num" title="its owner keys, its sign-in key included">{a.keys}</td>
            <td class="num">
                <button class="btn ghost sm" title="its sessions" on:click=move |_| open.update(|o| *o = !*o)>
                    {a.sessions}
                </button>
            </td>
            <td class="actions">
                <div class="row-acts">
                    <RowMenu items=Signal::stored(items) title="Account actions"/>
                </div>
            </td>
        </tr>
        <Show when=move || pw_open.get()>
            <tr>
                <td colspan="8">
                    <PasswordForm id=id you=you has=a.has_password first=first refresh=refresh/>
                </td>
            </tr>
        </Show>
        <Show when=move || link.get().is_some()>
            <tr>
                <td colspan="8"><LinkShown link=link/></td>
            </tr>
        </Show>
        <Show when=move || open.get()>
            <tr>
                <td colspan="8"><Sessions id=id refresh=refresh/></td>
            </tr>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_names_its_numbers() {
        let s = AccountsStatus {
            accounts: 3,
            other_accounts: 2,
            other_keys: 4,
            known_browsers: 1,
            ..Default::default()
        };
        let off = turn_off(&s);
        assert!(
            off.contains("2 other account(s)") && off.contains("The 3 account(s)"),
            "{off}"
        );
        assert!(kept_line(&s).unwrap().contains("1 known browser(s)"));
        assert_eq!(
            kept_line(&AccountsStatus {
                accounts: 1,
                ..Default::default()
            }),
            None
        );
    }
}
