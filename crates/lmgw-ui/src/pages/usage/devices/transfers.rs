//! The Devices card's Transfers section (device transfers design §11.1):
//! every file lmgw relays between paired devices, newest first — from and
//! to, the app id, the title and the file, its size, an upload's progress,
//! the state with when it expires or why it ended, and Cancel on one in
//! progress after a confirmation that says what it ends, then done. Its head
//! says the bytes the transfers in progress hold and the space left for new
//! offers.
//!
//! Live: `GET /transfers/api/transfers` again on every `/api/events` `keys`
//! frame with `what: "transfer"`, which lmgw sends on each state change and
//! at most once a second while an upload writes.

use leptos::task::spawn_local;
use lmgw_api_types::transfer::{Transfer, TransferList, TransferState};

use super::logic;
use super::*;
use crate::fmt::{fmt_bytes, rel_time_at};
use crate::live::use_live;
use crate::widgets::{use_toasts, Modal, ModalFooter};

/// A state's chip: its class and its words (`ready` says whom it waits
/// for).
fn state_chip(t: &Transfer) -> (&'static str, String) {
    match &t.state {
        TransferState::Uploading => ("chip live", "uploading".into()),
        TransferState::Ready => ("chip info", format!("waiting for {}", t.to.name)),
        TransferState::Delivered => ("chip ok", "delivered".into()),
        TransferState::Declined => ("chip off", "declined".into()),
        TransferState::Canceled => ("chip off", "canceled".into()),
        TransferState::Expired => ("chip off", "expired".into()),
        TransferState::Failed => ("chip err", "failed".into()),
        other => ("chip off", other.as_str().to_string()),
    }
}

/// An upload's progress in whole percent; `None` once it is not uploading.
fn progress(t: &Transfer) -> Option<u32> {
    (t.state == TransferState::Uploading && t.size > 0)
        .then(|| ((t.received as f64 / t.size as f64) * 100.0).floor() as u32)
}

/// When it expires, or when and why it ended, as of `now_s`.
fn when_words(t: &Transfer, now_s: f64) -> String {
    let rel = |ts: &str| {
        logic::parse_utc_secs(ts)
            .map(|s| rel_time_at(s, now_s))
            .unwrap_or_else(|| ts.to_string())
    };
    match (&t.ended_at, &t.end_reason) {
        (Some(at), Some(why)) => format!("ended {}: {why}", rel(at)),
        (Some(at), None) => format!("ended {}", rel(at)),
        _ => format!("expires {}", rel(&t.expires_at)),
    }
}

/// The section's head: what lmgw holds now, and the room left.
fn head_words(l: &TransferList) -> String {
    let open = l.transfers.iter().filter(|t| t.state.is_active()).count();
    format!(
        "{open} in progress · {} held · {} free for new offers",
        fmt_bytes(l.stored_bytes),
        fmt_bytes(l.free_bytes)
    )
}

