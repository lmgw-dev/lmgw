//! Settings — the row every setting is drawn in: a preference row
//! (kit.css, "preference rows") with its title, a tag after it (a unit that
//! is no number's, "restart to apply", "stored"), the hint as its subtitle,
//! why the value is refused and what is off about it, and the control at
//! the row's end, or, `stack`, under the title across the row.
//!
//! [`SetField`] takes what the kit's `Field` takes, so the settings' own
//! field components (the voice, the speech style, the forge tokens, …) draw
//! as rows by naming it in `Field`'s place. The kit's rows hold no anchor,
//! search visibility or pinned lock, which every setting row has; these do.

use leptos::prelude::*;

use super::{bridge, Ctl, Def};
use crate::widgets::prefs::{number_shown, number_typed};
use crate::widgets::{FormState, Spin, Switch};

/// `hint` as a subtitle: its first letter upper case (the table writes
/// hints as fragments: "0 = unlimited", "per-model overrides win").
pub(super) fn sentence(hint: &str) -> String {
    let mut c = hint.chars();
    match c.next() {
        Some(f) if f.is_lowercase() => f.to_uppercase().chain(c).collect(),
        _ => hint.to_string(),
    }
}

/// A setting's row. `stack` puts the control under the title, across the
/// row; otherwise it sits at the row's end. `class` adds to the row's
/// (`combo`, `entry`); `on_click` makes the whole row act (a switch's).
#[component]
pub(super) fn SetField(
    #[prop(into)] label: TextProp,
    #[prop(optional, into)] hint: Option<TextProp>,
    /// After the title, dimmed: what the value is in, when no number's
    /// field says it, or that it applies at a restart.
    #[prop(optional, into)]
    unit: Option<TextProp>,
    /// The kit's `Field` spans its grid with it; a row is always the box's
    /// width, so it only stacks the control.
    #[prop(optional)]
    wide: bool,
    /// The control at the row's end rather than under the title.
    #[prop(optional)]
    inline: bool,
    #[prop(optional, into)] dirty: Option<Signal<bool>>,
    #[prop(optional, into)] error: Option<Signal<Option<String>>>,
    #[prop(optional, into)] warn: Option<Signal<Option<String>>>,
    #[prop(optional, into)] id: Option<String>,
    #[prop(optional, into)] hidden: Option<Signal<bool>>,
    /// After the subtitle's text, inside it (a link to where a value is set).
    #[prop(optional, into)]
    hint_extra: Option<ViewFn>,
    #[prop(optional)] class: &'static str,
    #[prop(optional)] on_click: Option<Callback<()>>,
    children: Children,
) -> impl IntoView {
    let stack = wide || !inline;
    let is_dirty = move || dirty.is_some_and(|d| d.get());
    let msg = move || error.and_then(|e| e.get());
    let note = move || warn.and_then(|w| w.get());
    let gone = move || hidden.is_some_and(|h| h.get());
    let row_class = if class.is_empty() {
        "prow set-row".to_string()
    } else {
        format!("prow set-row {class}")
    };
    let sub = move || {
        let t = hint
            .as_ref()
            .map(|h| sentence(&h.get()))
            .unwrap_or_default();
        (!t.is_empty() || hint_extra.is_some()).then(|| {
            view! {
                <span class="prow-sub">
                    {t} {hint_extra.as_ref().map(|x| view! { " " {x.run()} })}
                </span>
            }
        })
    };
    view! {
        <div
            class=row_class
            class:stack=stack
            class:action=on_click.is_some()
            class:dirty=is_dirty
            class:invalid=move || msg().is_some()
            class:warned=move || msg().is_none() && note().is_some()
            id=id
            hidden=gone
            on:click=move |_| {
                if let Some(c) = on_click {
                    c.run(());
                }
            }
        >
            <div class="prow-text">
                <span class="prow-line">
                    <span class="prow-title">{move || label.get()}</span>
                    {unit
                        .map(|u| {
                            move || {
                                let t = u.get();
                                (!t.is_empty())
                                    .then(|| view! { <span class="prow-tag">{t.to_string()}</span> })
                            }
                        })}
                </span>
                {sub}
                {move || msg().map(|m| view! { <span class="prow-err" role="alert">{m}</span> })}
                {move || note().map(|m| view! { <span class="prow-warn" role="note">{m}</span> })}
            </div>
            <div class="prow-end">{children()}</div>
        </div>
    }
}

/// How a number setting reads, steps and shows: whole, or with the decimals
/// of a ratio or an amount of money.
pub(super) fn spin_of(ctl: Ctl) -> Spin {
    let digits = match ctl {
        Ctl::Float(..) | Ctl::Money => 2,
        _ => 0,
    };
    Spin {
        digits,
        ..Spin::default()
    }
}

