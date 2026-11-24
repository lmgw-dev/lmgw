//! The detail pane's frame for a run, a comparison and the status page: the
//! header bar (back in a narrow window, the actions, the menu), one scroller
//! under it, and a hero with the title, the line under it and its chips.
//! The body is a single column as wide as the pane allows: the charts and
//! the delta tables need the width more than a second column would.

use leptos::prelude::*;

use crate::widgets::nav_split::{track_scroll, DetailHead};
use crate::widgets::{Ico, Icon, MenuItem};

/// What the frame reads from the page: one pane at a time, and the way back
/// to the list.
#[derive(Clone, Copy)]
pub struct BnPane {
    pub narrow: Memo<bool>,
    pub back: Callback<()>,
}

#[component]
pub fn BnFrame(
    #[prop(into)] title: Signal<String>,
    #[prop(into)] sub: Signal<String>,
    #[prop(default = Ico::Clock)] glyph: Ico,
    /// Chips under the line (the status).
    #[prop(optional, into)]
    chips: Option<ViewFn>,
    #[prop(optional, into)] actions: Option<ViewFn>,
    #[prop(optional, into)] more: Option<Signal<Vec<MenuItem>>>,
    children: Children,
) -> impl IntoView {
    let pane = expect_context::<BnPane>();
    let scrolled = RwSignal::new(false);
    let head = match (actions, more) {
        (Some(a), Some(m)) => view! {
            <DetailHead narrow=pane.narrow back=pane.back back_label="Back to the runs" title=title scrolled=scrolled actions=a more=m/>
        }
        .into_any(),
        (Some(a), None) => view! {
            <DetailHead narrow=pane.narrow back=pane.back back_label="Back to the runs" title=title scrolled=scrolled actions=a/>
        }
        .into_any(),
        (None, Some(m)) => view! {
            <DetailHead narrow=pane.narrow back=pane.back back_label="Back to the runs" title=title scrolled=scrolled more=m/>
        }
        .into_any(),
        (None, None) => view! {
            <DetailHead narrow=pane.narrow back=pane.back back_label="Back to the runs" title=title scrolled=scrolled/>
        }
        .into_any(),
    };
    view! {
        {head}
        <div
            class="detail-scroll mdetail-scroll bn-scroll"
            tabindex="-1"
            on:scroll=move |ev| track_scroll(&ev, scrolled)
        >
            <div class="bn-flow">
                <div class="detail-hero">
                    <div class="glyph"><Icon ico=glyph/></div>
                    <div class="hero-text">
                        <h1>{move || title.get()}</h1>
                        <div class="sub" title=move || sub.get()>{move || sub.get()}</div>
                        {chips.map(|c| view! { <div class="chips">{move || c.run()}</div> })}
                    </div>
                </div>
                {children()}
            </div>
        </div>
    }
}
