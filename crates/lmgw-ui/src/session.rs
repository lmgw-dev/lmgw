//! The dashboard's own session: the gate, and the card that is the page while
//! the gate is up (principals design §3.4, §8).
//!
//! Every other piece of app-wide state here is a `provide_context` — the live
//! bus, the ops set, the toasts. This one cannot be. `api::decode` is what
//! learns the session is gone, and it is a plain `async fn` with no component,
//! no reactive owner and no context to read. So the gate is a pair of
//! `ArcRwSignal`s behind a `OnceLock`: reference-counted rather than
//! arena-allocated, which is what makes them legal outside the ownership tree.
//!
//! The cookie itself is never touched from here. It is `HttpOnly` (§3.3), so
//! the only things this module can do are ask whether it works
//! (`GET /api/session`) and hand the gateway a key to set a new one
//! (`POST /api/session`).

use std::sync::OnceLock;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::SessionView;
use serde_json::json;
use wasm_bindgen::JsValue;

struct Gate {
    /// `true` while this browser holds no principal.
    locked: ArcRwSignal<bool>,
    /// The login link carried a key the gateway did not accept (`?login=
    /// invalid`, §3.4) — a different sentence on the same card.
    invalid_link: ArcRwSignal<bool>,
    /// A key the page is already holding, put in the box so the owner does not
    /// have to paste it: the plaintext from a `owner:dashboard` rotate whose
    /// automatic re-login did not land (§3.12). Set by the Keys card through
    /// [`carry_key`] / [`lock_with_key`], spent by [`LoginCard`].
    prefill: ArcRwSignal<String>,
    /// Accounts are on (headless server design D25): the card asks for a
    /// sign-in name and a password, the key under a fold.
    accounts_on: ArcRwSignal<bool>,
    /// The page was opened at `/set-password#token=…` (§5.5): the card is
    /// the set-password form, with this token.
    set_password: ArcRwSignal<Option<String>>,
    /// The signed-in session's account, while accounts are on.
    account: ArcRwSignal<Option<lmgw_api_types::SessionAccount>>,
    /// The server image (D2): the card names `lmgw-server login-link`.
    container: ArcRwSignal<bool>,
    /// `GET /api/session` answered once: until it has, the card knows
    /// neither whether accounts are on nor where this gateway runs, so it
    /// says it is asking (or why it cannot) instead of guessing.
    known: ArcRwSignal<bool>,
    /// Why the last read of `GET /api/session` failed, while it is retried.
    read_error: ArcRwSignal<Option<String>>,
    /// The `/api/events` stream's last frame, `revoked` (D58): why this
    /// browser's credential ended, said on the sign-in card.
    revoked: ArcRwSignal<Option<String>>,
}

static GATE: OnceLock<Gate> = OnceLock::new();

fn gate() -> &'static Gate {
    GATE.get_or_init(|| Gate {
        locked: ArcRwSignal::new(false),
        invalid_link: ArcRwSignal::new(false),
        prefill: ArcRwSignal::new(String::new()),
        accounts_on: ArcRwSignal::new(false),
        set_password: ArcRwSignal::new(None),
        account: ArcRwSignal::new(None),
        container: ArcRwSignal::new(false),
        known: ArcRwSignal::new(false),
        read_error: ArcRwSignal::new(None),
        revoked: ArcRwSignal::new(None),
    })
}

/// Raise the gate. Called by [`crate::api`] on `401 session_required` and by
/// [`start`]; nothing else should need it.
pub fn lock() {
    gate().locked.set(true);
}

/// Hold `token` for the login card without raising the gate (§3.12).
///
/// The Keys card's safety net: rotating `owner:dashboard` invalidates this
/// browser's cookie, and if the immediate re-login does not land there is no
/// way back — `key_reveal` needs the session that just died. The page keeps
/// the plaintext on screen, but any other request on it can answer 401 and
/// raise the gate first, so the key is handed here as well and the card opens
/// with it already in the box.
pub fn carry_key(token: &str) {
    gate().prefill.set(token.to_string());
}