#[component]
pub(super) fn TransfersSection() -> impl IntoView {
    let toasts = use_toasts();
    let live = use_live();
    let Now(now) = expect_context::<Now>();
    let tick = RwSignal::new(0u32);
    let list = RwSignal::new(None::<TransferList>);
    let err = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        tick.track();
        spawn_local(async move {
            match crate::api::get::<TransferList>("/transfers/api/transfers").await {
                Ok(l) => {
                    list.set(Some(l));
                    err.set(None);
                }
                Err(e) => err.set(Some(e.to_string())),
            }
        });
    });
    Effect::new(move |_| {
        if live
            .keys
            .with(|k| k.as_ref().is_some_and(|k| k.what == "transfer"))
        {
            tick.update(|v| *v = v.wrapping_add(1));
        }
    });
    let asking = RwSignal::new(None::<Transfer>);
    let ask_open = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let ask = move |t: Transfer| {
        asking.set(Some(t));
        ask_open.set(true);
    };
    let confirm = move |_| {
        let Some(t) = asking.get_untracked() else {
            return;
        };
        busy.set(true);
        spawn_local(async move {
            let path = format!("/transfers/api/transfers/{}/cancel", t.id);
            let res = crate::api::post::<Transfer, _>(&path, &serde_json::json!({})).await;
            busy.set(false);
            ask_open.set(false);
            match res {
                Ok(_) => {
                    toasts.ok(format!(
                        "the transfer from {} to {} is canceled; its bytes are deleted",
                        t.from.name, t.to.name
                    ));
                    tick.update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let rows = move || {
        let now_s = now.get();
        list.get()
            .map(|l| l.transfers)
            .unwrap_or_default()
            .into_iter()
            .map(|t| {
                let (chip, words) = state_chip(&t);
                let title = if t.title.is_empty() {
                    t.filename.clone()
                } else {
                    t.title.clone()
                };
                let file_line = format!("{} · {} · {}", t.filename, t.kind, t.app_id);
                let pct = progress(&t);
                let size = match pct {
                    Some(_) => lmgw_api_types::units::fmt_bytes_of(t.received, t.size),
                    None => fmt_bytes(t.size),
                };
                let when = when_words(&t, now_s);
                let active = t.state.is_active();
                let t2 = t.clone();
                view! {
                    <tr>
                        <td class="xfer-what">
                            <div class="xfer-title">{title}</div>
                            <div class="dim mono-sm xfer-file">{file_line}</div>
                        </td>
                        <td class="nowrap">
                            {t.from.name.clone()} <span class="dim">" → "</span>
                            {t.to.name.clone()}
                        </td>
                        <td class="nowrap xfer-size">
                            {size}
                            {pct.map(|p| view! {
                                <div class="progress xfer-progress" title=format!("{p} %")>
                                    <i style=format!("width:{p}%")></i>
                                </div>
                            })}
                        </td>
                        <td class="xfer-state">
                            <span class=chip>{words}</span>
                            <div class="mini-note dim">{when}</div>
                        </td>
                        <td class="actions">
                            {active.then(|| view! {
                                <button
                                    class="btn ghost sm"
                                    title="end it: its bytes on lmgw are deleted, and both devices are told"
                                    on:click=move |_| ask(t2.clone())
                                >
                                    "Cancel…"
                                </button>
                            })}
                        </td>
                    </tr>
                }
            })
            .collect_view()
    };

    view! {
        <div class="xfer-section">
            <div class="xfer-head">
                <span class="mini-head">"Transfers"</span>
                <span class="dim xfer-sum">
                    {move || list.get().map(|l| head_words(&l)).unwrap_or_default()}
                </span>
            </div>
            {move || err.get().map(|e| view! { <p class="notice warn">"Could not load — " {e}</p> })}
            {move || match list.get() {
                None => view! { <p class="dim devices-empty">"Loading…"</p> }.into_any(),
                Some(l) if l.transfers.is_empty() => view! {
                    <p class="dim wrap devices-empty">
                        "No transfers. An app that declared its app id when it paired offers a "
                        "file to another device of the same app here, and lmgw keeps it until "
                        "that device fetches it."
                    </p>
                }
                .into_any(),
                Some(_) => view! {
                    <table class="data devices-table xfer-table">
                        <thead>
                            <tr>
                                <th>"Transfer"</th>
                                <th>"From → to"</th>
                                <th>"Size"</th>
                                <th>"State"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>{rows}</tbody>
                    </table>
                }
                .into_any(),
            }}
        </div>
        <Modal open=ask_open title="Cancel a transfer">
            {move || {
                asking
                    .get()
                    .map(|t| {
                        let name = if t.title.is_empty() { t.filename.clone() } else { t.title.clone() };
                        view! {
                            <p>
                                <b>
                                    {format!(
                                        "Cancel '{name}' from {} to {}?",
                                        t.from.name,
                                        t.to.name,
                                    )}
                                </b>
                            </p>
                            <p class="notice warn">
                                "Its bytes on lmgw are deleted now, an upload in progress stops, "
                                "and both devices are told it was canceled."
                            </p>
                        }
                    })
            }}
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| ask_open.set(false)>
                    "Keep it"
                </button>
                <button class="btn danger" disabled=move || busy.get() on:click=confirm>
                    "Cancel the transfer"
                </button>
            </ModalFooter>
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::transfer::TransferParty;

    fn t(state: TransferState) -> Transfer {
        Transfer {
            state,
            size: 200,
            received: 50,
            to: TransferParty {
                device_id: 2,
                name: "phone".into(),
            },
            expires_at: "2026-10-28T10:00:00Z".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_transfer_says_its_state_progress_and_time() {
        assert_eq!(
            state_chip(&t(TransferState::Ready)),
            ("chip info", "waiting for phone".to_string())
        );
        assert_eq!(state_chip(&t(TransferState::Failed)).0, "chip err");
        assert_eq!(progress(&t(TransferState::Uploading)), Some(25));
        assert_eq!(progress(&t(TransferState::Ready)), None);
        let mut ended = t(TransferState::Canceled);
        ended.ended_at = Some("2026-10-21T10:00:00Z".into());
        ended.end_reason = Some("device 'phone' was deleted".into());
        let now = logic::parse_utc_secs("2026-10-21T12:00:00Z").unwrap();
        assert_eq!(
            when_words(&ended, now),
            "ended 2h ago: device 'phone' was deleted"
        );
        assert_eq!(when_words(&t(TransferState::Ready), now), "expires in 6d");
        let l = TransferList {
            transfers: vec![t(TransferState::Uploading), ended],
            stored_bytes: 50,
            free_bytes: 2_000_000,
        };
        assert_eq!(
            head_words(&l),
            "1 in progress · 50 B held · 1.9 MiB free for new offers"
        );
    }
}
