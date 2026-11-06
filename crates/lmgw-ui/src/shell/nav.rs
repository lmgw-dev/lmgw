//! The sidebar's entries: a page ([`NavItem`]) and an entry with pages
//! under it ([`NavBranch`]), which folds in the full sidebar and opens a
//! flyout on the icon rail.

use leptos::html;
use leptos::prelude::*;
use leptos_router::components::A;
use wasm_bindgen::JsCast;

use super::SidebarState;
use crate::widgets::Popover;

/// A count on a nav entry: Docs' pending agent doc-requests, running
/// downloads, MCP servers in error. A queue nobody sees is a queue nobody
/// works. Rendered only when non-zero.
#[derive(Clone, Copy)]
pub(super) struct Badge {
    pub(super) n: Signal<u64>,
    /// What the count is, for its tooltip (the only place a rail shows it).
    pub(super) title: Signal<String>,
    /// Red, for something broken; amber (waiting on you) otherwise.
    pub(super) bad: bool,
}

fn badge_view(n: Signal<u64>, title: Signal<String>, bad: Signal<bool>) -> impl IntoView {
    view! {
        <Show when=move || { n.get() > 0 }>
            <span class="nav-badge" class:bad=move || bad.get() title=move || title.get()>
                {move || n.get()}
            </span>
        </Show>
    }
}

/// One sidebar entry. `icon` is the inner path/shape markup of a 16×16
/// stroke-based SVG (see app.css `.nav-item svg`). The SPA owns `/`, so the
/// hrefs are root-absolute (app.rs).
#[component]
pub(super) fn NavItem(
    href: &'static str,
    label: &'static str,
    icon: AnyView,
    /// The tooltip, where the label leans on the entry above it ("Audio"
    /// under Labs is the Audio lab); the label otherwise.
    #[prop(optional)]
    name: Option<&'static str>,
    /// Exact-match active state; needed for the root link, which would
    /// otherwise be marked active on every page.
    #[prop(default = false)]
    exact: bool,
    #[prop(default = None)] badge: Option<Badge>,
    /// A second count of another kind, after the first: Backends' updates
    /// available next to its live runs. Blue and marked "↑", so the two
    /// numbers are not read as one; rendered only when non-zero.
    #[prop(optional, into)]
    news: Option<Signal<u64>>,
    #[prop(optional, into)] news_title: Option<Signal<String>>,
) -> impl IntoView {
    // The title is the only label a rail has.
    view! {
        <A href=href exact=exact attr:class="nav-item" attr:title=name.unwrap_or(label)>
            <svg viewBox="0 0 16 16">{icon}</svg>
            <span class="nav-label">{label}</span>
            {badge.map(|b| badge_view(b.n, b.title, Signal::stored(b.bad)))}
            {news
                .map(|n| {
                    view! {
                        <Show when=move || { n.get() > 0 }>
                            <span
                                class="nav-badge news"
                                title=move || news_title.map(|t| t.get()).unwrap_or_default()
                            >
                                {move || format!("\u{2191}{}", n.get())}
                            </span>
                        </Show>
                    }
                })}
        </A>
    }
}

/// A page under a [`Branch`].
#[derive(Clone, Copy)]
pub(super) struct Leaf {
    pub(super) href: &'static str,
    pub(super) label: &'static str,
    /// The page's whole name, for tooltips: see [`NavItem`]'s `name`.
    pub(super) name: &'static str,
    pub(super) icon: fn() -> AnyView,
    pub(super) badge: Option<Badge>,
}

/// A nav entry with pages under it, one level deep and never deeper: Labs,
/// Models. In the full sidebar it is a disclosure row (macOS source list,
/// Breeze tree) whose pages show indented under it; on the icon rail it is
/// one icon that opens them in a flyout beside the rail.
///
/// Another page under one of them is one more [`Leaf`] in its `kids`.
pub(super) struct Branch {
    /// Names its open state, `lmgw.ui.open.nav.<key>`, and its DOM ids.
    pub(super) key: &'static str,
    pub(super) label: &'static str,
    pub(super) icon: fn() -> AnyView,
    /// The entry's own page, like a Mail parent mailbox: the label is a link
    /// and only the chevron folds. `None` when it only groups (Labs): the
    /// whole row folds.
    pub(super) href: Option<&'static str>,
    pub(super) kids: Vec<Leaf>,
}

