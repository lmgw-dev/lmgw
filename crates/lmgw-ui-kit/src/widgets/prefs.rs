//! Boxed preference groups and their rows, the two-line list row and the
//! save toast (docs/design/ui-rebuild/modern-sample.html): the libadwaita
//! shapes the modern design is built from.
//!
//! A [`PrefGroup`] is a heading, an optional sentence and a box of rows.
//! Each row is a title with an optional subtitle on the left and its control
//! on the right: [`SwitchRow`], [`SpinRow`], [`ComboRow`], [`InfoRow`],
//! [`ActionRow`] (the whole row is the button, `danger` paints it red),
//! and for typed text [`EntryRow`], [`TextRow`] and [`NumberRow`].
//! A row's `dirty` puts the accent dot after its title; the page's
//! [`SaveToast`] says how many there are and offers Save and Discard.
//!
//! The rows hold plain signals, so a page wires them to whatever keeps its
//! state (a `FormState`, a draft struct). Rules: kit.css, "preference rows".

use leptos::prelude::*;

use super::icon::{Ico, Icon};
use super::Select;

/// The rows that hold typed text: one line, several, a number or blank.
mod entry;
/// Whole numbers read and shown exactly, past what an `f64` holds.
mod whole;
pub use entry::{
    number_shown, number_stepped, number_text, number_typed, EntryRow, NumberRow, TextRow,
};

/// A heading, an optional sentence under it, and the box of rows.
#[component]
pub fn PrefGroup(
    #[prop(optional, into)] title: Option<TextProp>,
    #[prop(optional, into)] desc: Option<TextProp>,
    /// Controls on the heading's line, at its end.
    #[prop(optional, into)]
    suffix: Option<ViewFn>,
    /// How many rows the group holds (or any short figure), after the title.
    #[prop(optional, into)]
    count: Option<Signal<String>>,
    children: Children,
) -> impl IntoView {
    let head = (title.is_some() || suffix.is_some() || count.is_some()).then(|| {
        view! {
            <div class="pref-head">
                {title.map(|t| view! { <h2>{move || t.get()}</h2> })}
                {count.map(|c| view! { <span class="pref-count">{move || c.get()}</span> })}
                {suffix.map(|s| view! { <span class="pref-suffix">{s.run()}</span> })}
            </div>
        }
    });
    view! {
        <section class="pref-group">
            {head}
            {desc.map(|d| view! { <p class="pref-desc">{move || d.get()}</p> })}
            <div class="boxed">{children()}</div>
        </section>
    }
}

/// A request field's wire name, as a row shows it after its title: secondary
/// detail beside the plain label, never the label itself. It is a line of its
/// own under the title, whole on one line: a name wider than the row ends in
/// "…", and its tooltip is the whole name. A literal or an owned string.
#[component]
pub fn WireName(#[prop(into)] name: Oco<'static, str>) -> impl IntoView {
    let title = name.clone();
    view! { <i class="fname" title=move || title.to_string()>{name}</i> }
}

/// The title and subtitle every row starts with, and under them why the
/// row's value is refused (`err`), in red.
#[component]
pub(crate) fn RowText(
    title: TextProp,
    sub: Option<TextProp>,
    #[prop(optional)] err: Option<Signal<Option<String>>>,
    /// The request field's name after the title (`wire`), quiet and in the
    /// monospace face, breakable after each `.` and `_`.
    #[prop(default = None)]
    wire: Option<&'static str>,
) -> impl IntoView {
    view! {
        <div class="prow-text">
            <span class="prow-title">
                {move || title.get()}
                {wire.map(|w| view! { " " <WireName name=w/> })}
            </span>
            {sub.map(|s| {
                move || {
                    let t = s.get();
                    (!t.is_empty()).then(|| view! { <span class="prow-sub">{t.to_string()}</span> })
                }
            })}
            {err.map(|e| {
                move || {
                    e.get().map(|m| view! { <span class="prow-err" role="alert">{m}</span> })
                }
            })}
        </div>
    }
}

fn is_on(s: Option<Signal<bool>>) -> bool {
    s.is_some_and(|s| s.get())
}

