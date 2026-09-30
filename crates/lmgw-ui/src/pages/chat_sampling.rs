//! A chat thread's sampling parameters: `top_p`, `top_k`, `min_p`,
//! `repeat_penalty`, the two penalties, `seed` and `stop`, kept with the
//! thread beside its temperature. The draft the settings panel edits, the
//! checks a save makes before the round trip (the server makes the same
//! ones), and the fields themselves. A blank box is the route's own default.

use leptos::prelude::*;
use serde_json::{json, Map, Value};

use super::chat::ChatThread;

/// The boxes as they stand. Held by the page (see `SettingsDraft`).
#[derive(Clone, Copy)]
pub(super) struct SamplingDraft {
    top_p: RwSignal<String>,
    top_k: RwSignal<String>,
    min_p: RwSignal<String>,
    repeat: RwSignal<String>,
    presence: RwSignal<String>,
    frequency: RwSignal<String>,
    seed: RwSignal<String>,
    /// One stop sequence per line.
    stop: RwSignal<String>,
}

impl SamplingDraft {
    pub(super) fn new() -> Self {
        Self {
            top_p: RwSignal::new(String::new()),
            top_k: RwSignal::new(String::new()),
            min_p: RwSignal::new(String::new()),
            repeat: RwSignal::new(String::new()),
            presence: RwSignal::new(String::new()),
            frequency: RwSignal::new(String::new()),
            seed: RwSignal::new(String::new()),
            stop: RwSignal::new(String::new()),
        }
    }

    pub(super) fn seed(&self, text: SamplingText) {
        self.top_p.set(text.top_p);
        self.top_k.set(text.top_k);
        self.min_p.set(text.min_p);
        self.repeat.set(text.repeat);
        self.presence.set(text.presence);
        self.frequency.set(text.frequency);
        self.seed.set(text.seed);
        self.stop.set(text.stop);
    }

    /// The boxes as they stand (tracked).
    pub(super) fn text(&self) -> SamplingText {
        SamplingText {
            top_p: self.top_p.get(),
            top_k: self.top_k.get(),
            min_p: self.min_p.get(),
            repeat: self.repeat.get(),
            presence: self.presence.get(),
            frequency: self.frequency.get(),
            seed: self.seed.get(),
            stop: self.stop.get(),
        }
    }

    /// The boxes as they stand, untracked — for a save.
    pub(super) fn text_untracked(&self) -> SamplingText {
        SamplingText {
            top_p: self.top_p.get_untracked(),
            top_k: self.top_k.get_untracked(),
            min_p: self.min_p.get_untracked(),
            repeat: self.repeat.get_untracked(),
            presence: self.presence.get_untracked(),
            frequency: self.frequency.get_untracked(),
            seed: self.seed.get_untracked(),
            stop: self.stop.get_untracked(),
        }
    }
}

/// A thread's sampling values as the form's text.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct SamplingText {
    pub top_p: String,
    pub top_k: String,
    pub min_p: String,
    pub repeat: String,
    pub presence: String,
    pub frequency: String,
    pub seed: String,
    pub stop: String,
}

fn num<T: ToString>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