/// The credential this browser's event stream was opened with ended (the
/// stream's `revoked` frame, headless server design D58): raise the gate
/// and say why on the card.
pub fn revoked(message: String) {
    gate().account.set(None);
    gate().revoked.set(Some(message));
    lock();
}

/// Whether this browser's cookie still signs it in (`GET /api/session`):
/// asked when a stream says its credential ended, since the page may have
/// signed in again since. A failed read is no.
pub async fn still_signed_in() -> bool {
    crate::api::get::<SessionView>("/api/session")
        .await
        .is_ok_and(|s| s.authenticated)
}

/// The server image (D2), as `GET /api/session` said it; `false` until it
/// answered.
pub fn container_signal() -> Signal<bool> {
    Signal::derive(|| gate().container.get())
}

/// [`carry_key`] and raise the gate now — the button on that same screen.
pub fn lock_with_key(token: &str) {
    carry_key(token);
    gate().invalid_link.set(false);
    gate().locked.set(true);
}

/// Reactive read, for the shell's `Show` and for the live stream.
pub fn locked_signal() -> Signal<bool> {
    Signal::derive(|| gate().locked.get())
}

/// Untracked read, for the places that are already inside an async loop.
pub fn is_locked() -> bool {
    gate().locked.get_untracked()
}

/// The signed-in session's account, while accounts are on (§5.2).
pub fn account_signal() -> Signal<Option<lmgw_api_types::SessionAccount>> {
    Signal::derive(|| gate().account.get())
}

/// Ask once, on app start, whether this browser has a session (§3.4).
pub fn start() {
    // `/set-password#token=…` (headless server design §5.5): the link's page
    // is the card's form, whether or not this browser holds a session. The
    // fragment leaves the address bar at once, so it is in no history entry
    // a later visit reads back.
    if let Some(token) = take_set_password_token() {
        gate().set_password.set(Some(token));
        gate().accounts_on.set(true);
        gate().known.set(true);
        lock();
        return;
    }
    // `?login=invalid` is the login route's redirect for a key that matched no
    // row. It is a statement about the *link*, not about the cookie, so it goes
    // straight to the card without asking — and the query leaves the address
    // bar, so a reload does not repeat the accusation.
    if take_login_invalid() {
        gate().invalid_link.set(true);
        lock();
    }
    read_session(READ_RETRY_FIRST);
}

/// How long the load check waits before it asks again after a failure:
/// doubling from the first, up to the last, for as long as it fails.
const READ_RETRY_FIRST: std::time::Duration = std::time::Duration::from_secs(1);
/// The longest wait between two reads.
const READ_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// `GET /api/session`, asked until it answers.
///
/// A transport failure is not a locked session — a gateway that is down
/// says nothing about this browser's cookie — so only an answer counts,
/// and `decode` raises the gate by itself for the refusal that does. A
/// failure is said on the card (when the gate is up) and asked again after
/// `wait`, doubling up to [`READ_RETRY_MAX`]. `kind` and `name` of the
/// answer are deliberately ignored here: the dashboard has one thing to
/// decide on load, which is whether to show the page or the card.
fn read_session(wait: std::time::Duration) {
    spawn_local(async move {
        match crate::api::get::<SessionView>("/api/session").await {
            Ok(s) => {
                gate().accounts_on.set(s.accounts_on);
                gate().container.set(s.container);
                gate().account.set(s.account.clone());
                gate().read_error.set(None);
                gate().known.set(true);
                if !s.authenticated {
                    lock();
                }
            }
            Err(e) => {
                gate().read_error.set(Some(e.to_string()));
                set_timeout(move || read_session((wait * 2).min(READ_RETRY_MAX)), wait);
            }
        }
    });
}