/// A row with any control at its end, or none; with `on_click` the whole
/// row is a button (activatable by the keyboard too).
#[component]
pub fn ActionRow(
    #[prop(into)] title: TextProp,
    #[prop(optional, into)] sub: Option<TextProp>,
    #[prop(optional, into)] dirty: Option<Signal<bool>>,
    /// Red title: an action that deletes or cannot be undone.
    #[prop(optional)]
    danger: bool,
    /// A chevron at the end: the row leads somewhere.
    #[prop(optional)]
    chevron: bool,
    #[prop(optional, into)] on_click: Option<Callback<()>>,
    /// Dimmed and inert: it cannot act now (its subtitle says why). It leaves
    /// the Tab order and ignores clicks and keys.
    #[prop(optional, into)]
    disabled: Option<Signal<bool>>,
    /// The request field it sets, after the title.
    #[prop(optional)]
    wire: Option<&'static str>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let action = on_click.is_some();
    view! {
        <div
            class="prow"
            class:action=action
            class:danger=danger
            class:dirty=move || is_on(dirty)
            class:off=move || is_on(disabled)
            role=action.then_some("button")
            tabindex=move || (action && !is_on(disabled)).then_some("0")
            aria-disabled=move || is_on(disabled).then_some("true")
            on:click=move |_| {
                if let Some(c) = on_click {
                    if !is_on(disabled) {
                        c.run(());
                    }
                }
            }
            on:keydown=move |ev| {
                if let Some(c) = on_click {
                    if !is_on(disabled) && matches!(ev.key().as_str(), "Enter" | " ") {
                        ev.prevent_default();
                        c.run(());
                    }
                }
            }
        >
            <RowText title=title sub=sub wire=wire/>
            {children.map(|c| view! { <div class="prow-end">{c()}</div> })}
            {chevron.then(|| view! { <Icon ico=Ico::Right class="prow-chevron"/> })}
        </div>
    }
}

/// A row whose content goes under its title across the row's width (a
/// list of chips, a table, a player, a sentence), or with no title, a
/// padded block of the box: what a boxed group holds besides its rows.
#[component]
pub fn StackRow(
    #[prop(optional, into)] title: Option<TextProp>,
    #[prop(optional, into)] sub: Option<TextProp>,
    /// The request field it sets, after the title.
    #[prop(optional)]
    wire: Option<&'static str>,
    #[prop(optional, into)] dirty: Option<Signal<bool>>,
    /// Extra classes on the row (a page's own hook).
    #[prop(optional)]
    class: &'static str,
    children: Children,
) -> impl IntoView {
    view! {
        <div class=format!("prow stack block {class}") class:dirty=move || is_on(dirty)>
            {title.map(|t| view! { <RowText title=t sub=sub wire=wire/> })}
            <div class="prow-block">{children()}</div>
        </div>
    }
}

/// A title and the value it has, read only.
#[component]
pub fn InfoRow(
    #[prop(into)] title: TextProp,
    #[prop(into)] value: TextProp,
    /// The value in the monospace face (a path, an id).
    #[prop(optional)]
    mono: bool,
) -> impl IntoView {
    let shown = value.clone();
    view! {
        <div class="prow">
            <RowText title=title sub=None/>
            <span class="prow-value" class:mono=mono title=move || shown.get().to_string()>
                {move || value.get()}
            </span>
        </div>
    }
}

/// The on/off switch.
#[component]
pub fn Switch(
    value: RwSignal<bool>,
    /// What it switches, for assistive technology.
    #[prop(optional, into)]
    label: Option<TextProp>,
    #[prop(into, default = Signal::derive(|| false))] disabled: Signal<bool>,
) -> impl IntoView {
    view! {
        <button
            type="button"
            class="switch"
            class:on=move || value.get()
            role="switch"
            aria-checked=move || value.get().to_string()
            aria-label=move || label.as_ref().map(|l| l.get().to_string())
            disabled=move || disabled.get()
            on:click=move |ev| {
                // A switch in a row: the row is not clicked as well.
                ev.stop_propagation();
                value.update(|v| *v = !*v);
            }
        ></button>
    }
}

