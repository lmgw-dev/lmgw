//! What a device row says under its name (device link design §9.3): where
//! its pairing stands with the owner's next step (Confirm, End pairing
//! code), a contested notice, its certificate's dates and notice, its URLs,
//! and each connection it holds open. Live: the rows follow the keys list,
//! which refetches on every `keys` frame of `/api/events`.

use leptos::task::spawn_local;
use lmgw_api_types::{DeviceConfirmed, DeviceTokenEnded, KeyRow};
use serde_json::json;

use super::logic::{self, Awaiting};
use super::*;
use crate::widgets::use_toasts;

/// An RFC 3339 time as the reader's own clock: `14:32`, with the date when
/// it is not today.
pub(super) fn local_clock(ts: &str, now_s: f64) -> String {
    let Some(secs) = logic::parse_utc_secs(ts) else {
        return ts.to_string();
    };
    let at = |s: f64| js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(s * 1000.0));
    let (d, n) = (at(secs), at(now_s));
    let hm = format!("{:02}:{:02}", d.get_hours(), d.get_minutes());
    if (d.get_full_year(), d.get_month(), d.get_date())
        == (n.get_full_year(), n.get_month(), n.get_date())
    {
        hm
    } else {
        format!(
            "{:04}-{:02}-{:02} {hm}",
            d.get_full_year(),
            d.get_month() + 1,
            d.get_date()
        )
    }
}

/// Confirm the key that waits (§3.5), with the code the card displayed; for
/// a contested row an input for the code the device shows. `done` runs after
/// lmgw took the confirmation.
#[component]
pub(super) fn ConfirmAction(
    id: i64,
    awaiting: Awaiting,
    refresh: RwSignal<u32>,
    #[prop(optional, into)] done: Option<Callback<()>>,
) -> impl IntoView {
    let toasts = use_toasts();
    let typed = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let contested = awaiting.contested_from.is_some();
    let a = awaiting.clone();
    let confirm = move |_| {
        let body = match logic::confirm_body(id, &a, &typed.get_untracked()) {
            Ok(b) => b,
            Err(why) => {
                toasts.err(why.to_string());
                return;
            }
        };
        busy.set(true);
        spawn_local(async move {
            let res = crate::api::post::<DeviceConfirmed, _>("/api/op/device_confirm", &body).await;
            busy.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v.message);
                    refresh.update(|v| *v = v.wrapping_add(1));
                    if let Some(d) = done {
                        d.run(());
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        {contested
            .then(|| {
                view! {
                    <input
                        class="input mono"
                        placeholder="code the device shows"
                        aria-label="The code the device shows"
                        prop:value=move || typed.get()
                        on:input=move |ev| typed.set(event_target_value(&ev))
                    />
                    " "
                }
            })}
        <button class="btn sm primary" disabled=move || busy.get() on:click=confirm>
            "Confirm"
        </button>
    }
}

/// End device `id`'s pairing code (`device_token_end`), saying so; `done`
/// runs with whether lmgw ended one.
pub(super) fn end_token(
    id: i64,
    refresh: RwSignal<u32>,
    toasts: crate::widgets::Toasts,
    done: impl FnOnce(bool) + 'static,
) {
    spawn_local(async move {
        let res = crate::api::post::<DeviceTokenEnded, _>(
            "/api/op/device_token_end",
            &json!({ "id": id }),
        )
        .await;
        match res {
            Ok(v) => {
                toasts.ok(v.message);
                refresh.update(|v| *v = v.wrapping_add(1));
                done(v.ended);
            }
            Err(e) => {
                toasts.err(e.to_string());
                done(false);
            }
        }
    });
}

/// A row's pairing under its name.
#[component]
pub(super) fn PairingNotes(
    id: i64,
    row: Memo<Option<KeyRow>>,
    refresh: RwSignal<u32>,
) -> impl IntoView {
    let toasts = use_toasts();
    let Now(now) = expect_context::<Now>();
    let busy = RwSignal::new(false);
    let ask = Memo::new(move |_| row.with(|r| r.as_ref().and_then(logic::pairing_ask)));
    let bare_name = move || {
        row.with(|r| {
            r.as_ref()
                .map(|r| bare(&r.name).to_string())
                .unwrap_or_default()
        })
    };
    let pairing = move || {
        let r = row.get()?;
        let p = r.pairing.clone()?;
        let at = |t: &str| local_clock(t, now.get());
        let words = logic::pairing_words(&p, &at);
        let ask = ask.get();
        let confirm = ask.as_ref().and_then(|a| a.confirm.clone());
        let alive = ask.as_ref().and_then(|a| a.token_until.clone());
        let contested = confirm.as_ref().and_then(|a| a.contested_from.clone());
        let confirm_part = confirm.map(|a| {
            let note = if a.replaces {
                " (its current key works until you confirm)"
            } else {
                ""
            };
            view! {
                " "
                {note}
                " "
                <ConfirmAction id=id awaiting=a refresh=refresh/>
            }
        });
        let end_part = alive.map(|until| {
            let on_end = move |_| {
                busy.set(true);
                end_token(id, refresh, toasts, move |_| busy.set(false));
            };
            view! {
                " "
                <button
                    class="btn ghost sm"
                    disabled=move || busy.get()
                    title=format!(
                        "end the pairing code now (it is valid until {}): its link pairs nothing more",
                        local_clock(&until, now.get_untracked()),
                    )
                    on:click=on_end
                >
                    "End pairing code"
                </button>
            }
        });
        let contested_note = (p.contested_at.is_some() && contested.is_none()).then(|| {
            // A used token came back with another key, and the row is not
            // waiting for a confirmation any more (the owner deleted or
            // re-paired): the notice stays until the next token.
            let from = p.contested_from.clone().unwrap_or_default();
            view! {
                <div class="mini-note notice warn">
                    {logic::contested_words(&bare_name(), &from)}
                </div>
            }
        });
        let contested_waiting = contested.map(|from| {
            view! {
                <div class="mini-note notice warn">
                    {logic::contested_words(&bare_name(), &from)}
                    ". Confirm only with the code the device itself shows."
                </div>
            }
        });
        Some(view! {
            <div class="mini-note">{words} {confirm_part} {end_part}</div>
            {contested_waiting}
            {contested_note}
        })
    };
    let cert = move || {
        let r = row.get()?;
        let p = r.pairing.as_ref()?;
        let state = logic::cert_state(p, now.get());
        let words = state.words()?;
        let warns = state.warns();
        let urls = p.urls.clone();
        Some(view! {
            <div class="mini-note" class:dev-warn=warns class:dim=!warns>
                {words}
                {(!urls.is_empty())
                    .then(|| {
                        view! {
                            " · reaches lmgw at "
                            <span class="dev-urls mono-sm">
                                {urls
                                    .into_iter()
                                    .map(|u| view! { <span class="dev-url">{u}</span> " " })
                                    .collect_view()}
                            </span>
                        }
                    })}
            </div>
        })
    };
    let links = move || {
        let r = row.get()?;
        let at = |t: &str| local_clock(t, now.get());
        let lines = logic::links_words(&r, &at);
        (!lines.is_empty()).then(|| {
            view! {
                <div class="mini-note dev-links">
                    {lines
                        .into_iter()
                        .map(|l| view! { <div class="dev-link">{l}</div> })
                        .collect_view()}
                </div>
            }
        })
    };
    view! { {pairing} {cert} {links} }
}