/// Was the page opened at `/set-password#token=<t>`? The token, and the
/// address bar back at `/`.
fn take_set_password_token() -> Option<String> {
    let loc = window().location();
    if loc.pathname().ok()?.trim_end_matches('/') != "/set-password" {
        return None;
    }
    let hash = loc.hash().unwrap_or_default();
    let token = hash
        .trim_start_matches('#')
        .split('&')
        .find_map(|p| p.strip_prefix("token="))
        .unwrap_or_default()
        .to_string();
    if let Ok(h) = window().history() {
        let _ = h.replace_state_with_url(&JsValue::NULL, "", Some("/"));
    }
    Some(token)
}

/// Was the page opened at `?login=invalid`? Strips the query if so.
fn take_login_invalid() -> bool {
    let loc = window().location();
    let search = loc.search().unwrap_or_default();
    if !search
        .trim_start_matches('?')
        .split('&')
        .any(|p| p == "login=invalid")
    {
        return false;
    }
    let path = loc.pathname().unwrap_or_else(|_| "/".into());
    // Raw `replaceState`, as on the Chat page: the route has not changed, only
    // the query the gateway used to tell us something once.
    if let Ok(h) = window().history() {
        let _ = h.replace_state_with_url(&JsValue::NULL, "", Some(&path));
    }
    true
}

/// Present an owner key and take the cookie back (§3.4).
///
/// Also the Keys page's second caller: rotating `owner:dashboard` invalidates
/// this browser's cookie along with every other one, so the new key is
/// presented in the same breath (§3.12).
pub async fn sign_in(token: &str) -> crate::api::Result<()> {
    crate::api::post_no_content("/api/session", &json!({ "token": token })).await?;
    after_sign_in().await;
    Ok(())
}

/// Sign in with a sign-in name and a password (headless server design
/// §5.2), and re-run the page as [`sign_in`] does.
pub async fn sign_in_password(login: &str, password: &str) -> crate::api::Result<()> {
    crate::api::post_no_content(
        "/api/session",
        &json!({ "login": login, "password": password }),
    )
    .await?;
    after_sign_in().await;
    Ok(())
}

/// Set a password with a set-password link's token (§5.5); the answer
/// signs this browser in.
pub async fn set_password_with_link(
    token: &str,
    password: &str,
    login: Option<&str>,
) -> crate::api::Result<()> {
    crate::api::post_no_content(
        "/api/account/password",
        &json!({ "token": token, "password": password, "login": login }),
    )
    .await?;
    gate().set_password.set(None);
    after_sign_in().await;
    Ok(())
}

/// Read the new session's account, then drop the gate.
async fn after_sign_in() {
    if let Ok(s) = crate::api::get::<SessionView>("/api/session").await {
        gate().accounts_on.set(s.accounts_on);
        gate().container.set(s.container);
        gate().account.set(s.account);
        gate().known.set(true);
    }
    gate().invalid_link.set(false);
    gate().revoked.set(None);
    gate().locked.set(false);
}

/// Sign this browser out (`DELETE /api/session`): its session ends, and the
/// card goes up.
pub async fn sign_out() -> crate::api::Result<()> {
    let resp = gloo_net::http::Request::delete("/api/session")
        .send()
        .await
        .map_err(|e| crate::api::Error::Transport(e.to_string()))?;
    crate::api::checked(resp).await?;
    gate().account.set(None);
    lock();
    Ok(())
}