/// A number setting's field: the kit's spin field without − and +, its
/// value the form's text. Typed text that is no number stays as typed, for
/// the form's checks to refuse (the row says why, Save waits).
pub(super) fn number_field(
    form: FormState,
    d: &'static Def,
    unit: impl Fn() -> String + Send + Sync + 'static,
    locked: Signal<bool>,
) -> impl IntoView {
    let k = d.key;
    let spin = spin_of(d.ctl);
    let field: NodeRef<leptos::html::Input> = NodeRef::new();
    // What is being typed, as typed: the draft gets it on every keystroke
    // (so the dirty guard sees it before the field is left), while the
    // field keeps showing the typed text until `change` shows it the
    // field's way.
    let raw = RwSignal::new(None::<String>);
    view! {
        <div class="spin" on:click=|ev| ev.stop_propagation()>
            <input
                class="spin-field"
                class:invalid=move || form.error(k).is_some()
                node_ref=field
                inputmode="decimal"
                spellcheck="false"
                aria-label=d.label
                placeholder=d.ph
                disabled=move || locked.get()
                prop:value=move || {
                    let text = form.text(k);
                    match raw.get() {
                        Some(r) if number_typed(&spin, &r) == text => r,
                        _ => number_shown(&spin, &text),
                    }
                }
                on:input=move |ev| {
                    let r = event_target_value(&ev);
                    form.set_text(k, number_typed(&spin, &r));
                    raw.set(Some(r));
                }
                on:change=move |ev| {
                    raw.set(None);
                    let typed = number_typed(&spin, &event_target_value(&ev));
                    if typed == form.text(k) {
                        // The same value typed another way: shown the field's way.
                        if let Some(f) = field.get_untracked() {
                            f.set_value(&number_shown(&spin, &typed));
                        }
                    } else {
                        form.set_text(k, typed);
                    }
                }
            />
            <span class="spin-unit">{move || unit()}</span>
        </div>
    }
}

/// A one-line text setting's field.
pub(super) fn entry_field(
    form: FormState,
    d: &'static Def,
    mono: bool,
    locked: Signal<bool>,
) -> impl IntoView {
    let k = d.key;
    view! {
        <input
            class="prow-entry"
            class:mono=mono
            spellcheck="false"
            aria-label=d.label
            placeholder=d.ph
            disabled=move || locked.get()
            prop:value=move || form.text(k)
            on:click=|ev| ev.stop_propagation()
            on:input=move |ev| form.set_text(k, event_target_value(&ev))
        />
    }
}

/// A box of several lines (one per line, `name = value` lines).
pub(super) fn lines_field(
    form: FormState,
    d: &'static Def,
    mono: bool,
    locked: Signal<bool>,
) -> impl IntoView {
    let k = d.key;
    // As many rows as there are lines, and one to type into.
    let rows = move || (form.text(k).lines().count() + 1).max(2).to_string();
    view! {
        <textarea
            class="prow-area set-area"
            class:mono=mono
            rows=rows
            spellcheck="false"
            aria-label=d.label
            placeholder=d.ph
            disabled=move || locked.get()
            prop:value=move || form.text(k)
            on:input=move |ev| form.set_text(k, event_target_value(&ev))
        ></textarea>
    }
}

/// A form flag as the kit switch's signal, kept in step both ways (Discard
/// or a rebase moves the switch, a flip moves the draft).
pub(in crate::pages::settings) fn bridge_flag(
    form: FormState,
    key: &'static str,
) -> RwSignal<bool> {
    let sig = RwSignal::new(form.flag(key));
    Effect::new(move |_| {
        let f = form.flag(key);
        if sig.get_untracked() != f {
            sig.set(f);
        }
    });
    Effect::new(move |_| {
        let v = sig.get();
        if form.flag(key) != v {
            form.set_flag(key, v);
        }
    });
    sig
}

/// A switch setting's switch.
pub(super) fn switch_field(form: FormState, d: &'static Def, locked: Signal<bool>) -> AnyView {
    let value = bridge_flag(form, d.key);
    view! { <Switch value=value label=d.label disabled=locked/> }.into_any()
}

/// A choice's dropdown, flat until hovered (the row is a `combo`).
pub(super) fn choice_field(
    form: FormState,
    d: &'static Def,
    options: Signal<Vec<(String, String)>>,
    placeholder: Option<&'static str>,
    locked: Signal<bool>,
) -> AnyView {
    let value = bridge(form, d.key);
    match placeholder {
        Some(p) => view! { <crate::widgets::Select value=value options=options placeholder=p disabled=locked/> }
            .into_any(),
        None => {
            view! { <crate::widgets::Select value=value options=options disabled=locked/> }.into_any()
        }
    }
}
