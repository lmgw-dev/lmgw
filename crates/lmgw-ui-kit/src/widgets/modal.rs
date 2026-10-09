//! The `<dialog>` modal (UX plan §1.6, §2 #7): a head, one scrolling body and
//! a foot that is always on screen, in three sizes, optionally guarding its
//! unsaved input.
//!
//! A form rendered inside a modal owns its Save and its state, so it hands
//! its buttons up with [`ModalFooter`] rather than the modal owning them: the
//! buttons land in the pinned foot while staying the form's own closures.

use std::sync::atomic::{AtomicU64, Ordering};

use leptos::context::Provider;
use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::HtmlDialogElement;

use super::confirm::is_second_press;

/// The panel's size. `Auto` fits the content (420px to `min(820px, 94vw)`);
/// `Wide` is an editor's width and grows with its content up to the window
/// less 32px; `Full`, and any size with `fill`, is a frame that tall, for
/// browsers whose list should not make the frame jump as it loads.
#[allow(dead_code)] // `Wide` lands with the Phase 2 editors
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum ModalSize {
    #[default]
    Auto,
    /// `min(1280px, 94vw)` wide.
    Wide,
    /// `94vw` wide.
    Full,
}

impl ModalSize {
    fn class(self) -> &'static str {
        match self {
            Self::Auto => "modal-panel",
            Self::Wide => "modal-panel wide",
            Self::Full => "modal-panel full",
        }
    }
}

static NEXT_FOOT: AtomicU64 = AtomicU64::new(1);

/// The enclosing modal, for a form inside it ([`use_modal`]).
#[derive(Clone, Copy)]
pub struct ModalState {
    open: RwSignal<bool>,
    touched: RwSignal<bool>,
    asking: RwSignal<bool>,
    guard: bool,
    /// What a [`ModalFooter`] inside put in the foot. The id lets a footer
    /// that is going away clear only its own entry, not a newer one that a
    /// re-rendered form has already put there.
    foot: RwSignal<Option<(u64, ViewFn)>>,
}

impl ModalState {
    /// The input is saved but the modal stays open (it now shows a result):
    /// closing it must not ask about changes that are no longer unsaved.
    pub fn saved(&self) {
        self.touched.set(false);
        self.asking.set(false);
    }

    /// An edit that fired no `input`/`change` — a value a button wrote (a
    /// file picked in a nested browser, a row added or taken off): from now
    /// on closing asks, as it does after typing (review code:C7).
    pub fn touch(&self) {
        if self.guard && !self.touched.get_untracked() {
            self.touched.set(true);
        }
    }

    /// Close the way ✕ does: with unsaved input in a guarded modal, ask
    /// first. For a footer button that means "done here" rather than
    /// "discard" (an image editor's Close).
    pub fn request_close(&self) {
        if self.guard && self.touched.get_untracked() {
            self.asking.set(true);
        } else {
            self.open.set(false);
        }
    }
}

/// The modal this component renders inside, if any.
pub fn use_modal() -> Option<ModalState> {
    use_context::<ModalState>()
}

