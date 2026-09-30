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
use serde::Deserialize;
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
}

static GATE: OnceLock<Gate> = OnceLock::new();

fn gate() -> &'static Gate {
    GATE.get_or_init(|| Gate {
        locked: ArcRwSignal::new(false),
        invalid_link: ArcRwSignal::new(false),
        prefill: ArcRwSignal::new(String::new()),
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

/// What `GET /api/session` answers. `kind` and `name` describe the current
/// principal and are deliberately ignored here: the dashboard has one thing
/// to decide on load, which is whether to show the page or the card.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SessionView {
    authenticated: bool,
}

/// Ask once, on app start, whether this browser has a session (§3.4).
pub fn start() {
    // `?login=invalid` is the login route's redirect for a key that matched no
    // row. It is a statement about the *link*, not about the cookie, so it goes
    // straight to the card without asking — and the query leaves the address
    // bar, so a reload does not repeat the accusation.
    if take_login_invalid() {
        gate().invalid_link.set(true);
        lock();
        return;
    }
    spawn_local(async move {
        // A transport failure is not a locked session — a gateway that is down
        // says nothing about this browser's cookie — so only an answer counts,
        // and `decode` raises the gate by itself for the refusal that does.
        if let Ok(s) = crate::api::get::<SessionView>("/api/session").await {
            if !s.authenticated {
                lock();
            }
        }
    });
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
    gate().invalid_link.set(false);
    gate().locked.set(false);
    Ok(())
}

/// The content pane while the gate is up (§8).
///
/// Dropping it is what re-runs the page: the shell mounts this *instead of*
/// the routes, so signing in re-creates every resource the page had in flight
/// and each one asks again, now with a cookie.
#[component]
pub fn LoginCard() -> impl IntoView {
    let invalid = Signal::derive(|| gate().invalid_link.get());
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
    view! {
        <div class="login-gate density-dense">
            <div class="card login-card">
                <h3>"This dashboard needs its session."</h3>
                <p class="dim">
                    "Open the login link printed in the lmgw process log, or paste the key."
                </p>
                <Show when=move || invalid.get()>
                    <div class="notice warn" style="margin-top:10px">
                        "that link's key is not valid — it may have been rotated"
                    </div>
                </Show>
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
