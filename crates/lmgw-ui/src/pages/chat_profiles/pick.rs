//! The profile controls of the Chat page: the settings drawer's (and a
//! folder's defaults') picker, the thread header's chip, and the voice
//! panel's chip with its picker.

use leptos::prelude::*;

use super::dir::use_profile_dir;
use crate::pages::chat::ChatThread;
use crate::widgets::Select;

/// The Profile field of the thread drawer and of a folder's defaults.
/// `value` is the draft (`""` none, else an id). `thread` is the open
/// thread, for the editor's link to open on it; a folder has none.
/// `folder`: the field is a folder's default, where none means "Settings →
/// Chat's profile for new threads", not "no profile".
#[component]
pub(in crate::pages) fn ProfileField(
    value: RwSignal<String>,
    #[prop(default = Signal::stored(None))] thread: Signal<Option<i64>>,
    #[prop(optional)] folder: bool,
) -> impl IntoView {
    let dir = use_profile_dir();
    let options = Signal::derive(move || dir.map(|d| d.options()).unwrap_or_default());
    let href = move || match thread.get() {
        Some(id) => format!("/chat/profiles?thread={id}"),
        None => "/chat/profiles".to_string(),
    };
    let hint = move || {
        let picked = dir.and_then(|d| value.with(|v| d.of_value(v)));
        match picked {
            Some(p) if p.persona.trim().is_empty() => format!(
                "'{}' sets no persona: the system prompt below still applies.",
                p.name
            ),
            Some(p) => format!(
                "'{}' shapes this chat's system message, reasoning and voice.",
                p.name
            ),
            None if folder => folder_default_hint(dir.and_then(|d| d.default_profile())),
            None => {
                "Default: no profile, the system prompt and the Chat's settings alone.".to_string()
            }
        }
    };
    view! {
        <div class="field" data-profile-field="">
            <label>"Profile"</label>
            <Select value=value options=options placeholder="Default"/>
            <div class="field-hint">
                {hint} " " <a class="link-btn" href=href data-dock-pick="">"Edit profiles…"</a>
            </div>
        </div>
    }
}

/// What a folder's "Default" (`profile_id` null) really does: its new
/// threads, and its current thread when the change is applied, take
/// Settings → Chat's profile for new threads, `chat_profile` (what a new
/// thread outside a folder takes too).
fn folder_default_hint(chat_profile: Option<lmgw_api_types::chat_profiles::Profile>) -> String {
    match chat_profile {
        Some(p) => format!(
            "Default: this folder names no profile, so its new threads take Settings → Chat's \
             profile for new threads, now '{}'.",
            p.name
        ),
        None => "Default: this folder names no profile, and Settings → Chat's profile for new \
                 threads is empty, so its new threads start with none."
            .to_string(),
    }
}

/// The thread header's chip: the profile's name, shown when one is set and
/// the profile list holds it. A click opens the thread settings, where the
/// picker is.
#[component]
pub(in crate::pages) fn ProfileBadge(current: RwSignal<Option<ChatThread>>) -> impl IntoView {
    let dir = use_profile_dir();
    let picked = Memo::new(move |_| current.with(|c| c.as_ref().and_then(|t| t.profile_id)));
    move || {
        let id = picked.get()?;
        // An id the list does not hold (a profile deleted since the thread
        // was read) is none, as the gateway assembles it: no chip.
        let name = dir.and_then(|d| d.get(id)).map(|p| p.name)?;
        Some(view! {
            <button
                type="button"
                class="type-badge as-btn"
                data-profile-badge=""
                title="This chat's personality profile. Click to open the thread settings, where it is picked."
                on:click=move |_| open_thread_settings()
            >
                {name}
            </button>
        })
    }
}

/// Unfold the thread settings drawer: its rail is the right-hand dock's
/// button (a drawer already open has none, so nothing happens).
fn open_thread_settings() {
    if let Ok(Some(rail)) = document().query_selector(".dock.right > .dock-rail") {
        if let Ok(b) = wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(rail) {
            b.click();
        }
    }
}