/// A row with a switch; a click anywhere on the row flips it.
#[component]
pub fn SwitchRow(
    #[prop(into)] title: TextProp,
    #[prop(optional, into)] sub: Option<TextProp>,
    value: RwSignal<bool>,
    #[prop(optional, into)] dirty: Option<Signal<bool>>,
    #[prop(into, default = Signal::derive(|| false))] disabled: Signal<bool>,
    /// The request field it sets, after the title.
    #[prop(optional)]
    wire: Option<&'static str>,
) -> impl IntoView {
    let label = title.clone();
    view! {
        <div
            class="prow action"
            class:dirty=move || is_on(dirty)
            class:off=move || disabled.get()
            on:click=move |_| {
                if !disabled.get_untracked() {
                    value.update(|v| *v = !*v);
                }
            }
        >
            <RowText title=title sub=sub wire=wire/>
            <Switch value=value label=label disabled=disabled/>
        </div>
    }
}

/// Bounds and step of a [`SpinButton`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spin {
    pub step: f64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Digits after the point.
    pub digits: usize,
}

impl Default for Spin {
    fn default() -> Self {
        Self {
            step: 1.0,
            min: None,
            max: None,
            digits: 0,
        }
    }
}

impl Spin {
    fn clamp(&self, v: f64) -> f64 {
        let v = self.min.map_or(v, |m| v.max(m));
        self.max.map_or(v, |m| v.min(m))
    }

    /// The value as the field shows it: thousands grouped by a thin space,
    /// which reads at any locale and parses back.
    pub fn show(&self, v: f64) -> String {
        let s = format!("{v:.*}", self.digits);
        let (int, frac) = s
            .split_once('.')
            .map_or((s.as_str(), None), |(i, f)| (i, Some(f)));
        let grouped = group(int);
        match frac {
            Some(f) => format!("{grouped}.{f}"),
            None => grouped,
        }
    }

    /// What was typed, as a number, or `None` ([`Spin::read`] says why).
    pub fn parse(&self, text: &str) -> Option<f64> {
        self.read(text).ok()
    }

    /// What was typed, as a number, the bounds aside. Spaces, thin spaces and
    /// apostrophes are group marks and ignored. A comma or a point is read
    /// as people type it:
    /// - in a spin without decimals, one followed by exactly three digits is
    ///   a group mark ("131,072" and "131.072" are 131072), and the value
    ///   must be whole;
    /// - in a spin with decimals, the last comma or point is the decimal mark
    ///   when it is the only one ("0,5") or when the other one groups
    ///   ("1,024.5", "1.024,5"); the same mark typed more than once groups.
    pub fn read(&self, text: &str) -> Result<f64, SpinError> {
        let v = self
            .plain(text)?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or(SpinError::NotANumber)?;
        if self.digits == 0 && v.fract() != 0.0 {
            return Err(SpinError::NotWhole);
        }
        Ok(v)
    }

    /// What was typed as a plain number ("-131072", "0.5"): the marks read
    /// as [`Spin::read`] says, group marks gone, a point for the decimal.
    fn plain(&self, text: &str) -> Result<String, SpinError> {
        let t: String = text
            .chars()
            .filter(|c| {
                !c.is_whitespace() && !matches!(c, '\u{2009}' | '\u{202F}' | '\'' | '\u{2019}')
            })
            .collect();
        let (neg, body) = match t.strip_prefix(['-', '\u{2212}']) {
            Some(rest) => (true, rest),
            None => (false, t.strip_prefix('+').unwrap_or(&t)),
        };
        let chars: Vec<char> = body.chars().collect();
        if chars.is_empty()
            || !chars
                .iter()
                .all(|c| c.is_ascii_digit() || matches!(c, ',' | '.'))
        {
            return Err(SpinError::NotANumber);
        }
        // A mark followed by exactly three digits, then the end or a mark.
        let groups = |i: usize| {
            chars.len() >= i + 4
                && chars[i + 1..i + 4].iter().all(char::is_ascii_digit)
                && chars.get(i + 4).is_none_or(|c| !c.is_ascii_digit())
        };
        let marks: Vec<usize> = (0..chars.len())
            .filter(|&i| matches!(chars[i], ',' | '.'))
            .collect();
        // The decimal mark's position, if there is one.
        let point = if self.digits == 0 {
            match marks.iter().copied().find(|&i| !groups(i)) {
                // Only a point can be one; a whole spin then refuses the
                // fraction below, rather than reading it as something else.
                Some(i) if chars[i] == '.' && marks.last() == Some(&i) => Some(i),
                Some(_) => return Err(SpinError::NotANumber),
                None => None,
            }
        } else {
            let commas = marks.iter().filter(|&&i| chars[i] == ',').count();
            let points = marks.len() - commas;
            match (commas, points) {
                (0, 0) => None,
                (1, 0) | (0, 1) => marks.first().copied(),
                (_, 0) | (0, _) => None,
                _ => {
                    let last = *marks.last().unwrap_or(&0);
                    let kind = chars[last];
                    if marks.iter().filter(|&&i| chars[i] == kind).count() > 1 {
                        return Err(SpinError::NotANumber);
                    }
                    Some(last)
                }
            }
        };
        let mut num = String::with_capacity(chars.len() + 1);
        if neg {
            num.push('-');
        }
        for (i, c) in chars.iter().enumerate() {
            match c {
                ',' | '.' if Some(i) == point => num.push('.'),
                ',' | '.' if groups(i) => {}
                ',' | '.' => return Err(SpinError::NotANumber),
                d => num.push(*d),
            }
        }
        Ok(num)
    }

