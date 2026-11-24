//! Rows that hold typed text: [`EntryRow`] (one line, libadwaita's entry
//! row), [`TextRow`] (several lines under the title) and [`NumberRow`], a
//! number that may also be left blank — "unset, the program's own default",
//! which a [`super::SpinRow`] cannot say.
//!
//! All three keep their value as text, the form's own currency: what is
//! typed and not (yet) valid stays as typed, and the caller's `error` says
//! why under the title. Rules: kit.css, "preference rows".

use leptos::prelude::*;

use super::super::icon::{Ico, Icon};
use super::{is_on, RowText, Spin};

/// A title and a one-line text field at the row's end (or, `wide`, under
/// the title across the row: a path, a command). `children` sit after the
/// field (a Browse button).
#[component]
pub fn EntryRow(
    #[prop(into)] title: TextProp,
    #[prop(optional, into)] sub: Option<TextProp>,
    value: RwSignal<String>,
    #[prop(optional, into)] placeholder: Option<TextProp>,
    /// In the monospace face: an id, a path, a flag.
    #[prop(optional)]
    mono: bool,
    /// The field under the title, as wide as the row.
    #[prop(optional)]
    wide: bool,
    #[prop(optional, into)] dirty: Option<Signal<bool>>,
    #[prop(optional, into)] error: Option<Signal<Option<String>>>,
    /// Greyed out, and the field is out of the Tab order: a value nothing
    /// would read for this row.
    #[prop(into, default = Signal::derive(|| false))]
    disabled: Signal<bool>,
    /// The request field it sets, after the title.
    #[prop(optional)]
    wire: Option<&'static str>,
    /// The field's accessible name when the title alone is not specific
    /// enough ("Device name" under "Name").
    #[prop(optional, into)]
    aria_label: Option<TextProp>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let bad = move || error.is_some_and(|e| e.with(Option::is_some));
    let label = aria_label.unwrap_or_else(|| title.clone());
    view! {
        <div
            class="prow entry"
            class:stack=wide
            class:dirty=move || is_on(dirty)
            class:invalid=bad
            class:off=move || disabled.get()
        >
            <RowText title=title sub=sub err=error.unwrap_or_else(|| Signal::stored(None)) wire=wire/>
            <div class="prow-end">
                <input
                    class="prow-entry"
                    class:mono=mono
                    spellcheck="false"
                    aria-label=move || label.get().to_string()
                    aria-invalid=move || bad().then_some("true")
                    placeholder=move || placeholder.as_ref().map(|p| p.get().to_string())
                    disabled=move || disabled.get()
                    prop:value=move || value.get()
                    on:input=move |ev| value.set(event_target_value(&ev))
                />
                {children.map(|c| c())}
            </div>
        </div>
    }
}

/// A title and a text box of several lines under it ("one argument per
/// line", a JSON object).
#[component]
pub fn TextRow(
    #[prop(into)] title: TextProp,
    #[prop(optional, into)] sub: Option<TextProp>,
    value: RwSignal<String>,
    #[prop(optional, into)] placeholder: Option<TextProp>,
    #[prop(optional, into)] dirty: Option<Signal<bool>>,
    #[prop(optional, into)] error: Option<Signal<Option<String>>>,
    /// Rendered after the subtitle (a link to where an inherited value is set).
    #[prop(optional, into)]
    extra: Option<ViewFn>,
    /// The request field it sets, after the title.
    #[prop(optional)]
    wire: Option<&'static str>,
    /// In the monospace face: code, JSON, a template, arguments one per line.
    /// Prose (a prompt, a sentence) is in the text face.
    #[prop(optional)]
    mono: bool,
    /// Rendered under the text box (notes about what is in it).
    #[prop(optional, into)]
    after: Option<ViewFn>,
    /// The box's height in lines; unset, the box has its stylesheet's height.
    #[prop(optional, into)]
    rows: Option<Signal<u32>>,
) -> impl IntoView {
    let bad = move || error.is_some_and(|e| e.with(Option::is_some));
    let label = title.clone();
    view! {
        <div class="prow entry stack" class:dirty=move || is_on(dirty) class:invalid=bad>
            <RowText title=title sub=sub err=error.unwrap_or_else(|| Signal::stored(None)) wire=wire/>
            {extra.map(|x| view! { <div class="prow-extra">{x.run()}</div> })}
            <textarea
                class="prow-area"
                class:mono=mono
                spellcheck="false"
                rows=move || rows.map(|r| r.get().to_string())
                aria-label=move || label.get().to_string()
                placeholder=move || placeholder.as_ref().map(|p| p.get().to_string())
                prop:value=move || value.get()
                on:input=move |ev| value.set(event_target_value(&ev))
            ></textarea>
            {after.map(|x| view! { <div class="prow-extra prow-after">{x.run()}</div> })}
        </div>
    }
}

