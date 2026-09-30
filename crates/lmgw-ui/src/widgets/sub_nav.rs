//! Tabs that are routes (UX plan §2 #4, §4 URLs): a path segment picks the
//! view, so a tab is a link — bookmarkable, reloadable, and Back goes to the
//! tab before.

// The area phases adopt these page by page; until then only the design
// sample shows them.
#![allow(dead_code)]

use leptos::prelude::*;
use leptos_router::components::A;
use leptos_router::hooks::use_location;

/// A count pill's meaning (UX plan §4 counts): a size, waiting on you, broken.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Hash)]
pub enum Tone {
    #[default]
    Neutral,
    Attn,
    Bad,
}

impl Tone {
    pub fn class(self) -> &'static str {
        match self {
            Self::Neutral => "count",
            Self::Attn => "count attn",
            Self::Bad => "count bad",
        }
    }
}

/// One tab. The default tab of a page (`/traffic`) needs `exact`, or it
/// would count as active on every sub-path (`/traffic/conversations`) too.
#[derive(Clone, PartialEq, Debug)]
pub struct NavTab {
    pub label: &'static str,
    pub href: String,
    pub count: Option<String>,
    pub tone: Tone,
    pub exact: bool,
}

#[allow(dead_code)] // the area phases build their tabs with these
impl NavTab {
    pub fn new(label: &'static str, href: impl Into<String>) -> Self {
        Self {
            label,
            href: href.into(),
            count: None,
            tone: Tone::Neutral,
            exact: false,
        }
    }

    pub fn count(mut self, count: impl ToString) -> Self {
        self.count = Some(count.to_string());
        self
    }

    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    pub fn exact(mut self) -> Self {
        self.exact = true;
        self
    }
}

/// `<nav class="seg sub-nav">` of router links, styled like the segmented
/// control. The active tab is the router's `aria-current="page"`.
///
/// Every other tab leads back to its view as it was left: its href carries
/// the query string that path was last shown with (filters, range, sort), so
/// a round trip through another tab does not reset them.
#[component]
pub fn SubNav(#[prop(into)] tabs: Signal<Vec<NavTab>>) -> impl IntoView {
    let loc = use_location();
    view! {
        <nav class="seg sub-nav">
            // Keyed on everything shown: a count that changes re-renders its
            // tab (a keyed list never updates an entry in place).
            <For each=move || tabs.get() key=|t| (t.href.clone(), t.count.clone(), t.tone) let:t>
                {
                    let base = t.href.clone();
                    // The tab of the page being shown stays bare: the router
                    // marks it current by comparing the path.
                    let href = move || {
                        if loc.pathname.with(|p| *p == base) {
                            base.clone()
                        } else {
                            crate::url_state::with_last_query(&base)
                        }
                    };
                    view! {
                        <A href=href exact=t.exact attr:class="seg-btn">
                            {t.label}
                            {t.count.clone().map(|c| view! { <span class=t.tone.class()>{c}</span> })}
                        </A>
                    }
                }
            </For>
        </nav>
    }
}
