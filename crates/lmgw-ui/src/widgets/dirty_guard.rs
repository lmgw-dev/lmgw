//! The unsaved-edits guard is the kit's; the router-bound half is here: the
//! path a page registers under, and the navigation after "Leave and discard".

use leptos::prelude::*;
use leptos_router::hooks::{use_location, use_navigate};
use leptos_router::NavigateOptions;

pub use lmgw_ui_kit::widgets::dirty_guard::try_use_dirty_guard;

/// Install the guard. Call once, in `App`; render [`DirtyGuardHost`] inside
/// the router.
pub fn provide_dirty_guard() {
    lmgw_ui_kit::widgets::dirty_guard::provide_dirty_guard(|| {
        use_location().pathname.get_untracked()
    });
}

/// The capture-phase link and unload listeners, and the question.
#[component]
pub fn DirtyGuardHost() -> impl IntoView {
    let navigate = use_navigate();
    let go = Callback::new(move |to: String| navigate(&to, NavigateOptions::default()));
    view! { <lmgw_ui_kit::widgets::dirty_guard::DirtyGuardHost navigate=go/> }
}
