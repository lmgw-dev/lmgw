//! A destructive row action that takes two clicks (UX plan §2 #8, §4
//! "nothing destructive is one click").
//!
//! The first click arms it: the button becomes `[<confirm> ✓] [✕]` in place,
//! so the question is asked where the pointer already is, without a modal
//! for something as small as one row. It disarms by itself after six
//! seconds, on Esc, and when focus leaves it — an armed button left behind
//! is a one-click delete waiting for a stray click. And the second click of
//! a double-click is not an answer: it lands on the armed button that just
//! took the first one's place ([`is_second_press`]).

use std::time::Duration;

use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

/// How long an armed button waits for the second click.
const ARMED_FOR: Duration = Duration::from_secs(6);

/// Presses closer than this to the arming one are the same gesture — a
/// double-click, a key held down — not a decision (review code:C2).
pub const SETTLE_MS: f64 = 400.0;

/// Is a press that would confirm really the tail of the press that armed?
/// `detail` is the click's count (2 for a double-click's second press, 0 for
/// a keyboard "click"), `since_armed` the milliseconds since arming.
pub fn is_second_press(detail: i32, since_armed: f64) -> bool {
    detail > 1 || since_armed < SETTLE_MS
}

#[component]
pub fn ConfirmButton(
    #[prop(into)] label: TextProp,
    /// The question on the armed button, e.g. "Revoke agent:folder-chat?".
    #[prop(into)]
    confirm: TextProp,
    on_confirm: Callback<()>,
    /// The resting button's classes; `btn ghost sm` when unset. The armed pair
    /// is small when this is (or when the trigger is not a `.btn` at all, like
    /// a thread list's ✕), full height otherwise.
    #[prop(optional)]
    class: Option<&'static str>,
    #[prop(into, default = false.into())] disabled: Signal<bool>,
    #[prop(optional, into)] title: Option<TextProp>,
    /// Drawn before the label on the resting button (an icon-only button has
    /// an empty label and says what it does in `title`).
    #[prop(optional, into)]
    icon: Option<ViewFn>,
) -> impl IntoView {
    let class = class.unwrap_or("btn ghost sm");
    let has = |c: &str| class.split_whitespace().any(|x| x == c);
    let armed_class = if !has("btn") || has("sm") {
        "btn danger sm"
    } else {
        "btn danger"
    };
    let cancel_class = if !has("btn") || has("sm") {
        "btn ghost sm"
    } else {
        "btn ghost"
    };

    let armed = RwSignal::new(false);
    let armed_at = StoredValue::new(0.0_f64);
    let timer = StoredValue::new(None::<TimeoutHandle>);
    // A press that started inside: the focus move it causes is not "left".
    let pressing = StoredValue::new(false);
    // Hand focus back to the resting button once it is mounted again (Esc,
    // ✕): a keyboard user stays on the row instead of landing on <body>.
    let refocus = StoredValue::new(false);
    let wrap: NodeRef<html::Span> = NodeRef::new();
    let rest_btn: NodeRef<html::Button> = NodeRef::new();
    let armed_btn: NodeRef<html::Button> = NodeRef::new();
    let cancel_btn: NodeRef<html::Button> = NodeRef::new();

    let focus_inside = move || {
        let (Some(w), Some(active)) = (
            wrap.get_untracked(),
            document().active_element().map(web_sys::Node::from),
        ) else {
            return false;
        };
        w.contains(Some(&active))
    };
    let disarm = move |back: bool| {
        if let Some(h) = timer.get_value() {
            h.clear();
        }
        timer.set_value(None);
        refocus.set_value(back);
        armed.set(false);
    };
    let arm = move || {
        armed_at.set_value(js_sys::Date::now());
        armed.set(true);
        if let Ok(h) = set_timeout_with_handle(move || disarm(focus_inside()), ARMED_FOR) {
            timer.set_value(Some(h));
        }
    };
    on_cleanup(move || {
        if let Some(h) = timer.get_value() {
            h.clear();
        }
    });
    // The armed button takes focus as it appears, so Esc and focus-out mean
    // something from the first moment. The pair is wider than the button it
    // replaced, and in a table card that scrolls sideways the question could
    // land past the edge: it is brought into view with it.
    Effect::new(move |_| {
        if let Some(b) = armed_btn.get() {
            let _ = b.focus();
            let opts = web_sys::ScrollIntoViewOptions::new();
            opts.set_block(web_sys::ScrollLogicalPosition::Nearest);
            opts.set_inline(web_sys::ScrollLogicalPosition::Nearest);
            if let Some(w) = wrap.get_untracked() {
                w.scroll_into_view_with_scroll_into_view_options(&opts);
            }
        }
    });
    Effect::new(move |_| {
        if let Some(b) = rest_btn.get() {
            if refocus.get_value() {
                refocus.set_value(false);
                let _ = b.focus();
            }
        }
    });

    let tip = title.map(|t| move || t.get().to_string());
    view! {
        <span
            class="confirm"
            node_ref=wrap
            // A confirm inside a clickable row (a chat thread) must not also
            // open the row.
            on:click=|ev| ev.stop_propagation()
            on:pointerdown=move |_| pressing.set_value(true)
            on:pointerup=move |_| pressing.set_value(false)
            on:keydown=move |ev| {
                if armed.get_untracked() && ev.key() == "Escape" {
                    // Inside a dialog, Esc would close the whole modal too.
                    ev.prevent_default();
                    ev.stop_propagation();
                    disarm(true);
                }
            }
            on:focusout=move |ev| {
                if !armed.get_untracked() || pressing.get_value() {
                    return;
                }
                // Only focus leaving the armed pair counts. The resting button
                // reports one too as the pair replaces it (Chrome blurs a
                // focused element on its way out of the document), and that
                // is our own swap, not the owner moving on.
                let from = ev.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok());
                let from_pair = from.is_some_and(|n| {
                    [armed_btn.get_untracked(), cancel_btn.get_untracked()]
                        .into_iter()
                        .flatten()
                        .any(|b| n.is_same_node(Some(&b)))
                });
                if !from_pair {
                    return;
                }
                let to = ev.related_target().and_then(|t| t.dyn_into::<web_sys::Node>().ok());
                let inside = match (wrap.get_untracked(), to) {
                    (Some(w), Some(n)) => w.contains(Some(&n)),
                    _ => false,
                };
                if !inside {
                    disarm(false);
                }
            }
        >
            <Show
                when=move || armed.get()
                fallback=move || {
                    let tip = tip.clone();
                    view! {
                        <button
                            type="button"
                            class=class
                            node_ref=rest_btn
                            title=tip
                            disabled=move || disabled.get()
                            on:click=move |_| {
                                pressing.set_value(false);
                                if !disabled.get_untracked() {
                                    arm();
                                }
                            }
                        >
                            {icon.clone().map(|i| i.run())}
                            {let label = label.clone(); move || label.get()}
                        </button>
                    }
                }
            >
                <button
                    type="button"
                    class=armed_class
                    node_ref=armed_btn
                    title="Esc or ✕ cancels"
                    disabled=move || disabled.get()
                    on:click=move |ev| {
                        pressing.set_value(false);
                        let since = js_sys::Date::now() - armed_at.get_value();
                        if is_second_press(ev.detail(), since) {
                            return;
                        }
                        disarm(false);
                        on_confirm.run(());
                    }
                >
                    {let confirm = confirm.clone(); move || confirm.get()}
                    " ✓"
                </button>
                <button
                    type="button"
                    class=cancel_class
                    node_ref=cancel_btn
                    title="Cancel"
                    on:click=move |_| {
                        pressing.set_value(false);
                        disarm(true);
                    }
                >
                    "✕"
                </button>
            </Show>
        </span>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_double_click_does_not_confirm_what_its_first_press_armed() {
        // the second press of a double-click
        assert!(is_second_press(2, 120.0));
        // a triple-click's third, even late
        assert!(is_second_press(3, 900.0));
        // two single clicks in quick succession, or a key held down
        assert!(is_second_press(1, 150.0));
        assert!(is_second_press(0, 30.0));
        // a deliberate second click, by mouse or by Enter
        assert!(!is_second_press(1, SETTLE_MS));
        assert!(!is_second_press(0, 1500.0));
    }
}
