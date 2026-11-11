//! The sign-in cards accounts add (headless server design §5.2, §5.5): a
//! sign-in name and a password, with the key card under a fold, and the
//! set-password link's page.

use leptos::prelude::*;
use leptos::task::spawn_local;

/// The shortest password the gateway takes (its `PASSWORD_MIN_CHARS`, said
/// on the form as the refusal says it).
pub const PASSWORD_MIN_CHARS: usize = 12;

/// The sign-in card while accounts are on: name and password, and the key
/// card under a fold; `revoked` is why this browser was signed out, when
/// its event stream said so.
#[component]
pub fn PasswordCard<K, KV>(key_card: K, revoked: Signal<Option<String>>) -> impl IntoView
where
    K: Fn() -> KV + Send + Sync + 'static,
    KV: IntoView + 'static,
{
    let login = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let submit = move || {
        if busy.get_untracked() {
            return;
        }
        let (l, p) = (login.get_untracked(), password.get_untracked());
        if l.trim().is_empty() || p.is_empty() {
            error.set(Some("enter your sign-in name and password".into()));
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let res = crate::session::sign_in_password(l.trim(), &p).await;
            busy.set(false);
            match res {
                Ok(()) => password.set(String::new()),
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
    let enter = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Enter" {
            submit()
        }
    };
    let container = crate::session::container_signal();
    let fold_text = move || {
        if container.get() {
            view! {
                <code>{LOGIN_LINK_COMMAND}</code>
                " prints a one-time sign-in link on the server. An owner key pasted here signs \
                 this browser into its account's session; the key itself stays out of the \
                 browser."
            }
            .into_any()
        } else {
            view! {
                "The lmgw window signs in by itself. An owner key pasted here signs this browser \
                 into its account's session; the key itself stays out of the browser."
            }
            .into_any()
        }
    };
    view! {
        <div class="login-gate density-dense">
            <div class="card login-card">
                <h3>"Sign in to this gateway"</h3>
                <p class="dim">
                    "Everyone with an account here administers this gateway."
                </p>
                {move || {
                    revoked
                        .get()
                        .map(|m| view! { <div class="notice warn" style="margin-top:10px">{m}</div> })
                }}
                <label class="field-label" style="margin-top:12px">"Sign-in name"</label>
                <input
                    class="input"
                    type="text"
                    style="width:100%"
                    autocomplete="username"
                    autocapitalize="none"
                    spellcheck="false"
                    prop:value=move || login.get()
                    on:input=move |ev| login.set(event_target_value(&ev))
                    on:keydown=enter
                />
                <label class="field-label" style="margin-top:8px">"Password"</label>
                <input
                    class="input"
                    type="password"
                    style="width:100%"
                    autocomplete="current-password"
                    prop:value=move || password.get()
                    on:input=move |ev| password.set(event_target_value(&ev))
                    on:keydown=enter
                />
                {move || {
                    error
                        .get()
                        .map(|e| view! { <div class="notice err" style="margin-top:10px">{e}</div> })
                }}
                <div class="row" style="justify-content:flex-end; margin-top:12px">
                    <button class="btn primary" disabled=move || busy.get() on:click=move |_| submit()>
                        {move || if busy.get() { "Signing in…" } else { "Sign in" }}
                    </button>
                </div>
                <details class="login-fold" style="margin-top:12px">
                    <summary class="dim">"Sign in with a key or a one-time link instead"</summary>
                    <p class="dim">{fold_text}</p>
                    {key_card()}
                </details>
            </div>
        </div>
    }
}

/// The command a server's log names for a one-time sign-in link (the
/// gateway's `LOGIN_LINK_COMMAND`).
pub const LOGIN_LINK_COMMAND: &str = "podman exec -it lmgw lmgw-server login-link";

/// What a `/set-password` link without its `#token=…` says, up front.
pub const LINK_INCOMPLETE: &str = "This link is incomplete: it carries no token. Copy the whole \
    link, which ends in #token= and a long code, or ask for a new one.";

/// The set-password link's page (§5.5): the new password twice, and for
/// account 1's first password its sign-in name. A link without its token
/// says so before anything is typed.
#[component]
pub fn SetPasswordCard(token: String) -> impl IntoView {
    if token.trim().is_empty() {
        return view! {
            <div class="login-gate density-dense">
                <div class="card login-card">
                    <h3>"Set your password"</h3>
                    <div class="notice err" style="margin-top:10px">{LINK_INCOMPLETE}</div>
                </div>
            </div>
        }
        .into_any();
    }
    let token = StoredValue::new(token);
    let login = RwSignal::new(String::new());
    let first = RwSignal::new(String::new());
    let second = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let submit = move || {
        if busy.get_untracked() {
            return;
        }
        let (a, b) = (first.get_untracked(), second.get_untracked());
        if a != b {
            error.set(Some("the two passwords differ".into()));
            return;
        }
        if a.chars().count() < PASSWORD_MIN_CHARS {
            error.set(Some(format!(
                "a password has at least {PASSWORD_MIN_CHARS} characters"
            )));
            return;
        }
        let l = login.get_untracked().trim().to_string();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let t = token.get_value();
            let res =
                crate::session::set_password_with_link(&t, &a, (!l.is_empty()).then_some(&*l))
                    .await;
            busy.set(false);
            if let Err(e) = res {
                error.set(Some(e.to_string()));
            }
        });
    };
    view! {
        <div class="login-gate density-dense">
            <div class="card login-card">
                <h3>"Set your password"</h3>
                <p class="dim">
                    {format!(
                        "At least {PASSWORD_MIN_CHARS} characters, up to 2 MiB; any characters, no \
                         rules about which. The link works once."
                    )}
                </p>
                <label class="field-label" style="margin-top:12px">"Sign-in name"</label>
                <input
                    class="input"
                    type="text"
                    style="width:100%"
                    placeholder="owner"
                    autocomplete="username"
                    autocapitalize="none"
                    spellcheck="false"
                    prop:value=move || login.get()
                    on:input=move |ev| login.set(event_target_value(&ev))
                />
                <div class="dim hint">
                    "Leave it empty to keep the name you were given. The first account ('owner') \
                     can choose another one now, with its first password, and only then."
                </div>
                <label class="field-label" style="margin-top:8px">"New password"</label>
                <input
                    class="input"
                    type="password"
                    style="width:100%"
                    autocomplete="new-password"
                    prop:value=move || first.get()
                    on:input=move |ev| first.set(event_target_value(&ev))
                />
                <label class="field-label" style="margin-top:8px">"Again"</label>
                <input
                    class="input"
                    type="password"
                    style="width:100%"
                    autocomplete="new-password"
                    prop:value=move || second.get()
                    on:input=move |ev| second.set(event_target_value(&ev))
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        if ev.key() == "Enter" {
                            submit()
                        }
                    }
                />
                {move || {
                    error
                        .get()
                        .map(|e| view! { <div class="notice err" style="margin-top:10px">{e}</div> })
                }}
                <div class="row" style="justify-content:flex-end; margin-top:12px">
                    <button class="btn primary" disabled=move || busy.get() on:click=move |_| submit()>
                        {move || if busy.get() { "Saving…" } else { "Set password and sign in" }}
                    </button>
                </div>
            </div>
        </div>
    }
    .into_any()
}

/// The sidebar foot's line with accounts on (headless server design §5.2):
/// who this browser is signed in as, and Sign out.
#[component]
pub fn AccountFoot() -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    move || {
        crate::session::account_signal().get().map(|a| {
            view! {
                <div class="foot-row foot-account">
                    <span class="dim clip" title="the account this browser is signed in as">
                        {a.login}
                    </span>
                    <button
                        type="button"
                        class="btn ghost sm"
                        on:click=move |_| {
                            error.set(None);
                            spawn_local(async move {
                                if let Err(e) = crate::session::sign_out().await {
                                    error.set(Some(format!("signing out failed: {e}")));
                                }
                            })
                        }
                    >
                        "Sign out"
                    </button>
                </div>
                {move || error.get().map(|e| view! { <div class="foot-row notice err">{e}</div> })}
            }
        })
    }
}