    /// What was typed, as a value of this spin: a number within its bounds.
    /// Out of bounds is an error to show, never a value moved to the bound.
    pub fn check(&self, text: &str) -> Result<f64, SpinError> {
        let v = self.read(text)?;
        if let Some(m) = self.min.filter(|m| v < *m) {
            return Err(SpinError::Below(m));
        }
        if let Some(m) = self.max.filter(|m| v > *m) {
            return Err(SpinError::Above(m));
        }
        Ok(v)
    }

    /// Why `text` is not a value, in a sentence for the field.
    pub fn describe(&self, text: &str, e: SpinError) -> String {
        let t = text.trim();
        match e {
            SpinError::NotANumber => format!("\u{201c}{t}\u{201d} is not a number"),
            SpinError::NotWhole => format!("\u{201c}{t}\u{201d} is not a whole number"),
            SpinError::Below(m) => format!("at least {}", self.show(m)),
            SpinError::Above(m) => format!("at most {}", self.show(m)),
        }
    }
}

/// The integer part `int` ("-131072") with its thousands grouped by a thin
/// space, as [`Spin::show`] shows them.
fn group(int: &str) -> String {
    let (sign, digits) = int.strip_prefix('-').map_or(("", int), |d| ("-", d));
    let mut grouped = String::from(sign);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push('\u{2009}');
        }
        grouped.push(c);
    }
    grouped
}

/// Why typed text is not a value of a [`Spin`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SpinError {
    NotANumber,
    /// A fraction in a spin without decimals.
    NotWhole,
    /// Under the spin's floor (the floor).
    Below(f64),
    /// Over the spin's ceiling (the ceiling).
    Above(f64),
}