/// What a [`NumberRow`] shows for `text`: a whole number grouped the
/// spin's way, anything else as it is (a decimal as stored, typed text that
/// is not a number yet).
pub fn number_shown(spin: &Spin, text: &str) -> String {
    let t = text.trim();
    match t.parse::<i64>() {
        Ok(n) if spin.digits == 0 => spin.show_whole(n),
        _ => t.to_string(),
    }
}

/// `v` as a [`NumberRow`] stores it: a whole number without marks, a
/// decimal to the spin's digits at most, without trailing zeros.
pub fn number_text(spin: &Spin, v: f64) -> String {
    if spin.digits == 0 {
        return format!("{}", v.round() as i64);
    }
    let s = format!("{v:.*}", spin.digits);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// What a [`NumberRow`] stores for typed `text`: blank stays blank (unset),
/// a number the spin reads is stored plainly ("131,072" → "131072"), and
/// anything else is kept as typed for the caller's error to name.
pub fn number_typed(spin: &Spin, text: &str) -> String {
    let t = text.trim();
    if t.is_empty() {
        return String::new();
    }
    if spin.digits == 0 {
        // Read as an i64, not an f64: every digit stays, past 2^53 too.
        return spin
            .read_whole(t)
            .map_or_else(|_| t.to_string(), |n| n.to_string());
    }
    match spin.read(t) {
        // A decimal keeps every digit that was typed: the spin's digits
        // only round what − and + make.
        Ok(v) => {
            let s = v.to_string();
            if s == "-0" {
                "0".to_string()
            } else {
                s
            }
        }
        Err(_) => t.to_string(),
    }
}

/// What a [`NumberRow`]'s − (`up` false) or + makes of `text`: blank counts
/// as 0, a whole number steps exactly (an `i64`, never through an `f64`),
/// and text that is not a number, or a step past the `i64` range, does
/// nothing (`None`).
pub fn number_stepped(spin: &Spin, text: &str, up: bool) -> Option<String> {
    let t = text.trim();
    if spin.digits == 0 {
        let base = if t.is_empty() {
            0
        } else {
            spin.read_whole(t).ok()?
        };
        let step = (spin.step.round() as i64).max(1);
        let n = if up {
            base.checked_add(step)
        } else {
            base.checked_sub(step)
        }?;
        return Some(n.to_string());
    }
    let base = if t.is_empty() {
        0.0
    } else {
        spin.read(t).ok()?
    };
    let d = if up { spin.step } else { -spin.step };
    Some(number_text(spin, base + d))
}

/// A number field that may be left blank (the placeholder says what blank
/// means: "llama.cpp default"), with − and + when `buttons`. The value is
/// text: blank, a number, or what was typed and does not read as one, which
/// stays for the caller's `error`. No bounds: the caller validates, and
/// nothing typed is moved to a bound.
#[component]
pub fn NumberRow(
    #[prop(into)] title: TextProp,
    #[prop(optional, into)] sub: Option<TextProp>,
    value: RwSignal<String>,
    /// How it reads and steps; `digits > 0` is a decimal field.
    #[prop(optional)]
    spin: Spin,
    #[prop(optional, into)] placeholder: Option<TextProp>,
    #[prop(optional, into)] unit: Option<TextProp>,
    /// − and + beside the field.
    #[prop(optional)]
    buttons: bool,
    #[prop(optional, into)] dirty: Option<Signal<bool>>,
    #[prop(optional, into)] error: Option<Signal<Option<String>>>,
    /// The request field it sets, after the title.
    #[prop(optional)]
    wire: Option<&'static str>,
) -> impl IntoView {
    let bad = move || error.is_some_and(|e| e.with(Option::is_some));
    let label = title.clone();
    let step = move |up: bool| {
        if let Some(v) = value.with_untracked(|now| number_stepped(&spin, now, up)) {
            value.set(v);
        }
    };
    view! {
        <div class="prow" class:dirty=move || is_on(dirty) class:invalid=bad>
            <RowText title=title sub=sub err=error.unwrap_or_else(|| Signal::stored(None)) wire=wire/>
            <div class="spin" on:click=|ev| ev.stop_propagation()>
                <input
                    class="spin-field"
                    class:invalid=bad
                    inputmode="decimal"
                    spellcheck="false"
                    aria-label=move || label.get().to_string()
                    aria-invalid=move || bad().then_some("true")
                    placeholder=move || placeholder.as_ref().map(|p| p.get().to_string())
                    prop:value=move || number_shown(&spin, &value.get())
                    on:change=move |ev| {
                        let typed = number_typed(&spin, &event_target_value(&ev));
                        if typed == value.get_untracked() {
                            // The same value typed another way: shown again.
                            value.update(|_| {});
                        } else {
                            value.set(typed);
                        }
                    }
                />
                {unit.map(|u| view! { <span class="spin-unit">{move || u.get()}</span> })}
                {buttons
                    .then(|| {
                        view! {
                            <button type="button" class="spin-btn" title="Less" on:click=move |_| step(false)>
                                <Icon ico=Ico::Minus/>
                            </button>
                            <button type="button" class="spin-btn" title="More" on:click=move |_| step(true)>
                                <Icon ico=Ico::Plus/>
                            </button>
                        }
                    })}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHOLE: Spin = Spin {
        step: 1.0,
        min: None,
        max: None,
        digits: 0,
    };
    const DEC: Spin = Spin {
        step: 0.05,
        min: None,
        max: None,
        digits: 2,
    };

    #[test]
    fn a_number_row_stores_what_was_typed_plainly() {
        assert_eq!(number_typed(&WHOLE, "131,072"), "131072");
        assert_eq!(number_typed(&WHOLE, "131\u{2009}072"), "131072");
        assert_eq!(number_typed(&WHOLE, "  "), "");
        assert_eq!(number_typed(&WHOLE, "-1"), "-1");
        // Not a number, or not whole: kept for the caller's error.
        assert_eq!(number_typed(&WHOLE, "auto"), "auto");
        assert_eq!(number_typed(&WHOLE, "0.5"), "0.5");
        assert_eq!(number_typed(&DEC, "0,15"), "0.15");
        assert_eq!(number_typed(&DEC, "0.125"), "0.125");
        assert_eq!(number_typed(&DEC, "1"), "1");
    }

    #[test]
    fn a_number_row_shows_whole_numbers_grouped() {
        assert_eq!(number_shown(&WHOLE, "131072"), "131\u{2009}072");
        assert_eq!(number_shown(&WHOLE, ""), "");
        assert_eq!(number_shown(&WHOLE, "abc"), "abc");
        assert_eq!(number_shown(&DEC, "0.15"), "0.15");
    }

    #[test]
    fn a_whole_number_row_keeps_every_digit_past_two_to_the_53() {
        // 2^53 + 1: through an f64 it would come back as ...992.
        let big = "9007199254740993";
        assert_eq!(number_typed(&WHOLE, "9,007,199,254,740,993"), big);
        assert_eq!(number_typed(&WHOLE, big), big);
        assert_eq!(
            number_shown(&WHOLE, big),
            "9\u{2009}007\u{2009}199\u{2009}254\u{2009}740\u{2009}993"
        );
        assert_eq!(
            number_stepped(&WHOLE, big, true).as_deref(),
            Some("9007199254740994")
        );
        assert_eq!(
            number_stepped(&WHOLE, big, false).as_deref(),
            Some("9007199254740992")
        );
        assert_eq!(
            number_typed(&WHOLE, "9223372036854775807"),
            "9223372036854775807"
        );
        // Past the range: nothing moves, and typed text stays for the error.
        assert_eq!(number_stepped(&WHOLE, "9223372036854775807", true), None);
        assert_eq!(
            number_typed(&WHOLE, "9223372036854775808"),
            "9223372036854775808"
        );
    }

    #[test]
    fn a_number_row_steps_from_blank_and_not_from_text() {
        assert_eq!(number_stepped(&WHOLE, "", true).as_deref(), Some("1"));
        assert_eq!(number_stepped(&WHOLE, " ", false).as_deref(), Some("-1"));
        assert_eq!(number_stepped(&WHOLE, "auto", true), None);
        assert_eq!(number_stepped(&DEC, "0.15", true).as_deref(), Some("0.2"));
        assert_eq!(number_stepped(&DEC, "", false).as_deref(), Some("-0.05"));
    }

    #[test]
    fn a_number_rows_steps_round_to_its_digits() {
        assert_eq!(number_text(&DEC, 0.15 + 0.05), "0.2");
        assert_eq!(number_text(&DEC, 0.1 + 0.2), "0.3");
        assert_eq!(number_text(&DEC, -0.001), "0");
        assert_eq!(number_text(&WHOLE, 4096.0), "4096");
    }
}
