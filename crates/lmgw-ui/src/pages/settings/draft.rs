//! Settings — the draft: row lookup and anchors, the baseline, the sparse patch and what a save judges.

use super::*;

/// `vram.headroom_mb` → `f-vram-headroom-mb`: what `/settings/gpu#…` names.
pub(super) fn anchor(key: &str) -> String {
    format!("f-{}", key.replace(['.', '_'], "-"))
}

/// The deep link to a setting, for the "Settings → X" pointers elsewhere.
pub(in crate::pages) fn href(key: &str) -> String {
    let cat = fields().find(|d| d.key == key).map_or("network", |d| d.cat);
    format!("/settings/{cat}#{}", anchor(key))
}

pub(super) fn pointer(key: &str) -> String {
    format!("/{}", key.replace('.', "/"))
}

fn cat_label(slug: &str) -> &'static str {
    CATS.iter()
        .find(|(s, ..)| *s == slug)
        .map_or("", |(_, l, _)| l)
}

/// The category a changed form key belongs to (a secret's clear tick counts
/// with its secret).
pub(super) fn cat_of_key(k: &str) -> Option<&'static str> {
    fields()
        .find(|d| d.key == k || matches!(d.ctl, Ctl::Secret(_, clear) if clear == k))
        .map(|d| d.cat)
}

/// Everything the search reads for one setting, lowercased.
pub(super) fn haystack(d: &Def) -> String {
    let group = GROUPS
        .iter()
        .find(|g| g.id == d.group)
        .map_or("", |g| g.title);
    format!(
        "{} {} {} {} {} {} {} {}",
        d.label,
        d.unit,
        d.hint,
        d.key,
        d.key.replace(['.', '_'], " "),
        group,
        cat_label(d.cat),
        d.terms
    )
    .to_lowercase()
}

/// Whether a search may show the row: every field, but not the machine
/// check's card, which its link rows stand for there (§2.3).
pub(super) fn in_search(d: &Def) -> bool {
    !matches!(d.ctl, Ctl::Machine)
}

/// Whether the row shows in its category when nothing is searched: every
/// field, but not the machine check's link rows, which only a search shows
/// (the card is right there).
pub(super) fn in_category(d: &Def) -> bool {
    !matches!(d.ctl, Ctl::MachineLink)
}

/// Whether every searched word is in the row's words.
pub(super) fn hits(d: &Def, words: &[String]) -> bool {
    let hay = haystack(d);
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// The machine check's link rows a search's `words` match, in page order:
/// one link stands for all of them (review L11), the first.
pub(super) fn machine_hits(words: &[String]) -> Vec<&'static Def> {
    if words.is_empty() {
        return Vec::new();
    }
    fields()
        .filter(|d| matches!(d.ctl, Ctl::MachineLink) && hits(d, words))
        .collect()
}

/// What the one link row names: the matched rows whose own label has the
/// words ("nvidia" → NVIDIA), else the first match ("gpu" → Graphics card).
pub(super) fn link_labels(words: &[String]) -> Vec<&'static str> {
    let found = machine_hits(words);
    let by_label: Vec<&'static str> = found
        .iter()
        .filter(|d| {
            let l = d.label.to_lowercase();
            words.iter().all(|w| l.contains(w.as_str()))
        })
        .map(|d| d.label)
        .collect();
    if by_label.is_empty() {
        found.first().map(|d| d.label).into_iter().collect()
    } else {
        by_label
    }
}

/// The one link row's words before "This computer": "NVIDIA is in",
/// "Graphics card and NVIDIA are in", "A, B and C are in".
pub(super) fn link_words(labels: &[&str]) -> String {
    match labels {
        [] => String::new(),
        [one] => format!("{one} is in"),
        [first @ .., last] => format!("{} and {last} are in", first.join(", ")),
    }
}

/// What the server holds, as the form's flat baseline: one entry per form
/// key, in the shape its box shows (money in units, lists as lines, a
/// missing alias as "").
pub(super) fn baseline(s: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    for d in fields() {
        let read = || s.pointer(&pointer(d.key)).cloned().unwrap_or(Value::Null);
        match d.ctl {
            Ctl::Secret(_, clear) => {
                m.insert(d.key.into(), Value::String(String::new()));
                m.insert(clear.into(), Value::Bool(false));
            }
            Ctl::ForgeTokens => {
                m.insert(d.key.into(), json!(forge_baseline(&read())));
            }
            Ctl::Money => {
                let micro = read().as_i64().unwrap_or(0);
                m.insert(
                    d.key.into(),
                    if micro > 0 {
                        json!(micro as f64 / 1e6)
                    } else {
                        Value::Null
                    },
                );
            }
            Ctl::Map => {
                m.insert(d.key.into(), Value::String(realtime::map_text(&read())));
            }
            Ctl::Words => {
                m.insert(d.key.into(), Value::String(realtime::words_text(&read())));
            }
            Ctl::Lines => {
                let lines: Vec<String> = read()
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                m.insert(d.key.into(), Value::String(lines.join("\n")));
            }
            c if c.form_kind() == Some(Kind::Text) => {
                let v = read();
                m.insert(
                    d.key.into(),
                    if v.is_null() {
                        Value::String(String::new())
                    } else {
                        v
                    },
                );
            }
            c if c.form_kind().is_some() => {
                m.insert(d.key.into(), read());
            }
            _ => {}
        }
    }
    m
}

