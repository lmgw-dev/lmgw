//! `/chat/profiles`: the personality-profile editor (personality-profiles
//! design §4.1). The page only frames the kit's [`ProfilesPanel`] (the list
//! beside the editor) inside the page frame's one scroller; `?thread=<id>`,
//! set by the thread drawer's "Edit profiles…", makes Preview, Test and
//! Speak assemble as that thread would and tests on its model. Leaving the
//! page with unsaved edits asks first (the dirty guard).
//!
//! The Chat page's other profile pieces live beside it: [`dir`] (the list
//! the pickers read) and [`pick`] (the drawer's field and the header chip).

mod dir;
mod pick;

use leptos::prelude::*;
use leptos_router::components::A;
use leptos_router::hooks::use_query_map;
use lmgw_ui_kit::profiles::ProfilesPanel;
use serde_json::Value;

pub(super) use dir::{use_profile_dir, ProfileDir};
pub(super) use pick::{ProfileBadge, ProfileField};

use crate::widgets::{use_dirty_guard, PageFrame};

#[component]
pub fn ChatProfiles() -> impl IntoView {
    let thread =
        use_query_map().with_untracked(|q| q.get("thread").and_then(|v| v.parse::<i64>().ok()));
    // The thread's model, for Test: read once; a thread that is gone just
    // leaves the model to the editor's own picker.
    let model = RwSignal::new(None::<String>);
    let ready = RwSignal::new(thread.is_none());
    if let Some(id) = thread {
        leptos::task::spawn_local(async move {
            if let Ok(v) = crate::api::get::<Value>(format!("/chat/api/threads/{id}")).await {
                model.try_set(
                    v["thread"]["model_alias"]
                        .as_str()
                        .filter(|m| !m.is_empty())
                        .map(str::to_string),
                );
            }
            ready.try_set(true);
        });
    }
    // Unsaved edits in the editor: leaving the page asks first (the panel
    // asks itself before it opens another profile).
    let dirty = RwSignal::new(false);
    use_dirty_guard().watch_page("the profile editor", dirty.into());
    let back = match thread {
        Some(id) => format!("/chat?t={id}"),
        None => "/chat".to_string(),
    };
    view! {
        <PageFrame
            title="Chat profiles"
            sub="How a chat's model talks, in text and in voice"
            class="chat-profiles"
            actions=move || {
                view! {
                    <A href=back.clone() attr:class="btn ghost">
                        "Back to chat"
                    </A>
                }
            }
        >
            <Show when=move || ready.get()>
                <ProfilesPanel thread_id=thread test_model=model.get_untracked() dirty=dirty/>
            </Show>
        </PageFrame>
    }
}
