//! Shared widget base: toasts, the custom select, and the widget modules.
//!
//! The select is custom-rendered on purpose: WebKitGTK ignores CSS on native
//! `<select>` popups (the reason the old UI shipped select.js), so a native
//! control would flash a mis-styled GTK popup inside the Tauri shell.

use leptos::html;
use leptos::prelude::*;

/// Running text folded to its first lines, with "more".
pub mod clamp;
/// A destructive action that takes two clicks.
pub mod confirm;
/// Leaving a page with unsaved edits asks first.
pub mod dirty_guard;
/// The one filter row of a long list: search, facets, chips, the count.
pub mod filter_bar;
/// Forms that know what changed: FormState, Field, SaveBar, Explain.
pub mod form;
/// The image picker: a class-image or image-override field over the local
/// images of the class's engine.
pub mod image_picker;
/// The `<dialog>` modal: sizes, a pinned foot, a guard on unsaved input.
pub mod modal;
/// The model picker over the shared catalog.
pub mod model_picker;
/// The frame every `.page` route renders through (head, one scroller, foot).
pub mod page;
/// A panel in the top layer, anchored to a button: never clipped.
pub mod popover;
/// The agent catalog's config form (agent-catalog §2.6, §6.3): a JSON-schema
/// subset rendered as controls, built out of the widgets in this module.
pub mod schema_form;
/// A collapsible page section with its count and folded summary.
pub mod section;
/// A side panel beside a main pane, folding to a labelled rail.
pub mod split;
/// Tabs that are routes.
pub mod sub_nav;
/// Helpers on the table contract: group rows, the row menu, the "Showing
/// 10 of 55" line.
pub mod table;
/// The tool picker shared by the Chat's thread settings and the key editor.
pub mod tool_picker;
/// A voice of a text-to-speech model, offered the model's voice list.
pub mod voice_picker;

pub use clamp::ClampText;
pub use confirm::ConfirmButton;
pub use dirty_guard::DirtyGuardHost;
#[allow(unused_imports)]
pub use dirty_guard::{use_dirty_guard, DirtyGuard};
#[allow(unused_imports)]
pub use form::{flatten, use_touched, Explain, Field, FormState, Kind, SaveBar};
pub use image_picker::{ImageClass, ImagePicker};
#[allow(unused_imports)] // `ModalFooter` outside a modal is for the area phases
pub use modal::{use_modal, Modal, ModalFooter, ModalSize};
pub use model_picker::ModelPicker;
pub use page::{Density, PageFrame, PageMode};
pub use popover::Popover;
pub use split::{Side, SplitPane};
// The area phases adopt these; the design sample shows them meanwhile.
#[allow(unused_imports)]
pub use filter_bar::{use_slash_focus, Facet, FacetSet, FilterBar};
#[allow(unused_imports)]
pub use section::Section;
#[allow(unused_imports)]
pub use sub_nav::{NavTab, SubNav, Tone};
#[allow(unused_imports)]
pub use table::{GroupRow, MenuItem, RowMenu, ShowMore};

// ---------------------------------------------------------------------------
// Toasts
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum ToastKind {
    Ok,
    /// Amber: the thing happened, with something the owner has to know — a
    /// thread that opened while its MCP server is still missing. Not an error;
    /// red would say the action failed when it did not.
    Warn,
    Err,
}

impl ToastKind {
    fn class(self) -> &'static str {
        match self {
            Self::Ok => "toast ok",
            Self::Warn => "toast warn",
            Self::Err => "toast err",
        }
    }
}

#[derive(Clone)]
struct Toast {
    id: u64,
    kind: ToastKind,
    msg: String,
}

#[derive(Clone, Copy)]
pub struct Toasts {
    list: RwSignal<Vec<Toast>>,
    next_id: RwSignal<u64>,
}

impl Toasts {
    pub fn ok(&self, msg: impl Into<String>) {
        self.push(ToastKind::Ok, msg.into());
    }

    pub fn warn(&self, msg: impl Into<String>) {
        self.push(ToastKind::Warn, msg.into());
    }

    pub fn err(&self, msg: impl Into<String>) {
        self.push(ToastKind::Err, msg.into());
    }

    pub fn dismiss(&self, id: u64) {
        self.list.update(|l| l.retain(|t| t.id != id));
    }