/// The content pane while the gate is up (§8).
///
/// Dropping it is what re-runs the page: the shell mounts this *instead of*
/// the routes, so signing in re-creates every resource the page had in flight
/// and each one asks again, now with a cookie. With accounts on it asks for
/// a sign-in name and a password; on a set-password link it is that form.
#[component]
pub fn LoginCard() -> impl IntoView {
    let accounts_on = Signal::derive(|| gate().accounts_on.get());
    let known = Signal::derive(|| gate().known.get());
    let link = Signal::derive(|| gate().set_password.get());
    let revoked = Signal::derive(|| gate().revoked.get());
    view! {
        {move || match link.get() {
            Some(token) => view! { <crate::session_cards::SetPasswordCard token=token/> }.into_any(),
            None if !known.get() => view! { <ReadingCard/> }.into_any(),
            None if accounts_on.get() => {
                view! {
                    <crate::session_cards::PasswordCard
                        revoked=revoked
                        key_card=|| view! { <KeyCard inner=true/> }
                    />
                }
                .into_any()
            }
            None => view! { <KeyCard/> }.into_any(),
        }}
    }
}

/// The card while the load check has not answered: what it is waiting for,
/// and why the last try failed. Signing in needs to know whether accounts
/// are on, so the card does not guess.
#[component]
fn ReadingCard() -> impl IntoView {
    let error = Signal::derive(|| gate().read_error.get());
    view! {
        <div class="login-gate density-dense">
            <div class="card login-card">
                <h3>"Reaching this gateway…"</h3>
                {move || match error.get() {
                    Some(e) => view! {
                        <div class="notice err" style="margin-top:10px">
                            {format!("asking how to sign in failed: {e}; trying again")}
                        </div>
                    }
                    .into_any(),
                    None => view! { <p class="dim">"asking how to sign in"</p> }.into_any(),
                }}
            </div>
        </div>
    }
}

/// The key card: today's sign-in, and with accounts on the fallback under a
/// fold.
#[component]
pub fn KeyCard(
    /// Inside the password card's fold: the form without its own card.
    #[prop(optional)]
    inner: bool,
) -> impl IntoView {
    let invalid = Signal::derive(|| gate().invalid_link.get());
    let revoked = Signal::derive(|| gate().revoked.get());
    // Normally empty. Non-empty when the gate was raised by [`lock_with_key`]:
    // the card opens with that key in the box, and says why.
    let carried = gate().prefill.get_untracked();
    let rotated_away = !carried.is_empty();
    let token = RwSignal::new(carried);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let submit = move || {
        if busy.get_untracked() {
            return;
        }
        let t = token.get_untracked().trim().to_string();
        if t.is_empty() {
            error.set(Some("paste the key from the login link".into()));
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let res = sign_in(&t).await;
            busy.set(false);
            match res {
                // The key is not kept around after it has been spent.
                Ok(()) => {
                    gate().prefill.set(String::new());
                    token.set(String::new());
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
    let heading = (!inner).then(|| {
        view! {
            <h3>"This dashboard needs its session."</h3>
            <p class="dim">
                "Open the login link printed in the lmgw process log, or paste the key."
            </p>
        }
    });
    view! {
        <div class:login-gate=!inner class="density-dense">
            <div class:card=!inner class:login-card=!inner>
                {heading}
                <Show when=move || invalid.get()>
                    <div class="notice warn" style="margin-top:10px">
                        "that link's key is not valid — it may have been rotated"
                    </div>
                </Show>
                {move || {
                    revoked
                        .get()
                        .filter(|_| !inner)
                        .map(|m| view! { <div class="notice warn" style="margin-top:10px">{m}</div> })
                }}
                <Show when=move || rotated_away>
                    <div class="notice warn" style="margin-top:10px">
                        "sign back in with this key: the tab's session was rotated away"
                    </div>
                </Show>
                <input
                    class="input mono"
                    type="password"
                    style="margin-top:12px;width:100%"
                    placeholder="lmgw-owner-…"
                    autocomplete="current-password"
                    prop:value=move || token.get()
                    on:input=move |ev| token.set(event_target_value(&ev))
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
                    <button
                        class="btn primary"
                        disabled=move || busy.get()
                        on:click=move |_| submit()
                    >
                        {move || if busy.get() { "Signing in…" } else { "Sign in" }}
                    </button>
                </div>
            </div>
        </div>
    }
}
