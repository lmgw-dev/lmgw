//! The conversation's header bar (the modern design, docs/design/ui-rebuild/
//! plan.md "Chat"): the thread's title, and under it its model as the
//! picker, the way a libadwaita header shows a title and its subtitle; the
//! thread's marks (the folder's model, admin, reasoning, MCP, knowledge,
//! profile, a stopped feed), the way back to an ongoing conversation, the
//! download menu, and in the narrow layout the drawers' buttons and New
//! chat. It is a window header (`data-window-header`) with the host's window
//! buttons ([`crate::ChatWindow`]) at its ends; the start buttons give way
//! to the thread list's head while the list is open beside it (chat.css).

use leptos::prelude::*;
use lmgw_ui_kit::widgets::{Drawer, Ico, Icon, ModelPicker};

use super::narrow;
use super::ChatThread;
use crate::host::{ChatWindow, Place};

/// What the head shows of the open thread, and what its buttons do.
#[derive(Clone, Copy)]
pub(crate) struct HeadEnv {
    pub(crate) narrow: Memo<bool>,
    pub(crate) threads_drawer: Drawer,
    pub(crate) settings_drawer: Drawer,
    /// The list's count, on the narrow list button.
    pub(crate) badge: Signal<String>,
    pub(crate) new_label: Signal<String>,
    pub(crate) on_new: Callback<&'static str>,
    pub(crate) place: Place,
}

/// The window's buttons at one end, where the host has them.
fn window_end(window: &Option<ChatWindow>, start: bool) -> Option<AnyView> {
    window.as_ref().map(|w| {
        if start {
            view! { <span class="chat-wstart">{w.start.run()}</span> }.into_any()
        } else {
            w.end.run()
        }
    })
}

/// The narrow layout's list button and New chat, first in the head.
fn narrow_start(env: HeadEnv) -> impl IntoView {
    view! {
        <Show when=move || env.narrow.get()>
            <narrow::DrawerButton
                drawer=env.threads_drawer
                other=env.settings_drawer
                label="Conversations"
                icon=narrow::ICON_LIST
                badge=env.badge
            />
            <narrow::NewChatButton
                label=env.new_label
                on_new=Callback::new(move |()| env.on_new.run("chat"))
            />
        </Show>
    }
}

/// The head while no conversation is open: the Chat's name, the list's
/// button in the narrow layout, and the window buttons.
#[component]
pub(crate) fn EmptyHead(env: HeadEnv, window: Option<ChatWindow>) -> impl IntoView {
    view! {
        <header class="chat-head chat-head-empty" data-window-header>
            {window_end(&window, true)}
            {narrow_start(env)}
            <div class="chat-title">
                <h1>"Chat"</h1>
            </div>
            {window_end(&window, false)}
        </header>
    }
}

/// The open conversation's head.
#[component]
pub(crate) fn ChatHead(
    env: HeadEnv,
    window: Option<ChatWindow>,
    current: RwSignal<Option<ChatThread>>,
    current_id: Memo<Option<i64>>,
    model_sel: RwSignal<String>,
    #[prop(into)] picker_held: Signal<bool>,
    /// The folder's model holds the picker (a new chat waits in a folder).
    #[prop(into)]
    held: Signal<bool>,
    is_admin: Memo<bool>,
    mcp_names: Memo<Vec<String>>,
    /// The ongoing folder's name, while the way back to it is offered.
    #[prop(into)]
    back: Signal<Option<String>>,
    on_back: Callback<()>,
) -> impl IntoView {
    let place = env.place;
    let title = Memo::new(move |_| {
        current
            .with(|c| c.as_ref().map(|t| t.title.trim().to_string()))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "New chat".to_string())
    });
    view! {
        <header class="chat-head" data-window-header>
            {window_end(&window, true)}
            {narrow_start(env)}
            <div class="chat-title">
                <h1 title=move || title.get()>{move || title.get()}</h1>
                <ModelPicker
                    value=model_sel
                    tasks=&["chat"]
                    recent_key="chat"
                    disabled=picker_held
                    empty_hint=move || {
                        // The dashboard's pages, where the host has them.
                        let (models, providers) = (place.href("/models"), place.href("/upstreams"));
                        let both = models.is_some() && providers.is_some();
                        view! {
                            {models.map(|h| view! { <a href=h>"Add a model"</a> })}
                            {both.then_some(" · ")}
                            {providers.map(|h| view! { <a href=h>"Add a cloud provider"</a> })}
                        }
                    }
                />
            </div>
            <Show when=move || held.get()>
                <span class="type-badge held-badge" title=crate::chat_sync::HELD>
                    "folder's model"
                </span>
            </Show>
            <Show when=move || is_admin.get()>
                <span class="type-badge" title="self-admin tools attached">
                    "admin"
                </span>
            </Show>
            {move || {
                current
                    .with(|c| c.as_ref().and_then(crate::chat_reasoning::reasoning_badge))
                    .map(|b| {
                        view! {
                            <span
                                class="type-badge"
                                title="This thread's reasoning overrides — Thread settings → Reasoning"
                            >
                                {b}
                            </span>
                        }
                    })
            }}
            {move || {
                let names = mcp_names.get();
                (!names.is_empty())
                    .then(|| {
                        view! {
                            <span
                                class="type-badge mcp-badge"
                                title=format!("MCP servers attached: {}", names.join(", "))
                                aria-label=format!("{} MCP servers attached", names.len())
                            >
                                <Icon ico=Ico::Plug/>
                                {names.len()}
                            </span>
                        }
                    })
            }}
            {move || {
                current
                    .with(|c| c.as_ref().and_then(crate::chat_knowledge::kb_badge))
                    .map(|b| {
                        view! {
                            <span
                                class="type-badge"
                                title="Knowledge bases on this thread — Thread settings → Knowledge"
                            >
                                {b}
                            </span>
                        }
                    })
            }}
            <crate::chat_profiles::ProfileBadge current=current/>
            <crate::chat_live::FeedStopped/>
            {move || {
                back.get()
                    .map(|name| {
                        view! {
                            <narrow::BackButton name=name on_back=move |_| on_back.run(())/>
                        }
                    })
            }}
            <div class="chat-head-end">
                <Show when=move || current_id.get().is_some()>
                    <crate::chat_export::ExportMenu thread=current_id icon=true/>
                </Show>
                <Show when=move || env.narrow.get()>
                    <narrow::DrawerButton
                        drawer=env.settings_drawer
                        other=env.threads_drawer
                        label="Thread settings"
                        icon=narrow::ICON_SETTINGS
                        class="chat-settings-btn"
                    />
                </Show>
            </div>
            {window_end(&window, false)}
        </header>
    }
}