    /// The id the next toast will get: remember it, and
    /// [`Self::errors_from`] it later lists what failed since.
    pub(crate) fn mark(&self) -> u64 {
        self.next_id.get_untracked()
    }

    /// The error toasts raised since `mark` that are still up (tracked). A
    /// modal repeats them inside itself: the toast host sits under a modal's
    /// backdrop, where an error can be seen but not dismissed.
    pub(crate) fn errors_from(&self, mark: u64) -> Vec<(u64, String)> {
        self.list.with(|l| {
            l.iter()
                .filter(|t| t.kind == ToastKind::Err && t.id >= mark)
                .map(|t| (t.id, t.msg.clone()))
                .collect()
        })
    }

    fn push(&self, kind: ToastKind, msg: String) {
        let id = self.next_id.get_untracked();
        self.next_id.set(id + 1);
        self.list.update(|l| l.push(Toast { id, kind, msg }));
        // An error stays until it is dismissed: it is the one toast that has
        // to be read, and six seconds is not always when the owner looks.
        if kind == ToastKind::Err {
            return;
        }
        let list = self.list;
        set_timeout(
            move || {
                list.try_update(|l| l.retain(|t| t.id != id));
            },
            std::time::Duration::from_secs(6),
        );
    }
}

/// From this many toasts on, the stack offers to clear itself in one click.
const DISMISS_ALL_FROM: usize = 3;

/// Install the toast context and render the overlay. Mounted once in `App`.
#[component]
pub fn ToastHost() -> impl IntoView {
    let toasts = Toasts {
        list: RwSignal::new(Vec::new()),
        next_id: RwSignal::new(0),
    };
    provide_context(toasts);
    listen_downloads(toasts);
    let many = move || toasts.list.with(Vec::len) >= DISMISS_ALL_FROM;
    view! {
        <div class="toast-host">
            <Show when=many>
                <button
                    type="button"
                    class="toast-clear"
                    on:click=move |_| toasts.list.set(Vec::new())
                >
                    {move || format!("Dismiss all {}", toasts.list.with(Vec::len))}
                </button>
            </Show>
            <For each=move || toasts.list.get() key=|t| t.id let:t>
                <div class=t.kind.class() role=if t.kind == ToastKind::Err { "alert" } else { "status" }>
                    {t.msg.clone()}
                    <button
                        class="toast-x"
                        title="Dismiss"
                        on:click=move |_| toasts.list.update(|l| l.retain(|x| x.id != t.id))
                    >
                        "✕"
                    </button>
                </div>
            </For>
        </div>
    }
}

/// The desktop app saves a download itself (its `on_download` handler) and
/// tells the page with `lmgw-downloaded` `{path}` or `lmgw-download-failed`
/// `{error}` window events; a plain browser never fires them.
fn listen_downloads(toasts: Toasts) {
    use wasm_bindgen::JsCast;
    let field = |ev: &web_sys::Event, key: &str| -> String {
        ev.dyn_ref::<web_sys::CustomEvent>()
            .and_then(|c| js_sys::Reflect::get(&c.detail(), &key.into()).ok())
            .and_then(|v| v.as_string())
            .unwrap_or_default()
    };
    let saved = window_event_listener_untyped("lmgw-downloaded", move |ev| {
        toasts.ok(format!("Saved to {}", field(&ev, "path")));
    });
    let failed = window_event_listener_untyped("lmgw-download-failed", move |ev| {
        toasts.err(format!("Download failed: {}", field(&ev, "error")));
    });
    on_cleanup(move || {
        saved.remove();
        failed.remove();
    });
}

pub fn use_toasts() -> Toasts {
    expect_context::<Toasts>()
}

// ---------------------------------------------------------------------------
// Copy button
// ---------------------------------------------------------------------------

