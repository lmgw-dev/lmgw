//! An account's sessions on the Accounts card (headless server design
//! §5.2): where it is signed in, each with End, asked before it ends.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{AccountDone, SessionList};
use serde_json::json;

use crate::widgets::{use_toasts, ConfirmButton};

/// An account's sessions, each with End.
#[component]
pub(super) fn Sessions(id: i64, refresh: Callback<()>) -> impl IntoView {
    let toasts = use_toasts();
    let tick = RwSignal::new(0u32);
    let list = LocalResource::new(move || {
        tick.get();
        async move { super::op::<SessionList>("account_sessions", json!({ "id": id })).await }
    });
    let end = move |sid: i64, this: bool| {
        spawn_local(async move {
            match super::op::<AccountDone>("session_end", json!({ "id": sid })).await {
                Ok(_) if this => {
                    let _ = crate::session::sign_out().await;
                }
                Ok(d) => {
                    toasts.ok(d.message);
                    tick.update(|t| *t = t.wrapping_add(1));
                    refresh.run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        })
    };
    view! {
        <Suspense fallback=|| view! { <span class="dim">"…"</span> }>
            {move || {
                list.get().map(|r| match r {
                    Err(e) => view! { <div class="notice err">{e.to_string()}</div> }.into_any(),
                    Ok(l) if l.sessions.is_empty() => {
                        view! { <p class="dim">"no sessions"</p> }.into_any()
                    }
                    Ok(l) => view! {
                        <table class="data">
                            <thead>
                                <tr>
                                    <th>"Signed in"</th>
                                    <th>"Last seen (to the hour)"</th>
                                    <th>"Ends"</th>
                                    <th>"From"</th>
                                    <th>"Browser"</th>
                                    <th></th>
                                </tr>
                            </thead>
                            <tbody>
                                {l.sessions
                                    .into_iter()
                                    .map(|s| {
                                        let (sid, this) = (s.id, s.this_browser);
                                        view! {
                                            <tr>
                                                <td class="nowrap">{super::when(&s.created_at)}</td>
                                                <td class="nowrap">{super::when(&s.last_seen_at)}</td>
                                                <td class="nowrap">{super::when(&s.expires_at)}</td>
                                                <td class="mono-sm">{s.peer}</td>
                                                <td class="clip dim" title=s.user_agent.clone()>
                                                    {s.user_agent.clone()}
                                                    {this.then(|| view! { " " <span class="chip">"this browser"</span> })}
                                                </td>
                                                <td>
                                                    <ConfirmButton
                                                        label="End"
                                                        confirm=if this { "Sign out here?" } else { "End it?" }
                                                        title=if this {
                                                            "end this browser's session: it signs out"
                                                        } else {
                                                            "end this session: that browser signs in again"
                                                        }
                                                        on_confirm=Callback::new(move |()| end(sid, this))
                                                    />
                                                </td>
                                            </tr>
                                        }
                                    })
                                    .collect_view()}
                            </tbody>
                        </table>
                    }
                    .into_any(),
                })
            }}
        </Suspense>
    }
}