/// The form's patch in the shape the op takes: lines back into lists, money
/// back into micro-units (empty is 0, "no budget").
pub(super) fn finish(mut patch: Value) -> Value {
    // The forge-token rows become the op's two keys: tokens to set by host,
    // hosts to clear.
    if let Some(obj) = patch.as_object_mut() {
        if let Some(rows) = obj.remove("forge_tokens") {
            let (set, clear) = forge_patch(&forge_rows(&rows));
            if !set.is_empty() {
                obj.insert("forge_tokens".into(), json!(set));
            }
            if !clear.is_empty() {
                obj.insert("clear_forge_tokens".into(), json!(clear));
            }
        }
    }
    for d in fields() {
        let Some(v) = patch.pointer_mut(&pointer(d.key)) else {
            continue;
        };
        match d.ctl {
            Ctl::Lines => {
                let list: Vec<String> = v
                    .as_str()
                    .unwrap_or_default()
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                *v = json!(list);
            }
            Ctl::Money => {
                *v = json!(v.as_f64().map_or(0, |u| (u * 1e6).round() as i64));
            }
            Ctl::Map => {
                *v = realtime::map_object(v.as_str().unwrap_or_default());
            }
            Ctl::Words => {
                *v = realtime::words_list(v.as_str().unwrap_or_default());
            }
            _ => {}
        }
    }
    patch
}

/// A number that parses but is out of range, or a required field left
/// empty — a parse error is the form's own. Blocks Save like one.
pub(super) fn range_error(d: &Def, text: &str) -> Option<String> {
    let t = text.trim();
    if d.req && t.is_empty() {
        return Some("cannot be empty".to_string());
    }
    match d.ctl {
        Ctl::Int(min) if min > i64::MIN => match t.parse::<i64>() {
            Ok(n) if n < min => Some(if min == 0 {
                "must be 0 or more".to_string()
            } else {
                format!("must be at least {min}")
            }),
            _ => None,
        },
        Ctl::IntRange(min, max) | Ctl::OptIntRange(min, max) => match t.parse::<i64>() {
            Ok(n) if n < min || n > max => Some(format!("must be between {min} and {max}")),
            _ => None,
        },
        Ctl::Float(min, max) => match t.parse::<f64>() {
            Ok(n) if !(min..=max).contains(&n) => Some(format!("must be between {min} and {max}")),
            _ => None,
        },
        Ctl::Vad(col) => realtime::vad_range_error(col, t),
        Ctl::Map => realtime::map_error(t),
        Ctl::Money => match t.replace(',', ".").parse::<f64>() {
            Ok(v) if v < 0.0 => Some("must be 0 or more, or empty for none".to_string()),
            _ => None,
        },
        Ctl::BuildsDir if !t.is_empty() && !t.starts_with('/') => {
            Some("an absolute path, or empty for the default".to_string())
        }
        Ctl::BuildsDir if t.trim_end_matches('/').is_empty() && !t.is_empty() => {
            Some("cannot be / — builds create and remove directories under it".to_string())
        }
        _ => None,
    }
}

/// What is wrong with a field's draft, in words: [`range_error`], a
/// cross-field refusal ([`realtime::cross_error`]), or a forge row the
/// server would refuse.
pub(super) fn problem(form: FormState, d: &Def) -> Option<String> {
    range_error(d, &form.text(d.key))
        .or_else(|| chat_voice::error(d.key, &form.text(d.key)))
        .or_else(|| realtime::cross_error(form, d.key))
        .or_else(|| match d.ctl {
            Ctl::ForgeTokens => forge_error(&forge_rows(&form.value(d.key))),
            _ => None,
        })
}

/// The keys a save of the changed keys `dirty` judges — the server's rule
/// (realtime design §12, "Judged when touched"): what it changes, and what
/// a change is judged together with ([`realtime::judged_with`]). A value a
/// hand edit broke in a field nobody touched is shown, as a warning, but
/// holds back no other change: the server would not refuse it either.
pub(super) fn judged_keys(dirty: &[String]) -> BTreeSet<&'static str> {
    let groups: BTreeSet<&str> = dirty
        .iter()
        .filter_map(|k| realtime::judged_with(k))
        .collect();
    fields()
        .map(|d| d.key)
        .filter(|k| {
            dirty.iter().any(|d| d == k)
                || realtime::judged_with(k).is_some_and(|g| groups.contains(g))
        })
        .collect()
}

/// A field's message as the page shows it: `(error, warning)` — the
/// problem as an error Save waits for when the save judges the field, else
/// as a warning about the stored value.
pub(super) fn messages(form: FormState, d: &Def, judged: bool) -> (Option<String>, Option<String>) {
    match problem(form, d) {
        Some(m) if judged => (Some(m), None),
        Some(m) => (
            None,
            Some(format!("as stored: {m} — other changes still save")),
        ),
        None => (None, None),
    }
}

/// What Save waits for, per key: the problems of the fields `judged`.
pub(super) fn blocking(
    form: FormState,
    judged: &BTreeSet<&'static str>,
) -> BTreeMap<&'static str, String> {
    fields()
        .filter(|d| judged.contains(d.key))
        .filter_map(|d| problem(form, d).map(|e| (d.key, e)))
        .collect()
}