/// A number field with − and + beside it. Typed text that is not a value
/// of the spin (not a number, out of its bounds) stays in the field, marked
/// invalid with the reason, and the value keeps what it had; − and + stop
/// at the bounds.
#[component]
pub fn SpinButton(
    value: RwSignal<f64>,
    #[prop(optional)] spin: Spin,
    /// After the field, dimmed ("tokens", "s").
    #[prop(optional, into)]
    unit: Option<TextProp>,
    #[prop(into, default = Signal::derive(|| false))] disabled: Signal<bool>,
    /// Why the typed text is not a value, for the caller to show; the field
    /// holds it too, as its tooltip.
    #[prop(optional)]
    invalid: Option<RwSignal<Option<String>>>,
) -> impl IntoView {
    let bad = invalid.unwrap_or_else(|| RwSignal::new(None));
    let step = move |d: f64| {
        bad.set(None);
        value.set(spin.clamp(value.get_untracked() + d));
    };
    let at_min = move || spin.min.is_some_and(|m| value.get() <= m);
    let at_max = move || spin.max.is_some_and(|m| value.get() >= m);
    let field: NodeRef<leptos::html::Input> = NodeRef::new();
    // A value set from elsewhere (a step, Discard) replaces whatever was typed.
    Effect::new(move |_| {
        let v = value.get();
        bad.set(None);
        if let Some(f) = field.get_untracked() {
            f.set_value(&spin.show(v));
        }
    });
    view! {
        <div class="spin" on:click=|ev| ev.stop_propagation()>
            <input
                class="spin-field"
                class:invalid=move || bad.with(Option::is_some)
                node_ref=field
                inputmode="decimal"
                spellcheck="false"
                aria-invalid=move || bad.with(Option::is_some).then_some("true")
                title=move || bad.get()
                disabled=move || disabled.get()
                prop:value=move || spin.show(value.get())
                on:change=move |ev| {
                    let text = event_target_value(&ev);
                    match spin.check(&text) {
                        Ok(v) if v == value.get_untracked() => {
                            bad.set(None);
                            // The same value, typed another way: shown the spin's way.
                            if let Some(f) = field.get_untracked() {
                                f.set_value(&spin.show(v));
                            }
                        }
                        Ok(v) => value.set(v),
                        Err(e) => bad.set(Some(spin.describe(&text, e))),
                    }
                }
            />
            {unit.map(|u| view! { <span class="spin-unit">{move || u.get()}</span> })}
            <button
                type="button"
                class="spin-btn"
                title="Less"
                disabled=move || disabled.get() || at_min()
                on:click=move |_| step(-spin.step)
            >
                <Icon ico=Ico::Minus/>
            </button>
            <button
                type="button"
                class="spin-btn"
                title="More"
                disabled=move || disabled.get() || at_max()
                on:click=move |_| step(spin.step)
            >
                <Icon ico=Ico::Plus/>
            </button>
        </div>
    }
}

/// A row with a number field.
#[component]
pub fn SpinRow(
    #[prop(into)] title: TextProp,
    #[prop(optional, into)] sub: Option<TextProp>,
    value: RwSignal<f64>,
    #[prop(optional)] spin: Spin,
    #[prop(optional, into)] unit: Option<TextProp>,
    #[prop(optional, into)] dirty: Option<Signal<bool>>,
    /// Why the typed text is not a value, for the caller too: refused text
    /// leaves `value` as it was, so a page's Save waits on this (counted
    /// into [`SaveToast`]'s `invalid`) rather than saving the old value.
    #[prop(optional)]
    invalid: Option<RwSignal<Option<String>>>,
    /// The request field it sets, after the title.
    #[prop(optional)]
    wire: Option<&'static str>,
) -> impl IntoView {
    // Typed text the spin refuses: said under the title, until a value lands.
    let bad = invalid.unwrap_or_else(|| RwSignal::new(None::<String>));
    view! {
        <div class="prow" class:dirty=move || is_on(dirty) class:invalid=move || bad.with(Option::is_some)>
            <RowText title=title sub=sub err=Signal::from(bad) wire=wire/>
            {match unit {
                Some(u) => view! { <SpinButton value=value spin=spin unit=u invalid=bad/> }.into_any(),
                None => view! { <SpinButton value=value spin=spin invalid=bad/> }.into_any(),
            }}
        </div>
    }
}

/// A row with a dropdown: the kit's [`Select`], flat until hovered.
#[component]
pub fn ComboRow(
    #[prop(into)] title: TextProp,
    #[prop(optional, into)] sub: Option<TextProp>,
    value: RwSignal<String>,
    #[prop(into)] options: Signal<Vec<(String, String)>>,
    #[prop(optional, into)] dirty: Option<Signal<bool>>,
    /// What the dropdown says while its value is none of the options.
    #[prop(default = "—")]
    placeholder: &'static str,
    /// The request field it sets, after the title.
    #[prop(optional)]
    wire: Option<&'static str>,
) -> impl IntoView {
    view! {
        <div class="prow combo" class:dirty=move || is_on(dirty)>
            <RowText title=title sub=sub wire=wire/>
            <div class="prow-end">
                <Select value=value options=options placeholder=placeholder/>
            </div>
        </div>
    }
}