/// Put a secret the owner is about to paste on the clipboard, and say so only
/// once the clipboard took it: the write is a promise that can be refused (no
/// focus, no permission), and "key copied" over an empty clipboard sends the
/// owner off to paste nothing.
pub fn copy_secret(text: &str, toasts: Toasts, ok: String) {
    use wasm_bindgen::{closure::Closure, JsCast, JsValue};
    let done = Closure::once_into_js(move |_: JsValue| toasts.ok(ok));
    let failed = Closure::once_into_js(move |e: JsValue| {
        let why = e
            .as_string()
            .or_else(|| {
                js_sys::Reflect::get(&e, &"message".into())
                    .ok()
                    .and_then(|m| m.as_string())
            })
            .unwrap_or_else(|| "the clipboard refused it".to_string());
        toasts.err(format!("not copied: {why}"));
    });
    // `then` through Reflect: the two callbacks are one-shot JS functions,
    // and the one that never runs is a few bytes, not a kept `Closure`.
    let promise = window().navigator().clipboard().write_text(text);
    if let Some(then) = js_sys::Reflect::get(&promise, &"then".into())
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
    {
        let _ = then.call2(&promise, &done, &failed);
    }
}

/// Clipboard button that flips to a checkmark for two seconds. Shared by
/// every "here is a name/URL/snippet you'll paste elsewhere" surface.
#[component]
pub fn CopyBtn(
    text: String,
    /// Tooltip; defaults to a plain "Copy".
    #[prop(default = "Copy")]
    title: &'static str,
) -> impl IntoView {
    let copied = RwSignal::new(false);
    view! {
        <button
            class="copy-btn"
            title=title
            on:click=move |_| {
                let _ = window().navigator().clipboard().write_text(&text);
                copied.set(true);
                set_timeout(move || copied.set(false), std::time::Duration::from_secs(2));
            }
        >
            {move || if copied.get() { "✓" } else { "⧉" }}
        </button>
    }
}

// ---------------------------------------------------------------------------
// Select
// ---------------------------------------------------------------------------

/// How long typed letters keep adding to one type-ahead search.
const TYPEAHEAD_MS: f64 = 700.0;

