//! A collapsible page section (UX plan §2 #9): the eyebrow heading every
//! long page is made of, with its count, and a one-line summary standing in
//! for the body while it is folded.

// The area phases adopt these page by page; until then only the design
// sample shows them.
#![allow(dead_code)]

use leptos::prelude::*;

/// `<section class="sec">` → `.sec-head` (caret + eyebrow title, `.count`,
/// `.sec-summary` while closed, `.sec-actions`) and `.sec-body`.
///
/// The body is hidden with the `hidden` attribute, not unmounted, so a form
/// inside keeps what was typed into it when the section folds. `lazy` defers
/// mounting until the first open (for a body that fetches). `persist` keeps
/// the open state as `lmgw.ui.open.<persist>` (e.g. `overview.stopped`).
/// `force_open` holds it open — while a filter is narrowing the page, a
/// folded section would hide the matches it counts.
#[component]
pub fn Section(
    #[prop(into)] title: TextProp,
    /// The pill after the title: "23", or "3 of 23" while filtered.
    #[prop(optional, into)]
    count: Option<Signal<String>>,
    /// One line shown in place of the body while folded.
    #[prop(optional, into)]
    summary: Option<Signal<String>>,
    #[prop(optional)] persist: Option<&'static str>,
    #[prop(default = true)] default_open: bool,
    /// The open state, held by the caller instead (it persists it itself) —
    /// for a page that opens a section from outside, e.g. a deep link to a
    /// field inside it. `persist` and `default_open` are then unused.
    #[prop(optional)]
    open: Option<RwSignal<bool>>,
    #[prop(optional, into)] force_open: Option<Signal<bool>>,
    #[prop(optional)] lazy: bool,
    /// Right-aligned controls in the head; they stay usable while folded.
    #[prop(optional, into)]
    actions: Option<ViewFn>,
    children: ChildrenFn,
) -> impl IntoView {
    let open = match (open, persist) {
        (Some(o), _) => o,
        (None, Some(p)) => crate::prefs::persisted_bool(&format!("open.{p}"), default_open),
        (None, None) => RwSignal::new(default_open),
    };
    let forced = move || force_open.is_some_and(|f| f.get());
    let shown = move || open.get() || forced();
    let mounted = RwSignal::new(!lazy || open.get_untracked());
    Effect::new(move |_| {
        if shown() && !mounted.get_untracked() {
            mounted.set(true);
        }
    });
    let tip = move || forced().then_some("Held open while a filter is active");

    view! {
        <section class="sec" class:closed=move || !shown()>
            <div class="sec-head">
                <button
                    type="button"
                    class="sec-toggle"
                    aria-expanded=move || shown().to_string()
                    disabled=forced
                    title=tip
                    on:click=move |_| open.update(|o| *o = !*o)
                >
                    <span class="caret-icon" aria-hidden="true">"▸"</span>
                    <span class="sec-title">{move || title.get()}</span>
                </button>
                {count.map(|c| view! { <span class="count">{move || c.get()}</span> })}
                {summary
                    .map(|s| {
                        view! {
                            <Show when=move || !shown()>
                                <span class="sec-summary" title=move || s.get()>
                                    {move || s.get()}
                                </span>
                            </Show>
                        }
                    })}
                {actions.map(|a| view! { <div class="sec-actions">{a.run()}</div> })}
            </div>
            <div class="sec-body" hidden=move || !shown()>
                <Show when=move || mounted.get()>{children()}</Show>
            </div>
        </section>
    }
}
