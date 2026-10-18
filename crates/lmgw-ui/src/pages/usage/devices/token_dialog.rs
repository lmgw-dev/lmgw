//! New token… (device link design §5.2): re-pairing a device. On a row with a
//! working key it asks the two choices — keep the current key until the new
//! one is used and confirmed (a move to a new phone, or a certificate
//! renewed before it lapses), or end it now (a lost or stolen device) — and
//! the second says what it closes before it does it. A row with no working
//! key just gets its new token.

use leptos::task::spawn_local;
use lmgw_api_types::device_link::DevicePairingState;
use lmgw_api_types::{DevicePaired, KeyRow};
use serde_json::json;

use super::logic;
use super::qr_card::QrSession;
use super::*;
use crate::widgets::{use_toasts, Modal, ModalFooter};

#[component]
pub(super) fn TokenDialog(
    target: RwSignal<Option<KeyRow>>,
    open: RwSignal<bool>,
    refresh: RwSignal<u32>,
    session: RwSignal<Option<QrSession>>,
    card_open: RwSignal<bool>,
) -> impl IntoView {
    let toasts = use_toasts();
    let end_now = RwSignal::new(false);
    let busy = RwSignal::new(false);
    // Opened afresh: the safe choice first.
    Effect::new(move |_| {
        if open.get() {
            end_now.set(false);
        }
    });
    let has_key = move || {
        target.with(|t| {
            t.as_ref()
                .and_then(|r| r.pairing.as_ref())
                .is_some_and(logic::has_working_key)
        })
    };
    let ends = move || has_key() && end_now.get();
    let make = move |_| {
        let Some(r) = target.get_untracked() else {
            return;
        };
        let end_current = ends();
        busy.set(true);
        spawn_local(async move {
            let res = crate::api::post::<DevicePaired, _>(
                "/api/op/device_token",
                &json!({ "id": r.id, "end_current": end_current }),
            )
            .await;
            busy.set(false);
            open.set(false);
            match res {
                Ok(v) => {
                    session.set(Some(QrSession::from_answer(v, true)));
                    card_open.set(true);
                    toasts.ok(format!("device '{}' has a new pairing link", bare(&r.name)));
                    refresh.update(|n| *n = n.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        <Modal open=open title="New pairing token">
            {move || {
                target
                    .get()
                    .map(|r| {
                        let name = bare(&r.name).to_string();
                        let state = r.pairing.as_ref().map(|p| p.state);
                        let keyed = has_key();
                        let disabled = (!r.enabled)
                            .then_some(
                                "The device is disabled, so its new key is refused too until you enable it on its row.",
                            );
                        let plain = match state {
                            Some(DevicePairingState::V1) => {
                                "It was paired before TLS: its old key opens nothing, so there is no key to end. The new link pairs it again."
                            }
                            Some(DevicePairingState::Enrolled) => {
                                "The key it enrolled was never confirmed; whatever enrols with the new token replaces it, and you confirm that one's code."
                            }
                            _ => {
                                "The new link pairs it; its earlier token ends, and you confirm the code it shows."
                            }
                        };
                        view! {
                            <p>
                                <b>{format!("A new pairing token for device '{name}'?")}</b>
                            </p>
                            {if keyed {
                                view! {
                                    <div class="token-choices">
                                        <label class="check">
                                            <input
                                                type="radio"
                                                name="repair"
                                                prop:checked=move || !end_now.get()
                                                on:change=move |_| end_now.set(false)
                                            />
                                            <span>
                                                <b>" Keep the current key until the new one is confirmed"</b>
                                                <span class="dim">
                                                    " — a move to a new phone, or the same device renewing its certificate. The current key works on; the new one is confirmed with its code, and that ends the old."
                                                </span>
                                            </span>
                                        </label>
                                        <label class="check">
                                            <input
                                                type="radio"
                                                name="repair"
                                                prop:checked=move || end_now.get()
                                                on:change=move |_| end_now.set(true)
                                            />
                                            <span>
                                                <b>" End the current key now"</b>
                                                <span class="dim">
                                                    " — a lost or stolen device. Nothing it holds works from now on."
                                                </span>
                                            </span>
                                        </label>
                                    </div>
                                }
                                    .into_any()
                            } else {
                                view! { <p class="dim">{plain}</p> }.into_any()
                            }}
                            <Show when=ends>
                                <p class="notice warn">
                                    "Its Chat feed, voice sessions and tool link close now, and its requests are refused until the new token enrols."
                                </p>
                            </Show>
                            {disabled.map(|d| view! { <p class="notice warn">{d}</p> })}
                            <p class="dim mini-note">
                                "The new link is shown next, once. Its policy and history stay."
                            </p>
                        }
                    })
            }}
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| open.set(false)>
                    "Cancel"
                </button>
                <button
                    class="btn"
                    class:danger=ends
                    class:primary=move || !ends()
                    disabled=move || busy.get()
                    on:click=make
                >
                    {move || if ends() { "End its key and make the token" } else { "Make the token" }}
                </button>
            </ModalFooter>
        </Modal>
    }
}