impl SamplingText {
    pub(super) fn of(t: &ChatThread) -> Self {
        Self {
            top_p: num(t.top_p),
            top_k: num(t.top_k),
            min_p: num(t.min_p),
            repeat: num(t.repeat_penalty),
            presence: num(t.presence_penalty),
            frequency: num(t.frequency_penalty),
            seed: num(t.seed),
            stop: t
                .stop
                .iter()
                .map(|s| escape_stop(s))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// Compared as a save would send them: numbers by value (`0.90` saved is
    /// `0.9` stored), blanks around one are not an edit, and stop sequences
    /// as the list they make, not as text.
    pub(super) fn differs(&self, b: &Self) -> bool {
        !same_number::<f64>(&self.top_p, &b.top_p)
            || !same_number::<i64>(&self.top_k, &b.top_k)
            || !same_number::<f64>(&self.min_p, &b.min_p)
            || !same_number::<f64>(&self.repeat, &b.repeat)
            || !same_number::<f64>(&self.presence, &b.presence)
            || !same_number::<f64>(&self.frequency, &b.frequency)
            || !same_number::<i64>(&self.seed, &b.seed)
            || stop_list(&self.stop) != stop_list(&b.stop)
    }
}

/// Two boxes hold the same number, or — when either is not one — the same
/// text.
fn same_number<T: std::str::FromStr + PartialEq>(a: &str, b: &str) -> bool {
    match (a.trim().parse::<T>(), b.trim().parse::<T>()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.trim() == b.trim(),
    }
}

/// One stop sequence per non-empty line, with `\n`, `\r` and `\\` escapes: a
/// sequence that itself contains a newline (set through the API) is shown as
/// `\n` on one line and parsed back to the same string, so saving the box
/// untouched cannot rewrite it. An unknown escape (`\x`) stays as typed.
fn stop_list(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.is_empty())
        .map(unescape_stop)
        .collect()
}

fn escape_stop(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape_stop(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut it = line.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A number box: blank is unset, otherwise it must parse and lie in `range`.
fn parse_f64(
    name: &str,
    text: &str,
    ok: impl Fn(f64) -> bool,
    want: &str,
) -> Result<Option<f64>, String> {
    match text.trim() {
        "" => Ok(None),
        s => match s.parse::<f64>() {
            Ok(v) if v.is_finite() && ok(v) => Ok(Some(v)),
            _ => Err(format!("{name}: '{s}' is not {want}")),
        },
    }
}

fn parse_i64(
    name: &str,
    text: &str,
    ok: impl Fn(i64) -> bool,
    want: &str,
) -> Result<Option<i64>, String> {
    match text.trim() {
        "" => Ok(None),
        s => match s.parse::<i64>() {
            Ok(v) if ok(v) => Ok(Some(v)),
            _ => Err(format!("{name}: '{s}' is not {want}")),
        },
    }
}

/// The values as a save sends them (`null` clears one), or why they cannot
/// be — the checks the server makes, made before the round trip.
pub(super) fn sampling_patch(t: &SamplingText) -> Result<Value, String> {
    let unit = |v: f64| (0.0..=1.0).contains(&v);
    let pen = |v: f64| (-2.0..=2.0).contains(&v);
    let mut out = Map::new();
    out.insert(
        "top_p".into(),
        json!(parse_f64("top_p", &t.top_p, unit, "a number from 0 to 1")?),
    );
    out.insert(
        "top_k".into(),
        json!(parse_i64(
            "top_k",
            &t.top_k,
            |v| (0..=i64::from(u32::MAX)).contains(&v),
            "a whole number, 0 or more"
        )?),
    );
    out.insert(
        "min_p".into(),
        json!(parse_f64("min_p", &t.min_p, unit, "a number from 0 to 1")?),
    );
    out.insert(
        "repeat_penalty".into(),
        json!(parse_f64(
            "repeat_penalty",
            &t.repeat,
            |v| v > 0.0,
            "a number above 0"
        )?),
    );
    out.insert(
        "presence_penalty".into(),
        json!(parse_f64(
            "presence_penalty",
            &t.presence,
            pen,
            "a number from -2 to 2"
        )?),
    );
    out.insert(
        "frequency_penalty".into(),
        json!(parse_f64(
            "frequency_penalty",
            &t.frequency,
            pen,
            "a number from -2 to 2"
        )?),
    );
    out.insert(
        "seed".into(),
        json!(parse_i64("seed", &t.seed, |_| true, "a whole number")?),
    );
    out.insert("stop".into(), json!(stop_list(&t.stop)));
    Ok(Value::Object(out))
}

/// A saved patch applied to the thread as the page holds it: only the keys
/// the patch carries.
pub(super) fn apply(t: &mut ChatThread, body: &Value) {
    if let Some(v) = body.get("top_p") {
        t.top_p = v.as_f64();
    }
    if let Some(v) = body.get("top_k") {
        t.top_k = v.as_i64();
    }
    if let Some(v) = body.get("min_p") {
        t.min_p = v.as_f64();
    }
    if let Some(v) = body.get("repeat_penalty") {
        t.repeat_penalty = v.as_f64();
    }
    if let Some(v) = body.get("presence_penalty") {
        t.presence_penalty = v.as_f64();
    }
    if let Some(v) = body.get("frequency_penalty") {
        t.frequency_penalty = v.as_f64();
    }
    if let Some(v) = body.get("seed") {
        t.seed = v.as_i64();
    }
    if let Some(v) = body.get("stop") {
        t.stop = serde_json::from_value(v.clone()).unwrap_or_default();
    }
}

/// The Sampling section of the thread settings, and why the values as typed
/// cannot be saved (`error`, which also blocks Save).
#[component]
pub(super) fn SamplingFields(draft: SamplingDraft, error: Memo<Option<String>>) -> impl IntoView {
    let SamplingDraft {
        top_p,
        top_k,
        min_p,
        repeat,
        presence,
        frequency,
        seed,
        stop,
    } = draft;
    let field = move |label: &'static str, hint: &'static str, sig: RwSignal<String>| {
        view! {
            <div class="field">
                <label title=hint>{label}</label>
                <input
                    class="input mono"
                    placeholder="model default"
                    title=hint
                    prop:value=move || sig.get()
                    on:input=move |ev| sig.set(event_target_value(&ev))
                />
            </div>
        }
    };
    view! {
        <div class="field">
            <label>"Sampling"</label>
            <div class="field-grid" style="--field-min:100px">
                {field("Top P", "0 to 1", top_p)}
                {field("Top K", "whole number, 0 or more", top_k)}
                {field("Min P", "0 to 1 (llama-server only)", min_p)}
                {field("Repeat penalty", "above 0; 1 = none (llama-server only)", repeat)}
                {field("Presence penalty", "-2 to 2", presence)}
                {field("Frequency penalty", "-2 to 2", frequency)}
                {field("Seed", "whole number", seed)}
            </div>
            <div class="field">
                <label>"Stop sequences — one per line, \\n for a newline, \\\\ for a backslash"</label>
                <textarea
                    class="input ta mono"
                    rows="2"
                    placeholder="model default"
                    prop:value=move || stop.get()
                    on:input=move |ev| stop.set(event_target_value(&ev))
                ></textarea>
            </div>
            {move || error.get().map(|e| view! { <div class="notice warn">{e}</div> })}
            <div class="field-hint">
                "Blank is the model's own default. A parameter the route cannot take is not \
                 sent, and the reply's stats say which — llama-server takes all of them, other \
                 OpenAI-compatible providers no Top K, Min P or Repeat penalty, Anthropic only \
                 Temperature, Top P, Top K and Stop, Gemini those and Seed."
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text() -> SamplingText {
        SamplingText {
            top_p: "0.9".into(),
            top_k: "40".into(),
            stop: "END\nSTOP".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_patch_carries_every_key_and_blanks_clear() {
        let p = sampling_patch(&text()).unwrap();
        assert_eq!(p["top_p"], 0.9);
        assert_eq!(p["top_k"], 40);
        assert_eq!(p["min_p"], Value::Null);
        assert_eq!(p["seed"], Value::Null);
        assert_eq!(p["stop"], json!(["END", "STOP"]));
    }

    #[test]
    fn out_of_range_and_non_numeric_values_are_refused_by_name() {
        for (edit, name) in [
            (
                SamplingText {
                    top_p: "1.5".into(),
                    ..text()
                },
                "top_p",
            ),
            (
                SamplingText {
                    min_p: "-0.1".into(),
                    ..text()
                },
                "min_p",
            ),
            (
                SamplingText {
                    top_k: "2.5".into(),
                    ..text()
                },
                "top_k",
            ),
            (
                SamplingText {
                    top_k: "-1".into(),
                    ..text()
                },
                "top_k",
            ),
            (
                SamplingText {
                    repeat: "0".into(),
                    ..text()
                },
                "repeat_penalty",
            ),
            (
                SamplingText {
                    presence: "2.1".into(),
                    ..text()
                },
                "presence_penalty",
            ),
            (
                SamplingText {
                    frequency: "x".into(),
                    ..text()
                },
                "frequency_penalty",
            ),
            (
                SamplingText {
                    seed: "1.5".into(),
                    ..text()
                },
                "seed",
            ),
        ] {
            let e = sampling_patch(&edit).unwrap_err();
            assert!(e.starts_with(name), "{e}");
        }
        let edges = SamplingText {
            top_p: "1".into(),
            min_p: "0".into(),
            presence: "-2".into(),
            frequency: "2".into(),
            seed: "-5".into(),
            ..text()
        };
        assert!(sampling_patch(&edges).is_ok());
    }

    #[test]
    fn unsaved_detection_compares_numbers_and_stop_lists_by_value() {
        let t = ChatThread {
            top_p: Some(0.9),
            top_k: Some(40),
            stop: vec!["END".into(), "STOP".into()],
            ..Default::default()
        };
        let stored = SamplingText::of(&t);
        assert_eq!(stored.stop, "END\nSTOP");
        assert!(!stored.differs(&stored));
        let spelled = SamplingText {
            top_p: " 0.90 ".into(),
            top_k: "040".into(),
            stop: "END\n\nSTOP\n".into(),
            ..stored.clone()
        };
        assert!(!spelled.differs(&stored));
        let other = SamplingText {
            seed: "1".into(),
            ..stored.clone()
        };
        assert!(other.differs(&stored));
        let other = SamplingText {
            stop: "END".into(),
            ..stored.clone()
        };
        assert!(other.differs(&stored));
    }

    #[test]
    fn a_stop_sequence_with_a_newline_survives_an_untouched_save() {
        let t = ChatThread {
            stop: vec![
                "a\nb".into(),
                "back\\slash".into(),
                "END".into(),
                "cr\r".into(),
            ],
            ..Default::default()
        };
        let shown = SamplingText::of(&t);
        assert_eq!(shown.stop, "a\\nb\nback\\\\slash\nEND\ncr\\r");
        let p = sampling_patch(&shown).unwrap();
        assert_eq!(p["stop"], json!(t.stop));
        assert!(!shown.differs(&shown));
        assert_eq!(unescape_stop("\\x"), "\\x");
    }

    #[test]
    fn a_saved_patch_is_applied_key_by_key() {
        let mut t = ChatThread {
            top_p: Some(0.5),
            seed: Some(3),
            ..Default::default()
        };
        apply(&mut t, &json!({ "top_k": 10, "stop": ["a"] }));
        assert_eq!((t.top_p, t.top_k, t.seed), (Some(0.5), Some(10), Some(3)));
        assert_eq!(t.stop, ["a"]);
        apply(&mut t, &json!({ "seed": null, "stop": [] }));
        assert_eq!(t.seed, None);
        assert!(t.stop.is_empty());
    }
}