/// `open` drives visibility both ways: Esc, ✕ and a backdrop click write it
/// back, and a caller closing it (a successful Save) is never asked about.
///
/// `guard`: once anything in the body has been typed into or changed, a
/// backdrop click does nothing and Esc or ✕ ask first, in a strip under the
/// title ("Unsaved changes · Discard · Keep editing"), and a link out of the
/// page or a reload asks through the page's dirty guard, under the modal's
/// title. Filter boxes inside a pop, and inputs marked `data-untracked`, do
/// not count as changes.
///
/// `fill`: the body does not scroll; it holds one `.fill-pane` (or a flexing
/// frame, like the chat preview) that does.
#[component]
pub fn Modal(
    open: RwSignal<bool>,
    title: &'static str,
    #[prop(optional)] size: ModalSize,
    #[prop(optional)] guard: bool,
    #[prop(optional)] fill: bool,
    /// Buttons owned by the caller; a [`ModalFooter`] in the children adds
    /// its own after them.
    #[prop(optional, into)]
    footer: Option<ViewFn>,
    children: ChildrenFn,
) -> impl IntoView {
    let dialog: NodeRef<html::Dialog> = NodeRef::new();
    let keep_btn: NodeRef<html::Button> = NodeRef::new();
    let touched = RwSignal::new(false);
    let asking = RwSignal::new(false);
    // Where the press that ends in a click began: a text selection dragged
    // out of the panel ends on the backdrop, and is not a click on it.
    let down_on_backdrop = StoredValue::new(false);
    // When it opened: the second press of the double-click that opened it
    // (a "Delete…" menu item) lands on the backdrop, and is not a dismissal.
    let opened_at = StoredValue::new(0.0_f64);
    let foot = RwSignal::new(None::<(u64, ViewFn)>);
    let toasts = use_context::<super::Toasts>();
    // Errors raised while this modal is up (see `Toasts::errors_from`).
    let errs_since = RwSignal::new(u64::MAX);
    // Handed to the children through a Provider, not `provide_context`: a
    // component has no owner of its own, so a context provided here would
    // also reach everything rendered after the modal in the caller — and a
    // nested modal (an editor's file browser) would capture the editor's
    // own footer.
    let state = ModalState {
        open,
        touched,
        asking,
        guard,
        foot,
    };

    Effect::new(move |_| {
        let Some(el) = dialog.get() else { return };
        let el: HtmlDialogElement = el;
        if open.get() {
            opened_at.set_value(js_sys::Date::now());
            touched.set(false);
            asking.set(false);
            errs_since.set(toasts.map_or(u64::MAX, |t| t.mark()));
            if !el.open() {
                let _ = el.show_modal();
            }
        } else if el.open() {
            el.close();
        }
    });
    // The strip takes focus, so Enter keeps editing and Esc means the same.
    Effect::new(move |_| {
        if asking.get() {
            if let Some(b) = keep_btn.get() {
                let _ = b.focus();
            }
        }
    });

    // A guarded modal's unsaved input is a draft of the page it is on: a
    // link inside it (a notice's "Settings → Backends", a picker's empty
    // state) asks before it navigates away and discards it, as the sidebar
    // does for a page's own form — and so does a reload.
    if guard {
        if let Some(g) = super::dirty_guard::try_use_dirty_guard() {
            g.watch_page(title, Signal::derive(move || open.get() && touched.get()));
        }
    }

    let dirty = move || guard && touched.get_untracked();
    let request_close = move || state.request_close();
    let mark = move |ev: web_sys::Event| {
        if guard && !touched.get_untracked() && counts_as_edit(&ev) {
            touched.set(true);
        }
    };

    let has_foot = {
        let has_prop = footer.is_some();
        move || has_prop || foot.with(Option::is_some)
    };
    let footer = StoredValue::new(footer);

    view! {
        <dialog
            node_ref=dialog
            class="modal"
            // Esc: with unsaved input, ask instead. Handled on keydown too,
            // because Chrome lets a second Esc close a dialog whose `cancel`
            // was already refused. Only an Esc meant for this dialog: one
            // pressed in a modal nested inside it (an editor's file browser)
            // bubbles through here, and is that modal's to handle.
            on:keydown=move |ev| {
                if ev.key() == "Escape"
                    && !ev.default_prevented()
                    && dirty()
                    && is_innermost(&ev, dialog)
                {
                    ev.prevent_default();
                    asking.update(|a| *a = !*a);
                }
            }
            on:cancel=move |ev: web_sys::Event| {
                ev.prevent_default();
                request_close();
            }
            // Closed by anything else (a form's method=dialog, the browser):
            // the signal follows.
            on:close=move |_| {
                if open.get_untracked() {
                    open.set(false);
                }
            }
            on:pointerdown=move |ev| {
                down_on_backdrop.set_value(is_self(&ev, dialog));
            }
            on:click=move |ev| {
                let backdrop = down_on_backdrop.get_value() && is_self(&ev, dialog);
                down_on_backdrop.set_value(false);
                let since = js_sys::Date::now() - opened_at.get_value();
                if backdrop && !dirty() && !is_second_press(ev.detail(), since) {
                    open.set(false);
                }
            }
        >
            // Clicks inside stop here: a modal rendered inside a clickable
            // row must not also click the row.
            <div
                class=size.class()
                on:click=move |ev| ev.stop_propagation()
                on:input=mark
                on:change=mark
            >
                <div class="modal-head">
                    <h2>{title}</h2>
                    <button
                        type="button"
                        class="btn ghost"
                        title="Close"
                        on:click=move |_| request_close()
                    >
                        "✕"
                    </button>
                </div>
                <Show when=move || asking.get()>
                    <div class="modal-ask" role="alertdialog" aria-label="Unsaved changes">
                        <span>"Unsaved changes"</span>
                        <span class="spacer"></span>
                        <button type="button" class="btn danger sm" on:click=move |_| open.set(false)>
                            "Discard"
                        </button>
                        <button
                            type="button"
                            class="btn sm"
                            node_ref=keep_btn
                            on:click=move |_| asking.set(false)
                        >
                            "Keep editing"
                        </button>
                    </div>
                </Show>
                <div class="modal-body" class:fill=fill>
                    <Provider value=state>{children()}</Provider>
                </div>
                {toasts
                    .map(|t| {
                        let errs = move || t.errors_from(errs_since.get());
                        view! {
                            <Show when=move || !errs().is_empty()>
                                <div class="modal-errs" role="alert">
                                    <For each=errs key=|(id, _)| *id let:e>
                                        <div class="modal-err">
                                            <span>{e.1.clone()}</span>
                                            <button
                                                type="button"
                                                class="toast-x"
                                                title="Dismiss"
                                                on:click=move |_| t.dismiss(e.0)
                                            >
                                                "✕"
                                            </button>
                                        </div>
                                    </For>
                                </div>
                            </Show>
                        }
                    })}
                <Show when=has_foot>
                    <div class="modal-foot">
                        {move || footer.get_value().map(|f| f.run())}
                        {move || foot.get().map(|(_, f)| f.run())}
                    </div>
                </Show>
            </div>
        </dialog>
    }
}