/// Custom dropdown: `options` is (value, label) pairs; `value` holds the
/// selected value and is written on pick.
///
/// The list is a [`Popover`], so it is never clipped by the card, table or
/// modal it sits in. Keyboard: ↓/↑/Enter on the closed button open it; in the
/// list ↑↓ PgUp PgDn Home End move, Enter picks, Esc and Tab close, and typed
/// letters jump to the next label starting with them. Past 12 options a
/// filter box leads the list (all words must match), with the count it leaves
/// ("12 of 57"). A current value that is not among the options is shown as
/// "value (not in list)", never as the placeholder, which would hide it.
///
/// A pick fires a bubbling `change` from the button, so `use_touched` and a
/// guarded modal notice it like any native control.
#[component]
pub fn Select(
    value: RwSignal<String>,
    #[prop(into)] options: Signal<Vec<(String, String)>>,
    #[prop(default = "—")] placeholder: &'static str,
    /// Greyed out and unopenable — for a value this row does not own, which
    /// still has to *show* what it is.
    #[prop(into, default = Signal::derive(|| false))]
    disabled: Signal<bool>,
    /// Force the filter box on or off; by default it is on past 12 options.
    #[prop(optional)]
    filter: Option<bool>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let btn: NodeRef<html::Button> = NodeRef::new();
    let list: NodeRef<html::Div> = NodeRef::new();
    let query = RwSignal::new(String::new());
    // The keyboard's row, as an index into `shown`.
    let active = RwSignal::new(0usize);
    // A press on the button while the list is open: the list closes itself on
    // that press (light dismiss), and the click must not open it again.
    let was_open = StoredValue::new(false);
    let typed = StoredValue::new((String::new(), 0.0_f64));

    let with_filter = move || filter.unwrap_or_else(|| options.with(|o| o.len() > 12));
    let shown = Memo::new(move |_| {
        let q = query.get();
        let words = filter_words(&q);
        options.with(|opts| {
            opts.iter()
                .filter(|(v, l)| {
                    words
                        .iter()
                        .all(|w| matches_word(l, w) || matches_word(v, w))
                })
                .cloned()
                .collect::<Vec<_>>()
        })
    });

    let label = move || {
        let v = value.get();
        options.with(|opts| match opts.iter().find(|(val, _)| *val == v) {
            Some((_, l)) => (l.clone(), false),
            None if v.is_empty() => (placeholder.to_string(), true),
            // Options still loading: the value is the best there is.
            None if opts.is_empty() => (v, false),
            None => (format!("{v} (not in list)"), false),
        })
    };

    // Opening starts from the current value, with the filter empty.
    Effect::new(move |_| {
        if open.get() {
            query.set(String::new());
            let v = value.get_untracked();
            let i = shown.with_untracked(|s| s.iter().position(|(x, _)| *x == v));
            active.set(i.unwrap_or(0));
        }
    });

    let pick = move |v: String| {
        value.set(v);
        open.set(false);
        if let Some(b) = btn.get_untracked() {
            let _ = b.focus();
            popover::fire_change(&b);
        }
    };
    let move_to = move |i: usize| {
        let n = shown.with_untracked(Vec::len);
        if n == 0 {
            return;
        }
        let i = i.min(n - 1);
        active.set(i);
        if let Some(l) = list.get_untracked() {
            popover::reveal_child(&l, i);
        }
    };
    let close_to_button = move || {
        open.set(false);
        if let Some(b) = btn.get_untracked() {
            let _ = b.focus();
        }
    };

    let on_key = move |ev: web_sys::KeyboardEvent| {
        let cur = active.get_untracked();
        let n = shown.with_untracked(Vec::len);
        let page = list
            .get_untracked()
            .map(|l| popover::page_rows(&l))
            .unwrap_or(8);
        let in_filter = with_filter() && !query.with_untracked(String::is_empty);
        match ev.key().as_str() {
            "ArrowDown" => move_to(if n == 0 { 0 } else { (cur + 1).min(n - 1) }),
            "ArrowUp" => move_to(cur.saturating_sub(1)),
            "PageDown" => move_to(cur + page),
            "PageUp" => move_to(cur.saturating_sub(page)),
            // With text in the filter box, Home/End move its caret.
            "Home" if !in_filter => move_to(0),
            "End" if !in_filter => move_to(n.saturating_sub(1)),
            "Enter" => {
                if let Some((v, _)) = shown.with_untracked(|s| s.get(cur).cloned()) {
                    pick(v);
                }
            }
            "Escape" => {
                // Ours alone: inside a modal, Esc must not close the modal too.
                ev.stop_propagation();
                close_to_button();
            }
            // Leave the list the way a native one does: closed, with focus
            // back on the control, so Tab carries on from there.
            "Tab" => {
                close_to_button();
                return;
            }
            k if !with_filter() && is_typeahead(&ev, k) => {
                let now = js_sys::Date::now();
                let (mut buf, at) = typed.get_value();
                if now - at > TYPEAHEAD_MS {
                    buf.clear();
                }
                buf.push_str(&k.to_lowercase());
                typed.set_value((buf.clone(), now));
                let hit = shown.with_untracked(|s| typeahead(s, cur, &buf));
                if let Some(i) = hit {
                    move_to(i);
                }
            }
            _ => return,
        }
        ev.prevent_default();
    };

    view! {
        <div class="select">
            <button
                type="button"
                class="input select-btn"
                node_ref=btn
                aria-haspopup="listbox"
                aria-expanded=move || open.get().to_string()
                disabled=move || disabled.get()
                on:pointerdown=move |_| was_open.set_value(open.get_untracked())
                on:click=move |_| {
                    let reopen = !was_open.get_value();
                    was_open.set_value(false);
                    if disabled.get_untracked() {
                        return;
                    }
                    open.set(reopen && !open.get_untracked());
                }
                on:keydown=move |ev| {
                    if open.get_untracked() || disabled.get_untracked() {
                        return;
                    }
                    if matches!(ev.key().as_str(), "ArrowDown" | "ArrowUp") {
                        ev.prevent_default();
                        open.set(true);
                    }
                }
            >
                {move || {
                    let (text, dim) = label();
                    view! { <span class:dim=dim>{text}</span> }
                }}
                <span class="select-arrow">"▾"</span>
            </button>
            <Popover open=open anchor=btn class="select-pop">
                <div class="pop-inner" on:keydown=on_key>
                    <Show when=with_filter>
                        <div class="pop-filter">
                            <input
                                class="input"
                                type="search"
                                placeholder="Filter"
                                autocomplete="off"
                                spellcheck="false"
                                data-autofocus
                                data-untracked
                                prop:value=move || query.get()
                                on:input=move |ev| {
                                    query.set(event_target_value(&ev));
                                    active.set(0);
                                    if let Some(l) = list.get_untracked() {
                                        l.set_scroll_top(0);
                                    }
                                }
                            />
                            <span class="pop-count">
                                {move || {
                                    crate::fmt::of(
                                        shown.with(Vec::len),
                                        options.with(Vec::len),
                                    )
                                }}
                            </span>
                        </div>
                    </Show>
                    <div
                        class="pop-list"
                        role="listbox"
                        tabindex="-1"
                        node_ref=list
                        data-autofocus=move || (!with_filter()).then_some("")
                    >
                        <For
                            each=move || shown.get().into_iter().enumerate()
                            key=|(i, (v, _))| (*i, v.clone())
                            let:item
                        >
                            {
                                let (i, (val, lbl)) = item;
                                let sel = {
                                    let val = val.clone();
                                    move || value.with(|v| *v == val)
                                };
                                view! {
                                    <div
                                        class="select-opt"
                                        class:sel=sel.clone()
                                        class:active=move || active.get() == i
                                        role="option"
                                        aria-selected=move || sel().to_string()
                                        on:pointermove=move |_| {
                                            if active.get_untracked() != i {
                                                active.set(i);
                                            }
                                        }
                                        on:click=move |_| pick(val.clone())
                                    >
                                        {lbl}
                                    </div>
                                }
                            }
                        </For>
                        <Show when=move || shown.with(Vec::is_empty)>
                            <div class="pop-empty">
                                {move || {
                                    if options.with(Vec::is_empty) {
                                        "Nothing to choose from yet".to_string()
                                    } else {
                                        format!("Nothing matches \u{201c}{}\u{201d}", query.get().trim())
                                    }
                                }}
                            </div>
                        </Show>
                    </div>
                </div>
            </Popover>
        </div>
    }
}

