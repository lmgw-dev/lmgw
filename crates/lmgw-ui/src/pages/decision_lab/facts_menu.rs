//! The header bar's Facts button: in a pane too narrow for the model's
//! facts beside the title (`.dl-facts-inline` hides there, app.css), the
//! same pills in a popover, so the header keeps one line.

use leptos::html;
use leptos::prelude::*;

use super::head::{facts, FactPills};
use super::page::Lab;
use crate::widgets::{Ico, Icon, Popover};

#[component]
pub(super) fn FactsMenu(lab: Lab) -> impl IntoView {
    let open = RwSignal::new(false);
    let btn: NodeRef<html::Button> = NodeRef::new();
    let n = move || lab.model().map_or(0, |m| facts(&m).len());
    // Named: a `>` inside a `view!` attribute closure ends the tag.
    let any = move || n() != 0;
    // A warning or an error among them shows on the button too.
    let tone = move || {
        lab.model().map_or("", |m| {
            let all = facts(&m);
            if all.iter().any(|f| f.tone == super::head::Tone::Err) {
                "err"
            } else if all.iter().any(|f| f.tone == super::head::Tone::Warn) {
                "warn"
            } else {
                ""
            }
        })
    };
    view! {
        <Show when=any>
            <button
                type="button"
                class=move || format!("btn flat sm dl-facts-btn {}", tone())
                node_ref=btn
                title="What the picked model is: its header's facts"
                aria-haspopup="dialog"
                aria-expanded=move || open.get().to_string()
                on:click=move |_| open.update(|o| *o = !*o)
            >
                <Icon ico=Ico::Info/>
                {move || format!("Facts {}", n())}
            </button>
        </Show>
        <Popover open=open anchor=btn class="dl-facts-pop">
            <div class="dl-facts">
                <FactPills lab=lab/>
            </div>
        </Popover>
    }
}
