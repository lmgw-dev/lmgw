//! The page frame every `.page` route renders through (UX plan §1.2).
//!
//! The content pane itself never scrolls. A page is a column: a top strip
//! (title, subtitle, tabs, actions, and an optional toolbar) that stays put,
//! one body that is the page's single scroller, and an optional footer that is
//! always on screen — so a page's head and its Save never scroll away, at any
//! window height.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::frame::{End, WindowButtons};

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

/// The spacing set (kit.css `.density-*`; dense is also the `:root` default).
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

/// `<div class="page {density} {mode} {class}">` → `.page-top`
/// (`header.page-head`, `.page-toolbar`?) · `.page-body` · `.page-foot`?
///
/// The head is the page's header bar (48 px, `--head-h`): the title with the
/// subtitle under it, `head_extra` after them (tabs, badges), `actions` at
/// the end, and the window buttons after those when the shell draws no frame
/// of its own (crate::frame). It is marked `data-window-header`, so it moves
/// the window there. On a pane too narrow for one line its contents wrap,
/// the window buttons staying at the top right.
#[component]
pub fn PageFrame(
    #[prop(into)] title: TextProp,
    /// One line; ellipsized when the head is short of room, full text in the
    /// tooltip.
    #[prop(optional, into)]
    sub: Option<TextProp>,
    /// The subtitle's tooltip when it should say more than the line does
    /// (what its facts mean); the line's own text when unset.
    #[prop(optional, into)]
    sub_title: Option<TextProp>,
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
    // A document's head draws its line once the body has scrolled under it;
    // a fill or split page's strips always have one.
    let scrolled = RwSignal::new(mode != PageMode::Doc);
    let on_scroll = move |ev: web_sys::Event| {
        if mode != PageMode::Doc {
            return;
        }
        let top = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .map_or(0, |el| el.scroll_top());
        if scrolled.get_untracked() != (top > 0) {
            scrolled.set(top > 0);
        }
    };
    let has_toolbar = toolbar.is_some();
    view! {
        <div class=root.trim_end().to_string()>
            <div class="page-top" class:scrolled=move || scrolled.get() || has_toolbar>
                <HeadBar title=title sub=sub sub_title=sub_title head_extra=head_extra actions=actions/>
                {toolbar.map(|t| view! { <div class="page-toolbar">{t.run()}</div> })}
            </div>
            <div class="page-body" on:scroll=on_scroll>{children()}</div>
            {footer.map(|f| view! { <div class="page-foot">{f.run()}</div> })}
        </div>
    }
}

/// A content header bar on its own, for a route that is not a [`PageFrame`]
/// (the Chat, the labs): the same 48 px head, marked to move the window,
/// with the window buttons at its end.
#[component]
pub fn HeadBar(
    #[prop(into)] title: TextProp,
    /// One line under the title; ellipsized when the head is short of room,
    /// full text in the tooltip.
    #[prop(default = None)]
    sub: Option<TextProp>,
    /// The subtitle's tooltip when it should say more than the line does;
    /// the line's own text when unset.
    #[prop(default = None)]
    sub_title: Option<TextProp>,
    /// A control as the line under the title in place of `sub` (the labs'
    /// model picker, libadwaita's subtitle with a dropdown).
    #[prop(default = None)]
    sub_view: Option<ViewFn>,
    /// After the title (tabs, badges).
    #[prop(default = None)]
    head_extra: Option<ViewFn>,
    /// At the end, before the window buttons.
    #[prop(default = None)]
    actions: Option<ViewFn>,
) -> impl IntoView {
    let sub = sub.map(|s| {
        let tip = sub_title.unwrap_or_else(|| s.clone());
        view! { <span class="sub" title=move || tip.get().to_string()>{move || s.get()}</span> }
    });
    let rail = expect_context::<crate::shell::SidebarState>().rail;
    view! {
        <header class="page-head" data-window-header>
            // On the icon rail the sidebar has no room for the desktop's
            // start buttons; the content's head takes them.
            <Show when=move || rail.get()>
                <WindowButtons end=End::Start/>
            </Show>
            <div class="head-main">
                <div class="head-text">
                    <h1>{move || title.get()}</h1>
                    {sub}
                    {sub_view.map(|v| view! { <div class="sub sub-view">{v.run()}</div> })}
                </div>
                {head_extra.map(|v| v.run())}
                {actions.map(|a| view! { <div class="actions">{a.run()}</div> })}
            </div>
            <WindowButtons end=End::End/>
        </header>
    }
}
