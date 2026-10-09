//! The voice panel's profile chip (personality-profiles design §4.1) and
//! its picker: the thread's `profile_id`, written from the next turn on, the
//! drawer's draft kept in step so the write is no unsaved change there.

use leptos::html;
use leptos::prelude::*;
use serde_json::{json, Value};

use super::super::apply_answer;
use super::super::state::{Note, NoteKind};
use super::Realtime;
use crate::pages::chat_profiles::use_profile_dir;
use crate::widgets::{Popover, Select};

/// The chip and its popover.
#[component]
pub(super) fn ProfileChip(rt: Realtime) -> impl IntoView {
    let dir = use_profile_dir();
    let open = RwSignal::new(false);
    let anchor: NodeRef<html::Button> = NodeRef::new();
    let current = rt.parts.current;
    let id = Memo::new(move |_| current.with(|c| c.as_ref().and_then(|t| t.profile_id)));
    let name = Memo::new(move |_| match (id.get(), dir) {
        (Some(i), Some(d)) => d.get(i).map(|p| p.name).unwrap_or_else(|| "profile".into()),
        _ => "Default".to_string(),
    });
    let pick = RwSignal::new(
        id.get_untracked()
            .map(|i| i.to_string())
            .unwrap_or_default(),
    );
    // The popover opens on what the thread has now.
    Effect::new(move |_| {
        if open.get() {
            pick.set(
                id.get_untracked()
                    .map(|i| i.to_string())
                    .unwrap_or_default(),
            );
            if let Some(d) = dir {
                d.refresh();
            }
        }
    });
    let options = Signal::derive(move || dir.map(|d| d.options()).unwrap_or_default());
    let busy = RwSignal::new(false);
    let save = move |_| {
        let Some(t) = current.get_untracked() else {
            return;
        };
        let want = pick.get_untracked().trim().parse::<i64>().ok();
        let tid = t.id;
        busy.set(true);
        let (draft, status, refresh) = (rt.parts.profile, rt.status, rt.parts.refresh);
        leptos::task::spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                format!("/chat/api/threads/{tid}/settings"),
                &json!({ "profile_id": want }),
            )
            .await;
            busy.try_set(false);
            match res {
                Ok(answer) => {
                    current.try_update(|c| {
                        if let Some(t) = c.as_mut().filter(|t| t.id == tid) {
                            t.profile_id = want;
                            apply_answer(t, &answer);
                        }
                    });
                    draft.try_set(want.map(|i| i.to_string()).unwrap_or_default());
                    refresh.run(());
                    open.try_set(false);
                }
                Err(e) => status.set(Note::new(
                    "profile",
                    NoteKind::Error,
                    format!("the profile could not be saved: {e}"),
                )),
            }
        });
    };
    view! {
        <button
            type="button"
            node_ref=anchor
            class="chip rt-chip"
            data-rt-chip="profile"
            title=move || format!(
                "Personality profile: {} — click to choose another for this conversation (from the next turn)",
                name.get()
            )
            on:mousedown=|ev| ev.prevent_default()
            on:click=move |_| open.update(|o| *o = !*o)
        >
            <span class="rt-chip-k">"Profile"</span>
            <span class="rt-chip-text">{move || name.get()}</span>
        </button>
        <Popover open=open anchor=anchor class="rt-pop" min_width=300>
            <div class="vd-section rt-voice-pick">
                <div class="vd-head">"Profile of this conversation"</div>
                <Select value=pick options=options placeholder="Default"/>
                <div class="rt-pick-row">
                    <span class="vd-note dim">"Applies from the next reply."</span>
                    <button type="button" class="btn primary sm" disabled=move || busy.get() on:click=save>
                        "Save"
                    </button>
                </div>
            </div>
        </Popover>
    }
}
