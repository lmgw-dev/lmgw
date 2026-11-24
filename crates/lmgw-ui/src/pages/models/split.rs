//! The list beside the detail (modern-sample.html) on the shared split view
//! ([`crate::widgets::nav_split`]): side by side in a pane at least
//! [`NARROW_BELOW`] px wide, the list's width kept as
//! `lmgw.ui.models.list.width`; narrower, the list, or once a row is opened
//! its detail with a back button.
//!
//! Page keys: Ctrl+F goes to the search, Ctrl+S saves the detail.

use leptos::prelude::*;

use super::list::ListPane;
use super::route::{Sel, LIST_KEYS};
use super::PageCtx;
use crate::widgets::list_pane::ListRefs;
pub(super) use crate::widgets::nav_split::SaveKey;
use crate::widgets::nav_split::{provide_save_key, NavSplit, SplitConf, SplitState};

/// Below this pane width the view is one pane at a time. A 1024 px window
/// at 125 % leaves the pane about 770 px: one pane, where a list of 280 px
/// would leave the detail too little for its rows.
pub(super) const NARROW_BELOW: f64 = 800.0;

const CONF: SplitConf = SplitConf {
    class: "models-split",
    width_key: "models.list.width",
    list_width: (300, 38, 440),
    detail_min: 440,
    narrow_below: NARROW_BELOW,
    handle_label: "Resize the list",
};

#[component]
pub(super) fn ModelsSplit(page: PageCtx, sel: Memo<Option<Sel>>) -> impl IntoView {
    let split = SplitState::new(CONF);
    let narrow = split.narrow;
    let refs = ListRefs::new();
    let save_key = provide_save_key();
    let open_row = Callback::new(move |(s, replace): (Sel, bool)| {
        page.nav.open(&s.path(), LIST_KEYS, replace);
    });
    let back = Callback::new(move |()| page.nav.back("/models", LIST_KEYS, refs));

    view! {
        <NavSplit
            split=split
            has_sel=Signal::derive(move || sel.with(Option::is_some))
            save_key=save_key
            search=refs.search
            // A narrow window showing a detail goes back to the list first.
            search_off=Signal::derive(move || narrow.get() && sel.with(Option::is_some))
            list=ViewFn::from(move || {
                view! { <ListPane page=page sel=sel narrow=narrow refs=refs open_row=open_row/> }
            })
            detail=ViewFn::from(move || {
                view! { <super::detail::DetailPane page=page sel=sel narrow=narrow back=back/> }
            })
        />
    }
}
