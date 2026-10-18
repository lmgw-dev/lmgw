//! The link strip above the device rows (device link design §9.3): the
//! device listener's scope and state, the CA's fingerprint and key mode with
//! Copy whole and Copy CA certificate, Rotate the device CA… with its
//! confirmation naming every paired device (§5.5), the count of devices
//! that need a new pairing, and the failed pairings of the last hour (§3.3).

use leptos::task::spawn_local;
use lmgw_api_types::device_link::{DeviceCaKeyMode, DeviceCaState, DevicesStatus};
use lmgw_api_types::{DeviceCaRotated, GatewayStatus, KeysResponse};
use serde_json::json;

use super::logic::{self, Tone};
use super::*;
use crate::widgets::{copy_secret, use_toasts, Modal, ModalFooter};

/// What the strip reads from the keys list.
#[derive(Clone, PartialEq, Eq)]
struct Paired {
    /// The devices a rotation resets: those holding a key of their own.
    names: Vec<String>,
    /// The devices still paired before TLS.
    v1: usize,
}

#[component]
pub(super) fn LinkStrip(keys: Src<KeysResponse>, refresh: RwSignal<u32>) -> impl IntoView {
    let toasts = use_toasts();
    let status = RwSignal::new(None::<DevicesStatus>);
    // The status follows the card's refetches: every `keys` frame of
    // `/api/events` (a token, an enrolment, a confirmation) bumps `refresh`.
    Effect::new(move |_| {
        refresh.track();
        spawn_local(async move {
            if let Ok(s) = crate::api::get::<GatewayStatus>("/api/status").await {
                status.set(Some(s.devices));
            }
        });
    });
    let paired = Memo::new(move |_| {
        keys.data.with(|d| {
            d.as_ref()
                .map(|k| Paired {
                    names: logic::paired_names(&k.keys),
                    v1: logic::v1_count(&k.keys),
                })
                .unwrap_or(Paired {
                    names: Vec::new(),
                    v1: 0,
                })
        })
    });
    let rotate_open = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let rotate = move |_| {
        busy.set(true);
        spawn_local(async move {
            let res =
                crate::api::post::<DeviceCaRotated, _>("/api/op/device_ca_rotate", &json!({}))
                    .await;
            busy.set(false);
            rotate_open.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v.message);
                    refresh.update(|n| *n = n.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let listener = move || {
        let s = status.get()?;
        let (line, tone) = logic::listener_words(&s.listener);
        Some(view! {
            <span
                class="chip"
                class:warn=tone == Tone::Warn
                class:off=tone == Tone::Off
                class:ok=tone == Tone::Ok
            >
                <span class="dot"></span>
                {line}
            </span>
        })
    };
    let authority = move || {
        let s = status.get()?;
        let ca = s.ca;
        if let Some(e) = ca
            .error
            .as_ref()
            .filter(|_| ca.state == DeviceCaState::Error)
        {
            return Some(
                view! {
                    <span class="chip warn" title=e.message.clone()>
                        <span class="dot"></span>
                        {format!("device CA unusable: {}", e.message)}
                    </span>
                }
                .into_any(),
            );
        }
        let Some(fp) = ca.fp.clone() else {
            return Some(
                view! {
                    <span class="dim">
                        "device CA: " {logic::ca_words(DeviceCaState::Missing, None)}
                        " (made when the listener first runs)"
                    </span>
                }
                .into_any(),
            );
        };
        let wrapped = ca.key_mode.map(|m| m == DeviceCaKeyMode::Wrapped);
        let mode = logic::ca_words(ca.state, wrapped);
        let short = logic::fp_short(&fp);
        let whole = fp.clone();
        let pem = ca.certificate_pem.clone();
        let created = ca.created.clone().unwrap_or_default();
        Some(
            view! {
                <span class="dev-ca">
                    <span class="dim">"device CA "</span>
                    <span class="mono-sm" title=fp>{short}</span>
                    <span class="dim" title=format!("made {}", logic::date_of(&created))>
                        {format!(" · {mode}")}
                    </span>
                    <button
                        class="btn ghost sm"
                        title="the whole fingerprint, as a pairing link pins it"
                        on:click=move |_| copy_secret(
                            &whole,
                            toasts,
                            "fingerprint copied".into(),
                        )
                    >
                        "Copy whole"
                    </button>
                    <button
                        class="btn ghost sm"
                        disabled=pem.is_none()
                        title="the CA certificate, PEM: curl --cacert takes it"
                        on:click=move |_| {
                            if let Some(p) = pem.as_deref() {
                                copy_secret(p, toasts, "CA certificate copied".into());
                            }
                        }
                    >
                        "Copy CA certificate"
                    </button>
                    <button class="btn ghost sm" on:click=move |_| rotate_open.set(true)>
                        "Rotate the device CA…"
                    </button>
                </span>
            }
            .into_any(),
        )
    };
    let v1 = move || {
        let n = paired.get().v1;
        (n > 0).then(|| {
            view! {
                <span
                    class="chip warn"
                    title="Paired with a link from before TLS: its old key opens nothing. New token… on its row pairs it again."
                >
                    {format!("{n} device{} need{} a new pairing", if n == 1 { "" } else { "s" }, if n == 1 { "s" } else { "" })}
                </span>
            }
        })
    };
    let failures = move || {
        let p = status.get()?.pairing;
        (p.failures_last_hour > 0).then(|| {
            let from = p
                .last_failure_from
                .as_deref()
                .map(|f| format!(", the last from {f}"))
                .unwrap_or_default();
            view! {
                <span
                    class="chip warn"
                    title=format!(
                        "Unknown, used, lapsed or ended pairing codes presented to the device listener. The counts live in memory and start at zero with each start of lmgw (counting since {}).",
                        p.counted_since,
                    )
                >
                    {format!("{} failed pairing(s) in the last hour{from}", p.failures_last_hour)}
                </span>
            }
        })
    };
    let alive = move || {
        let n = status.get()?.pairing.tokens_alive;
        (n > 0).then(|| {
            view! {
                <span class="dim" title="Pairing codes alive: while any is, a device may open a connection without a certificate, for enrolment alone.">
                    {format!("{n} pairing code(s) alive")}
                </span>
            }
        })
    };

    view! {
        <div class="dev-strip">{listener} {authority} {v1} {failures} {alive}</div>
        <Modal open=rotate_open title="Rotate the device CA">
            {move || {
                let names = paired.get().names;
                let who = if names.is_empty() {
                    "No device holds a key yet.".to_string()
                } else {
                    format!("Every paired device ({}) stops connecting at once and must be paired again with a new token.", names.join(", "))
                };
                view! {
                    <p><b>"Replace lmgw's device CA?"</b></p>
                    <p class="notice warn">{who}</p>
                    <p class="dim">
                        "Rotate only if lmgw's data directory, or its wallet, may have been copied. The old key is not kept; every pairing code alive ends, and what each device holds open closes. Server certificates are issued anew. Each device's policy and history stay; New token… on its row pairs it again."
                    </p>
                }
            }}
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| rotate_open.set(false)>
                    "Cancel"
                </button>
                <button class="btn danger" disabled=move || busy.get() on:click=rotate>
                    "Rotate the CA"
                </button>
            </ModalFooter>
        </Modal>
    }
}
