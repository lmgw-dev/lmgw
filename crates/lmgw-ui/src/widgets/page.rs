//! The page frame every `.page` route renders through (UX plan §1.2).
//!
//! The content pane itself never scrolls. A page is a column: a top strip
//! (title, subtitle, tabs, actions, and an optional toolbar) that stays put,
//! one body that is the page's single scroller, and an optional footer that is
//! always on screen — so a page's head and its Save never scroll away, at any
//! window height.

use leptos::prelude::*;

/// How the body scrolls.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum PageMode {
    /// The body is the one scroller: a document.
    #[default]
    Doc,
    /// The body does not scroll; it holds one `.fill-pane` that does, with
    /// fixed strips above it — a long table under its filters.
    Fill,
    /// A rail and a pane side by side, each scrolling on its own.
    Split,
}

impl PageMode {
    fn class(self) -> &'static str {
        match self {
            Self::Doc => "doc",
            Self::Fill => "fill",
            Self::Split => "split",
        }
    }
}

/// The spacing set (app.css `.density-*`).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Density {
    #[default]
    Dense,
    /// No page is airy since the agent pages went dense (UX plan §4); the chat
    /// transcript, the one airy surface, sets the class on its own column.
    #[allow(dead_code)]
    Airy,
}

impl Density {
    fn class(self) -> &'static str {
        match self {
            Self::Dense => "density-dense",
            Self::Airy => "density-airy",
        }
    }
}

/// `<div class="page {density} {mode} {class}">` → `.page-top` (`.page-head`,
/// `.page-toolbar`?) · `.page-body` · `.page-foot`?
///
/// `head_extra` sits after the subtitle (tabs, badges); `actions` is pushed to
/// the right of the head and wraps under it when the pane is narrow.
#[component]
pub fn PageFrame(
    #[prop(into)] title: TextProp,
    /// One line; ellipsized when the head is short of room, full text in the
    /// tooltip.
    #[prop(optional, into)]
    sub: Option<TextProp>,
    #[prop(optional)] mode: PageMode,
    #[prop(optional)] density: Density,
    /// Extra classes on the root, e.g. `usage` for page-scoped rules.
    #[prop(optional)]
    class: &'static str,
    #[prop(optional, into)] head_extra: Option<ViewFn>,
    #[prop(optional, into)] actions: Option<ViewFn>,
    #[prop(optional, into)] toolbar: Option<ViewFn>,
    #[prop(optional, into)] footer: Option<ViewFn>,
    children: Children,
) -> impl IntoView {
    let root = format!("page {} {} {class}", density.class(), mode.class());
    let sub = sub.map(|s| {
        let tip = s.clone();
        view! { <span class="sub" title=move || tip.get().to_string()>{move || s.get()}</span> }
    });
    view! {
        <div class=root.trim_end().to_string()>
            <div class="page-top">
                <header class="page-head">
                    <h1>{move || title.get()}</h1>
                    {sub}
                    {head_extra.map(|v| v.run())}
                    {actions.map(|a| view! { <div class="actions">{a.run()}</div> })}
                </header>
                {toolbar.map(|t| view! { <div class="page-toolbar">{t.run()}</div> })}
            </div>
            <div class="page-body">{children()}</div>
            {footer.map(|f| view! { <div class="page-foot">{f.run()}</div> })}
        </div>
    }
}
