//! The Documents lab's reference panel (WP12): what this computer converts,
//! from the same capability report Settings › Documents shows (`doc_tools`),
//! so the form, the panel and the route never disagree: each kind of file
//! with how well it is read here, the programs found and missing, and the
//! tool pool every conversion step shares.

use leptos::prelude::*;
use leptos_router::components::A;
use lmgw_api_types::documents::{Quality, ToolState};

use super::Lab;
use crate::widgets::{ActionRow, InfoRow, PrefGroup};

fn dot(q: Quality) -> &'static str {
    match q {
        Quality::Full => "doclab-dot full",
        Quality::Partial => "doclab-dot part",
        Quality::None => "doclab-dot none",
    }
}

#[component]
pub(super) fn SidePanel(lab: Lab) -> impl IntoView {
    view! {
        <div class="lab-help">
            {move || match lab.caps.get() {
                None => {
                    view! {
                        <div class="empty">
                            {move || lab.caps_err.get().unwrap_or_else(|| "Reading what this computer converts…".into())}
                        </div>
                    }
                        .into_any()
                }
                Some(c) => {
                    let pool = c.pool;
                    view! {
                        <PrefGroup title="What this computer converts">
                            {c
                                .formats
                                .into_iter()
                                .map(|f| {
                                    view! {
                                        <ActionRow title=f.label.clone() sub=f.said.clone()>
                                            <i class=dot(f.quality)></i>
                                        </ActionRow>
                                    }
                                })
                                .collect_view()}
                        </PrefGroup>
                        <PrefGroup title="Programs">
                            {c
                                .tools
                                .into_iter()
                                .map(|t| {
                                    let v = match t.state {
                                        ToolState::Found => t.version.clone().unwrap_or_else(|| "found".into()),
                                        ToolState::Missing => "not installed".into(),
                                        ToolState::ProbeFailed => "did not answer".into(),
                                    };
                                    view! { <InfoRow title=t.label.clone() value=v/> }
                                })
                                .collect_view()}
                        </PrefGroup>
                        <PrefGroup title="Parallel tool runs">
                            <InfoRow title="Slots" value=pool.slots.to_string() mono=true/>
                            <InfoRow title="Running now" value=pool.running.to_string() mono=true/>
                            <InfoRow title="Waiting now" value=pool.waiting.to_string() mono=true/>
                        </PrefGroup>
                    }
                        .into_any()
                }
            }}
            <p class="dim mini-note">
                "Installing a program or choosing a model changes what the route does; "
                <A href="/settings/documents">"Settings › Documents"</A>
                " says how."
            </p>
        </div>
    }
}