/// Buttons for the enclosing [`Modal`]'s pinned foot, from inside its body —
/// the usual shape of an editor whose form owns its Save:
///
/// ```ignore
/// <ModalFooter>
///     <button class="btn ghost" on:click=move |_| open.set(false)>"Cancel"</button>
///     <button class="btn primary" on:click=save>"Save"</button>
/// </ModalFooter>
/// ```
///
/// Right-aligned; wrap a destructive action in `<span class="foot-danger">`
/// to send it to the far left, away from Save. Outside a modal it renders in
/// place, as a right-aligned row.
#[component]
pub fn ModalFooter(children: ChildrenFn) -> impl IntoView {
    let Some(ModalState { foot: slot, .. }) = use_modal() else {
        return view! { <div class="modal-foot inline">{children()}</div> }.into_any();
    };
    let id = NEXT_FOOT.fetch_add(1, Ordering::Relaxed);
    slot.set(Some((id, ViewFn::from(move || children()))));
    on_cleanup(move || {
        slot.try_update(|s| {
            if s.as_ref().is_some_and(|(mine, _)| *mine == id) {
                *s = None;
            }
        });
    });
    ().into_any()
}

/// Did this event land on the dialog element itself — its backdrop — rather
/// than on anything inside the panel?
fn is_self(ev: &web_sys::MouseEvent, dialog: NodeRef<html::Dialog>) -> bool {
    let (Some(t), Some(d)) = (ev.target(), dialog.get_untracked()) else {
        return false;
    };
    t.dyn_ref::<web_sys::Node>()
        .is_some_and(|n| n.is_same_node(Some(d.as_ref())))
}

/// Is this dialog the nearest one around the event's target — not an outer
/// dialog the event is bubbling through?
fn is_innermost(ev: &web_sys::KeyboardEvent, dialog: NodeRef<html::Dialog>) -> bool {
    let (Some(t), Some(d)) = (ev.target(), dialog.get_untracked()) else {
        return false;
    };
    let Some(el) = t.dyn_ref::<web_sys::Element>() else {
        return false;
    };
    matches!(el.closest("dialog"), Ok(Some(near)) if near.is_same_node(Some(d.as_ref())))
}

/// An `input`/`change` that edits the form: not one from a pop's filter box
/// (a Select being searched is not a change until something is picked), and
/// not from a control marked `data-untracked` (a search field, a view toggle).
pub fn counts_as_edit(ev: &web_sys::Event) -> bool {
    let Some(el) = ev
        .target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
    else {
        return false;
    };
    !matches!(el.closest("[popover], [data-untracked]"), Ok(Some(_)))
}