/// What a [`StateDot`] says. A record's list row and its hero chip show the
/// same `Dot`: resting states are neutral, colour is for a state that needs
/// the eye.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dot {
    /// Up and serving, or ready after a check: green with a soft ring.
    Running,
    /// Ready to start on demand, or at rest and fine: a hollow ring.
    Stopped,
    /// Served from elsewhere (a cloud upstream): a hollow accent ring, so it
    /// never reads as `Busy`'s solid dot in one list.
    Remote,
    /// Something is wrong: red.
    Error,
    /// Switched off: grey.
    Off,
    /// Needs a look: amber, steady.
    Warn,
    /// Anything in progress (a model on its way up or down, a run, a job, a
    /// pull): the steady accent. Nothing in progress pulses amber; amber is
    /// `Warn`.
    Busy,
}

impl Dot {
    /// The dot's classes.
    pub fn class(self) -> &'static str {
        match self {
            Self::Running => "dot run",
            Self::Stopped => "dot stop",
            Self::Remote => "dot cloud",
            Self::Error => "dot err",
            Self::Off => "dot",
            Self::Warn => "dot warn",
            Self::Busy => "dot busy",
        }
    }

    /// The hero chip that goes with this dot.
    pub fn chip_class(self) -> &'static str {
        match self {
            Self::Running => "chip ok",
            Self::Warn => "chip warn",
            Self::Busy => "chip info",
            Self::Stopped | Self::Remote => "chip",
            Self::Error => "chip err",
            Self::Off => "chip off",
        }
    }
}

#[component]
pub fn StateDot(#[prop(into)] dot: Signal<Dot>) -> impl IntoView {
    view! { <span class=move || dot.get().class()></span> }
}

/// A record's state as its hero shows it: the chip of [`Dot::chip_class`]
/// with the same dot its list row has, and the state's word.
#[component]
pub fn StateChip(
    #[prop(into)] dot: Signal<Dot>,
    #[prop(into)] word: TextProp,
    #[prop(optional, into)] title: Option<TextProp>,
) -> impl IntoView {
    view! {
        <span
            class=move || dot.get().chip_class()
            title=move || title.as_ref().map(|t| t.get().to_string())
        >
            <span class=move || dot.get().class()></span>
            {move || word.get()}
        </span>
    }
}

/// The two-line list row: the name, its state (a dot and a word) at the
/// end of the first line, the facts on the second. Selected rows get the
/// soft grey pill, the accent's tint while their list has the focus.
#[component]
pub fn ListRow(
    #[prop(into)] name: TextProp,
    #[prop(optional, into)] meta: Option<TextProp>,
    #[prop(optional, into)] dot: Option<Signal<Dot>>,
    #[prop(optional, into)] state: Option<TextProp>,
    #[prop(optional, into)] selected: Option<Signal<bool>>,
    /// Dimmed: switched off, or set aside.
    #[prop(optional, into)]
    off: Option<Signal<bool>>,
    #[prop(optional, into)] on_click: Option<Callback<()>>,
    /// What the list knows the row by, as `data-key`: a keyboard selection
    /// scrolls it into view by it.
    #[prop(optional, into)]
    key: Option<String>,
    /// An icon before the name (a sidebar's category).
    #[prop(optional)]
    icon: Option<Ico>,
    /// The row's tooltip: what its ellipsized lines say in full.
    #[prop(optional, into)]
    title: Option<TextProp>,
    /// A symbolic icon after the state word (or alone), the row's state in a
    /// second form.
    #[prop(optional, into)]
    state_ico: MaybeProp<Ico>,
    /// What the state icon says, as its tooltip and accessible name.
    #[prop(optional, into)]
    state_title: Option<TextProp>,
    /// The name in the secondary colour: a row that names nothing yet.
    #[prop(optional, into)]
    quiet_name: Signal<bool>,
) -> impl IntoView {
    view! {
        <div
            class="lrow"
            class:has-ico=icon.is_some()
            title=move || title.as_ref().map(|t| t.get().to_string())
            data-key=key
            class:sel=move || is_on(selected)
            class:off=move || is_on(off)
            role="option"
            aria-selected=move || is_on(selected).to_string()
            on:click=move |_| {
                if let Some(c) = on_click {
                    c.run(());
                }
            }
        >
            {icon.map(|i| view! { <Icon ico=i class="lrow-ico"/> })}
            <span class="lrow-name" class:quiet=move || quiet_name.get()>{move || name.get()}</span>
            <span class="lrow-state">
                    {dot.map(|d| view! { <StateDot dot=d/> })}
                    {state.map(|s| move || s.get())}
                    {move || {
                        state_ico
                            .get()
                            .map(|i| {
                                let icon = view! { <Icon ico=i class="lrow-state-ico"/> };
                                match state_title.clone() {
                                    Some(t) => {
                                        let label = t.clone();
                                        view! {
                                            <span
                                                class="lrow-state-wrap"
                                                role="img"
                                                title=move || t.get().to_string()
                                                aria-label=move || label.get().to_string()
                                            >
                                                {icon}
                                            </span>
                                        }
                                            .into_any()
                                    }
                                    None => icon.into_any(),
                                }
                            })
                    }}
            </span>
            {meta.map(|m| view! { <span class="lrow-meta">{move || m.get()}</span> })}
        </div>
    }
}

