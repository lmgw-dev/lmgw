//! The kept history in the results column (labs-store design §1.5, §2.9):
//! each group through the lab's own card, newest first; the files no row
//! names, with their removal; and at the end the line that reads the next
//! older page when it scrolls into view, "N older" until none are left, or
//! why the last one did not load, with Retry.

use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use super::client::{reads_on, LabHistory, Load};
use crate::fmt::{grouped, human_bytes};
use crate::widgets::ConfirmButton;

/// The history. `card` renders one group by its id; the card reads the group
/// from the history itself, so a run landing re-renders that card only.
#[component]
pub(in crate::pages) fn HistoryList(
    history: LabHistory,
    card: Callback<i64, AnyView>,
) -> impl IntoView {
    let ids = Memo::new(move |_| {
        history
            .groups
            .with(|g| g.iter().map(|g| g.id).collect::<Vec<_>>())
    });
    let sentinel: NodeRef<html::Div> = NodeRef::new();
    let seen = RwSignal::new(false);

    // The end of the list in view: read on while older groups are left.
    Effect::new(move |_| {
        let Some(el) = sentinel.get() else { return };
        let cb = Closure::<dyn FnMut(js_sys::Array)>::new(move |entries: js_sys::Array| {
            let hit = entries
                .iter()
                .filter_map(|e| e.dyn_into::<web_sys::IntersectionObserverEntry>().ok())
                .any(|e| e.is_intersecting());
            seen.try_set(hit);
        });
        let Ok(io) = web_sys::IntersectionObserver::new(cb.as_ref().unchecked_ref()) else {
            return;
        };
        io.observe(&el);
        let held = StoredValue::new_local(Some((io, cb)));
        on_cleanup(move || {
            if let Some((io, _cb)) = held.try_update_value(Option::take).flatten() {
                io.disconnect();
            }
        });
    });
    Effect::new(move |_| {
        let failed = history.older_failed.with(Option::is_some);
        if reads_on(seen.get(), history.more.get(), history.older.get(), failed) {
            history.load_older();
        }
    });

    let orphans = move || {
        history
            .usage
            .get()
            .filter(|u| u.orphan_files > 0)
            .map(|u| {
                let text = format!(
                    "{} {} ({}) in the artifacts folder belong to no group: a crash between a write and its commit leaves them.",
                    grouped(u.orphan_files),
                    if u.orphan_files == 1 { "file" } else { "files" },
                    human_bytes(u.orphan_bytes),
                );
                view! {
                    <div class="notice warn lh-orphans">
                        {text}
                        " "
                        <ConfirmButton
                            label="Remove them"
                            confirm="Remove the files?"
                            title="Delete the artifact files no group or run names (every lab's)"
                            on_confirm=Callback::new(move |()| history.delete_orphans())
                        />
                    </div>
                }
            })
    };

    view! {
        {move || match history.load.get() {
            Load::Failed(e) => {
                view! {
                    <div class="notice err">
                        {format!("⚠ The history did not load: {e}")}
                        " "
                        <button class="btn ghost sm" on:click=move |_| history.reload()>
                            "Retry"
                        </button>
                    </div>
                }
                    .into_any()
            }
            Load::Loading if ids.with(Vec::is_empty) => {
                view! { <div class="dim lh-loading">"Reading the history…"</div> }.into_any()
            }
            _ => ().into_any(),
        }}
        {orphans}
        <For each=move || ids.get() key=|id| *id let:id>
            {card.run(id)}
        </For>
        <div class="lh-more dim" node_ref=sentinel>
            {move || {
                let older = history.older.get();
                if let Some(e) = history.older_failed.get() {
                    return Some(
                        view! {
                            <span class="lh-older-failed">
                                {format!("couldn't load older groups: {e}")}
                                " · "
                                <button
                                    class="btn ghost sm"
                                    title="Read the next older page again"
                                    on:click=move |_| history.load_older()
                                >
                                    "Retry"
                                </button>
                            </span>
                        }
                            .into_any(),
                    );
                }
                (older > 0)
                    .then(|| {
                        view! {
                            <button
                                class="btn ghost sm"
                                disabled=move || history.more.get()
                                on:click=move |_| history.load_older()
                            >
                                {move || {
                                    if history.more.get() {
                                        format!("{} older · reading…", grouped(older))
                                    } else {
                                        format!("{} older · show them", grouped(older))
                                    }
                                }}
                            </button>
                        }
                            .into_any()
                    })
            }}
        </div>
    }
}