/// The words a filter box holds, lowercased: every one has to match.
pub(crate) fn filter_words(q: &str) -> Vec<String> {
    q.split_whitespace().map(str::to_lowercase).collect()
}

/// Case-insensitive substring test for one lowercased filter word.
pub(crate) fn matches_word(hay: &str, word: &str) -> bool {
    hay.to_lowercase().contains(word)
}

fn is_typeahead(ev: &web_sys::KeyboardEvent, key: &str) -> bool {
    !ev.ctrl_key() && !ev.meta_key() && !ev.alt_key() && key.chars().count() == 1 && key != " "
}

/// The row a type-ahead buffer lands on. One letter — or the same letter
/// pressed again — steps to the next label starting with it after the current
/// row, wrapping, the way a native list walks its "b…" entries; a longer
/// buffer narrows from the current row itself.
fn typeahead(opts: &[(String, String)], cur: usize, buf: &str) -> Option<usize> {
    let n = opts.len();
    let first = buf.chars().next()?;
    let (prefix, from) = if buf.chars().all(|c| c == first) {
        (first.to_string(), cur + 1)
    } else {
        (buf.to_string(), cur)
    };
    (0..n)
        .map(|k| (from + k) % n)
        .find(|&i| opts[i].1.to_lowercase().starts_with(&prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(labels: &[&str]) -> Vec<(String, String)> {
        labels
            .iter()
            .map(|l| (l.to_string(), l.to_string()))
            .collect()
    }

    #[test]
    fn typeahead_finds_the_next_label_with_the_prefix() {
        let o = opts(&["alpha", "beta", "bravo", "charlie"]);
        assert_eq!(typeahead(&o, 0, "b"), Some(1));
        assert_eq!(typeahead(&o, 1, "b"), Some(2));
        // wraps around to the first match
        assert_eq!(typeahead(&o, 2, "b"), Some(1));
        // a longer buffer searches from the current row itself
        assert_eq!(typeahead(&o, 2, "br"), Some(2));
        assert_eq!(typeahead(&o, 0, "ch"), Some(3));
        assert_eq!(typeahead(&o, 0, "x"), None);
    }

    #[test]
    fn a_repeated_letter_cycles_through_that_letter() {
        let o = opts(&["alpha", "beta", "bravo", "charlie"]);
        assert_eq!(typeahead(&o, 1, "bb"), Some(2));
        assert_eq!(typeahead(&o, 2, "bbb"), Some(1));
    }

    #[test]
    fn every_filter_word_has_to_match() {
        let words = filter_words("  Gem 12 ");
        assert_eq!(words, ["gem", "12"]);
        let hit = |s: &str| words.iter().all(|w| matches_word(s, w));
        assert!(hit("gemma4-12b"));
        assert!(!hit("gemma4-26b"));
    }
}