/// The unsaved-changes pill at the bottom of a pane: how many, Discard and
/// Save, a refused save's reason. It is there only while something is
/// unsaved (or the save failed); a clean form shows nothing.
#[component]
pub fn SaveToast(
    #[prop(into)] dirty_count: Signal<usize>,
    /// Fields that do not parse: Save waits for them.
    #[prop(optional, into)]
    invalid: Option<Signal<usize>>,
    #[prop(into)] saving: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    on_save: Callback<()>,
    on_discard: Callback<()>,
    /// The Save button's word ("Create model"); "Save" when unset.
    #[prop(optional, into)]
    save_label: Option<TextProp>,
) -> impl IntoView {
    let n = move || dirty_count.get();
    let bad = move || invalid.map_or(0, |i| i.get());
    // A field to fix is unsaved input too, even where its value has not
    // moved (a spin keeps the old value while it shows the refused text).
    let shown = move || n() > 0 || bad() > 0 || error.with(Option::is_some);
    // Named: a `>` inside a `view!` attribute closure ends the tag.
    let blocked = move || saving.get() || n() == 0 || bad() > 0;
    let clean = move || saving.get() || (n() == 0 && bad() == 0);
    view! {
        <div class="save-toast" class:show=shown role="status">
            <span class="save-toast-text">
                {move || match (n(), bad()) {
                    (_, b) if b > 0 => {
                        format!("{b} {} to fix", if b == 1 { "field" } else { "fields" })
                    }
                    (1, _) => "1 unsaved change".to_string(),
                    (k, _) => format!("{k} unsaved changes"),
                }}
            </span>
            {move || {
                error
                    .get()
                    .map(|e| {
                        let tip = e.clone();
                        view! {
                            <span class="save-toast-err" title=tip>
                                {format!("Not saved: {e}")}
                            </span>
                        }
                    })
            }}
            <button
                type="button"
                class="btn"
                disabled=clean
                on:click=move |_| on_discard.run(())
            >
                "Discard"
            </button>
            <button
                type="button"
                class="btn suggested"
                disabled=blocked
                on:click=move |_| on_save.run(())
            >
                {move || {
                    if saving.get() {
                        "Saving…".to_string()
                    } else {
                        save_label.as_ref().map_or_else(|| "Save".to_string(), |l| l.get().to_string())
                    }
                }}
            </button>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dot_and_its_hero_chip_share_one_tone() {
        for (dot, chip) in [
            (Dot::Running, "chip ok"),
            (Dot::Warn, "chip warn"),
            (Dot::Busy, "chip info"),
            (Dot::Stopped, "chip"),
            (Dot::Remote, "chip"),
            (Dot::Error, "chip err"),
            (Dot::Off, "chip off"),
        ] {
            assert_eq!(dot.chip_class(), chip);
        }
        assert_eq!(Dot::Warn.class(), "dot warn");
        assert_eq!(Dot::Busy.class(), "dot busy");
    }

    const WHOLE: Spin = Spin {
        step: 1.0,
        min: None,
        max: None,
        digits: 0,
    };
    const DEC: Spin = Spin {
        step: 0.1,
        min: None,
        max: None,
        digits: 1,
    };

    #[test]
    fn a_spin_groups_and_reads_back() {
        let s = Spin::default();
        assert_eq!(s.show(262_144.0), "262\u{2009}144");
        assert_eq!(s.show(-1_000.0), "-1\u{2009}000");
        assert_eq!(s.show(12.0), "12");
        assert_eq!(s.parse("262 144"), Some(262_144.0));
        assert_eq!(s.parse("262\u{2009}144"), Some(262_144.0));
        assert_eq!(s.parse(&s.show(262_144.0)), Some(262_144.0));
        assert_eq!(s.parse("1'024"), Some(1024.0));
        assert_eq!(s.parse("abc"), None);
        assert_eq!(s.parse(""), None);
        assert_eq!(s.parse("-12"), Some(-12.0));
        assert_eq!(s.parse("\u{2212}12"), Some(-12.0));
        let d = Spin {
            digits: 2,
            ..Spin::default()
        };
        assert_eq!(d.show(1234.5), "1\u{2009}234.50");
        assert_eq!(d.parse("1 234.50"), Some(1234.5));
        assert_eq!(d.parse(&d.show(1234.5)), Some(1234.5));
    }

    /// The forms people type, in a spin without decimals.
    #[test]
    fn a_whole_spin_reads_commas_and_points_before_three_digits_as_group_marks() {
        assert_eq!(WHOLE.parse("131,072"), Some(131_072.0));
        assert_eq!(WHOLE.parse("131.072"), Some(131_072.0));
        assert_eq!(WHOLE.parse("131 072"), Some(131_072.0));
        assert_eq!(WHOLE.parse("1,048,576"), Some(1_048_576.0));
        // A fraction is not whole; it is refused, not rounded.
        assert_eq!(WHOLE.read("1,024.5"), Err(SpinError::NotWhole));
        assert_eq!(WHOLE.read("0.5"), Err(SpinError::NotWhole));
        assert_eq!(WHOLE.parse("0,5"), None);
        assert_eq!(WHOLE.parse("1.024,5"), None);
        assert_eq!(WHOLE.parse("1,0485"), None);
        assert_eq!(WHOLE.parse("12.0"), Some(12.0));
    }

    /// The same forms in a spin with decimals.
    #[test]
    fn a_decimal_spin_reads_the_last_mark_as_the_decimal_one() {
        // The only mark is the decimal one.
        assert_eq!(DEC.parse("131,072"), Some(131.072));
        assert_eq!(DEC.parse("131 072"), Some(131_072.0));
        assert_eq!(DEC.parse("1,024.5"), Some(1024.5));
        assert_eq!(DEC.parse("0,5"), Some(0.5));
        assert_eq!(DEC.parse("0.5"), Some(0.5));
        assert_eq!(DEC.parse("1.024,5"), Some(1024.5));
        assert_eq!(DEC.parse("1\u{2009}234,5"), Some(1234.5));
        // One mark typed again groups; the decimal mark twice is no number.
        assert_eq!(DEC.parse("1.234.567"), Some(1_234_567.0));
        assert_eq!(DEC.parse("1,234,567.5"), Some(1_234_567.5));
        assert_eq!(DEC.parse("1.2.3,4"), None);
        assert_eq!(DEC.parse("1,2.3,4"), None);
    }

    #[test]
    fn a_spin_refuses_what_is_out_of_bounds_instead_of_moving_it() {
        let s = Spin {
            step: 4096.0,
            min: Some(0.0),
            max: Some(8192.0),
            digits: 0,
        };
        assert_eq!(s.check("-1"), Err(SpinError::Below(0.0)));
        assert_eq!(s.check("9000"), Err(SpinError::Above(8192.0)));
        assert_eq!(s.check("8,192"), Ok(8192.0));
        assert_eq!(
            s.describe("9000", SpinError::Above(8192.0)),
            "at most 8\u{2009}192"
        );
        assert_eq!(
            s.describe("1,5", SpinError::NotANumber),
            "\u{201c}1,5\u{201d} is not a number"
        );
        // The buttons stop at the bounds.
        assert_eq!(s.clamp(-1.0), 0.0);
        assert_eq!(s.clamp(9000.0), 8192.0);
        assert_eq!(s.clamp(100.0), 100.0);
    }
}
