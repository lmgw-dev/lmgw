//! The narrow layout (chat-plane design, "WP3b: the lmgw-chat-ui crate"):
//! below [`NARROW_PX`] of the Chat's own width (a Firefox sidebar opens at
//! 338 px) the thread list and the thread's settings have no rails; they are
//! drawers over the conversation (`lmgw_ui_kit::widgets::Drawer`), opened from
//! buttons in the Chat's head, and the conversation takes the whole column.
//! The head has New chat too, beside the list's button, so a new chat is one
//! press away (chat-plane design §9.10). It works from 280 px up.

use leptos::prelude::*;
use lmgw_ui_kit::widgets::Drawer;

/// The Chat's width below which its side panels are drawers. Wider, the
/// split panes fold to rails and lay a panel over the conversation as they
/// do on the dashboard.
pub(crate) const NARROW_PX: f64 = 520.0;

/// Whether a Chat `w` px wide takes the narrow layout. Not measured yet
/// (0, the first frame) is not narrow.
pub(crate) fn narrow_at(w: f64) -> bool {
    w > 0.0 && w < NARROW_PX
}

/// The list's icon: three lines.
pub(crate) const ICON_LIST: &str = "M2.5 4h11 M2.5 8h11 M2.5 12h11";
/// The settings' icon: three sliders.
pub(crate) const ICON_SETTINGS: &str =
    "M2.5 4h7 M12.5 4h1 M11 2.5v3 M2.5 8h2 M7.5 8h6 M6 6.5v3 M2.5 12h5 M10.5 12h3 M9 10.5v3";

/// New chat's icon: a plus.
pub(crate) const ICON_NEW: &str = "M8 3v10 M3 8h10";
/// The way back's icon: an arrow turning back.
pub(crate) const ICON_BACK: &str = "M6 3.5 2.5 7 6 10.5 M2.5 7h7a4 4 0 0 1 0 8h-1";

/// The narrow head's New chat: the list's New chat, one press away. Its
/// label says where the chat goes ("New chat in Kai").
#[component]
pub(crate) fn NewChatButton(
    #[prop(into)] label: Signal<String>,
    on_new: Callback<()>,
) -> impl IntoView {
    view! {
        <button
            type="button"
            class="btn ghost chat-drawer-btn chat-new-btn"
            title=move || label.get()
            aria-label=move || label.get()
            on:click=move |_| on_new.run(())
        >
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_NEW></path></svg>
        </button>
    }
}

/// "Back to <folder>": opens an ongoing folder's current thread from
/// another one (chat-plane design §9.10). Narrow, the icon and the name,
/// which gives way first.
#[component]
pub(crate) fn BackButton(
    name: String,
    on_back: impl Fn(web_sys::MouseEvent) + 'static,
) -> impl IntoView {
    let label = format!("Back to {name}, its current conversation");
    view! {
        <button
            type="button"
            class="btn ghost sm chat-back"
            title=label.clone()
            aria-label=label
            on:click=on_back
        >
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d=ICON_BACK></path></svg>
            <span class="chat-back-to" aria-hidden="true">"Back to "</span>
            <span class="chat-back-name" aria-hidden="true">{name}</span>
        </button>
    }
}

/// A head button that opens `drawer` (and puts `other` away), or puts it
/// away again. Its press is kept from the pane under it, which would put an
/// open drawer away first and have the click open it again.
#[component]
pub(crate) fn DrawerButton(
    drawer: Drawer,
    other: Drawer,
    label: &'static str,
    icon: &'static str,
    /// The drawer's count, beside the icon (the list's "5 of 23").
    #[prop(optional, into)]
    badge: Option<Signal<String>>,
    #[prop(optional)] class: &'static str,
) -> impl IntoView {
    view! {
        <button
            type="button"
            class=format!("btn ghost chat-drawer-btn {class}")
            title=label
            aria-label=label
            aria-expanded=move || drawer.open.get().to_string()
            data-drawer=label
            on:pointerdown=|ev| ev.stop_propagation()
            on:click=move |_| {
                other.open.set(false);
                drawer.open.update(|o| *o = !*o);
            }
        >
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d=icon></path></svg>
            {badge.map(|b| view! { <span class="count">{move || b.get()}</span> })}
        </button>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_narrow_layout_is_a_sidebar_s_width() {
        // Firefox opens an extension sidebar at 338 px; the layout works
        // from 280 px.
        assert!(narrow_at(338.0));
        assert!(narrow_at(280.0));
        assert!(narrow_at(519.0));
        assert!(!narrow_at(520.0));
        assert!(!narrow_at(1200.0));
        // Not measured yet: the wide layout, as the split panes decide.
        assert!(!narrow_at(0.0));
    }
}