/// `path` is `href` or a page under it — how `<A>` marks itself current.
fn under(path: &str, href: &str) -> bool {
    path.strip_prefix(href)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn focus(el: &web_sys::HtmlElement) {
    let opts = web_sys::FocusOptions::new();
    opts.set_prevent_scroll(true);
    let _ = el.focus_with_options(&opts);
}

#[component]
pub(super) fn NavBranch(branch: Branch) -> impl IntoView {
    let Branch {
        key,
        label,
        icon,
        href,
        kids,
    } = branch;
    let kids = StoredValue::new(kids);
    let state = expect_context::<SidebarState>();
    // Fresh install: folded. Remembered across restarts like the rail.
    let open = crate::prefs::persisted_bool(&format!("open.nav.{key}"), false);
    let path = leptos_router::hooks::use_location().pathname;
    let kid_active =
        Memo::new(move |_| path.with(|p| kids.with_value(|k| k.iter().any(|l| under(p, l.href)))));
    let self_active = Memo::new(move |_| href.is_some_and(|h| path.with(|p| under(p, h))));
    // Reaching one of its pages opens it, and it stays open after you leave
    // (Finder's reveal): folding is the owner's act, never the app's.
    Effect::new(move |_| {
        if kid_active.get() && !open.get_untracked() {
            open.set(true);
        }
    });
    // What a folded branch (and the rail's icon) shows for its pages: the
    // sum of their counts, each one named in the tooltip, red when one of
    // the counting kinds is broken.
    let badges = move || kids.with_value(|k| k.iter().filter_map(|l| l.badge).collect::<Vec<_>>());
    let sum = Signal::derive(move || badges().iter().map(|b| b.n.get()).sum::<u64>());
    let sum_title = Signal::derive(move || {
        badges()
            .iter()
            .filter(|b| b.n.get() > 0)
            .map(|b| b.title.get())
            .collect::<Vec<_>>()
            .join("\n")
    });
    let sum_bad = Signal::derive(move || badges().iter().any(|b| b.bad && b.n.get() > 0));
    let toggle_title = move || {
        let names = kids.with_value(|k| k.iter().map(|l| l.label).collect::<Vec<_>>().join(", "));
        format!("{} {names}", if open.get() { "Hide" } else { "Show" })
    };
    let toggle = move |_| open.update(|o| *o = !*o);
    let caret = || view! { <span class="caret-icon" aria-hidden="true">"▸"</span> };
    // A folded branch with the current page under it carries the selection.
    let holds = move || !open.get() && kid_active.get();

    let full = move || {
        let kids_id = format!("nav-kids-{key}");
        let head = match href {
            Some(h) => view! {
                <div class="nav-row">
                    <button
                        type="button"
                        class="nav-caret"
                        aria-expanded=move || open.get().to_string()
                        aria-controls=kids_id.clone()
                        aria-label=format!("Pages under {label}")
                        title=toggle_title
                        on:click=toggle
                    >
                        {caret()}
                    </button>
                    <A
                        href=h
                        attr:class=move || if holds() { "nav-item holds-current" } else { "nav-item" }
                        attr:title=label
                    >
                        <svg viewBox="0 0 16 16">{icon()}</svg>
                        <span class="nav-label">{label}</span>
                        <Show when=move || !open.get()>{badge_view(sum, sum_title, sum_bad)}</Show>
                    </A>
                </div>
            }
            .into_any(),
            None => view! {
                <button
                    type="button"
                    class="nav-item nav-head"
                    class:holds-current=holds
                    aria-expanded=move || open.get().to_string()
                    aria-controls=kids_id.clone()
                    title=toggle_title
                    on:click=toggle
                >
                    <span class="nav-caret">{caret()}</span>
                    <svg viewBox="0 0 16 16">{icon()}</svg>
                    <span class="nav-label">{label}</span>
                    <Show when=move || !open.get()>{badge_view(sum, sum_title, sum_bad)}</Show>
                </button>
            }
            .into_any(),
        };
        view! {
            {head}
            <div
                class="nav-kids"
                id=kids_id
                role="group"
                aria-label=label
                hidden=move || !open.get()
            >
                {kids
                    .get_value()
                    .into_iter()
                    .map(|l| {
                        view! {
                            <NavItem
                                href=l.href
                                label=l.label
                                name=l.name
                                icon=(l.icon)()
                                badge=l.badge
                            />
                        }
                    })
                    .collect_view()}
            </div>
        }
        .into_any()
    };

    // The rail: one icon, which opens the pages in a flyout beside it (the
    // branch's own page first). A click, never a hover.
    let fly_open = RwSignal::new(false);
    let anchor: NodeRef<html::Button> = NodeRef::new();
    let list: NodeRef<html::Div> = NodeRef::new();
    let was_open = StoredValue::new(false);
    // Focus goes back to the icon when the flyout closes by Escape or by
    // picking an item. A light dismiss (a click elsewhere) leaves the focus
    // where that click put it.
    let refocus = StoredValue::new(false);
    Effect::new(move |prev: Option<bool>| {
        let now = fly_open.get();
        if prev == Some(true) && !now && refocus.get_value() {
            refocus.set_value(false);
            request_animation_frame(move || {
                if let Some(a) = anchor.get_untracked() {
                    focus(&a);
                }
            });
        }
        now
    });
    let on_key = move |ev: web_sys::KeyboardEvent| {
        let Some(l) = list.get_untracked() else {
            return;
        };
        let Ok(nodes) = l.query_selector_all("[role=\"menuitem\"]") else {
            return;
        };
        let items: Vec<web_sys::HtmlElement> = (0..nodes.length())
            .filter_map(|i| nodes.item(i))
            .filter_map(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
            .collect();
        if items.is_empty() {
            return;
        }
        let n = items.len();
        let active = document().active_element();
        let cur = items.iter().position(|e| {
            active
                .as_ref()
                .is_some_and(|a| a == e.unchecked_ref::<web_sys::Element>())
        });
        let to = match ev.key().as_str() {
            "ArrowDown" => cur.map_or(0, |c| (c + 1) % n),
            "ArrowUp" => cur.map_or(n - 1, |c| (c + n - 1) % n),
            "Home" => 0,
            "End" => n - 1,
            // Enter is the link's own; Space picks too, as in any menu.
            " " => {
                if let Some(c) = cur {
                    items[c].click();
                }
                ev.prevent_default();
                return;
            }
            "Escape" => {
                ev.stop_propagation();
                ev.prevent_default();
                refocus.set_value(true);
                fly_open.set(false);
                return;
            }
            "Tab" => {
                fly_open.set(false);
                return;
            }
            _ => return,
        };
        ev.prevent_default();
        focus(&items[to]);
    };
    let rail = move || {
        let fly_id = format!("nav-fly-{key}");
        let entries: Vec<Leaf> = href
            .map(|h| Leaf {
                href: h,
                label,
                name: label,
                icon,
                badge: None,
            })
            .into_iter()
            .chain(kids.get_value())
            .collect();
        let entries = StoredValue::new(entries);
        // A stale open state must not pop the flyout open on the next mount.
        on_cleanup(move || fly_open.set(false));
        let items = move || {
            // The current page takes the focus when the flyout opens, the
            // first one otherwise.
            let first = path
                .with_untracked(|p| entries.with_value(|e| e.iter().position(|l| under(p, l.href))))
                .unwrap_or(0);
            entries
                .get_value()
                .into_iter()
                .enumerate()
                .map(|(i, l)| {
                    view! {
                        <A
                            href=l.href
                            attr:class="fly-item"
                            attr:role="menuitem"
                            attr:tabindex="-1"
                            attr:title=l.name
                            attr:data-autofocus=(i == first).then_some("")
                        >
                            <svg viewBox="0 0 16 16">{(l.icon)()}</svg>
                            <span class="fly-label">{l.label}</span>
                            {l.badge.map(|b| badge_view(b.n, b.title, Signal::stored(b.bad)))}
                        </A>
                    }
                })
                .collect_view()
        };
        view! {
            <button
                type="button"
                node_ref=anchor
                class="nav-item nav-head"
                class:holds-current=move || kid_active.get() || self_active.get()
                aria-haspopup="menu"
                aria-expanded=move || fly_open.get().to_string()
                aria-controls={
                    let id = fly_id.clone();
                    move || fly_open.get().then(|| id.clone())
                }
                title=label
                on:pointerdown=move |_| was_open.set_value(fly_open.get_untracked())
                on:click=move |_| {
                    let reopen = !was_open.get_value();
                    was_open.set_value(false);
                    fly_open.set(reopen && !fly_open.get_untracked());
                }
                on:keydown=move |ev| {
                    if !fly_open.get_untracked()
                        && matches!(ev.key().as_str(), "ArrowDown" | "ArrowUp")
                    {
                        ev.prevent_default();
                        fly_open.set(true);
                    }
                }
            >
                <svg viewBox="0 0 16 16">{icon()}</svg>
                <span class="nav-label">{label}</span>
                {badge_view(sum, sum_title, sum_bad)}
            </button>
            <Popover open=fly_open anchor=anchor class="nav-fly" beside=true min_width=180>
                <div
                    class="pop-list fly-list"
                    id=fly_id.clone()
                    role="menu"
                    aria-label=label
                    node_ref=list
                    on:keydown=on_key
                    // A pick navigates — the link's own click, so a guarded
                    // draft still asks — and closes the flyout.
                    on:click=move |ev| {
                        let on_link = ev
                            .target()
                            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                            .and_then(|e| e.closest("a[href]").ok().flatten())
                            .is_some();
                        if on_link {
                            refocus.set_value(true);
                            fly_open.set(false);
                        }
                    }
                >
                    // A branch without a page of its own is named on top; one
                    // with a page lists that page first, under its name.
                    {href
                        .is_none()
                        .then(|| view! { <div class="fly-head" role="presentation">{label}</div> })}
                    {items}
                </div>
            </Popover>
        }
        .into_any()
    };
    view! {
        <div class="nav-branch" data-branch=key>
            {move || if state.rail.get() { rail() } else { full() }}
        </div>
    }
}
