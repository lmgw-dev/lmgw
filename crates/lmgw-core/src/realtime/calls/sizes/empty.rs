//! An unbound call's oversized event with its largest fields emptied
//! (calls design §6.3): a stock client cannot join parts, so the event is
//! sent as its type still, with fields emptied one by one until it fits.
//!
//! **Which field.** A field may be emptied when it is a string, an array or
//! an object, under a key that is neither the event's `type` nor its
//! `event_id` nor an id (a string under `id`, or under a key ending in `_id`,
//! at any depth; an object or array under such a key is a schema's property,
//! not an id), and
//! — an array or an object — with no id anywhere inside it, so every id the
//! event carries arrives. Of those, the one whose emptying saves the most is
//! taken, largest first; and when emptying a field inside it alone would
//! bring the event within the limit, the narrowest such field on the way in
//! is taken instead (its largest child, while that child alone suffices), so
//! a long `instructions` beside short ones goes, not the whole `session`, and
//! a list of tools goes as `[]` rather than tool by tool. Then the event is
//! measured again, until it fits.
//!
//! **What it becomes.** A string becomes a short marker, `[omitted: 312 KiB
//! over this call's 256 KiB event limit]`; an array `[]`; an object `{}`.
//! Each emptied field is answered with its path (`session.instructions`,
//! `response.output[0].content[0].text`) and its size before, for the
//! `event_too_large` error the shaper sends after the event.
//!
//! The event is re-serialized from a `serde_json::Value`, whose maps are
//! sorted here: an emptied event's keys come in alphabetical order, which no
//! JSON reader minds.

use serde_json::Value;

use super::split::escaped_len;

/// An event made to fit.
#[derive(Debug)]
pub(super) struct Fitted {
    /// Its JSON, within the limit.
    pub text: String,
    /// Its `type` and `event_id`, for the errors.
    pub kind: String,
    pub event_id: String,
    /// The fields emptied, in the order they were.
    pub emptied: Vec<Emptied>,
}

/// One emptied field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Emptied {
    /// Its path: keys joined by `.`, indexes as `[n]`.
    pub path: String,
    /// Its JSON size before, in bytes.
    pub size: usize,
}

/// `text`, an event's JSON, with its largest fields emptied until it is at
/// most `max` bytes (module doc); `limit` is `max` as the marker says it.
/// Err: why it cannot be made to fit — it is no JSON object, or what may not
/// be emptied is over the limit by itself.
pub(super) fn fit(text: &str, max: usize, limit: &str) -> Result<Fitted, String> {
    let mut root: Value = serde_json::from_str(text).map_err(|e| format!("no JSON: {e}"))?;
    if !root.is_object() {
        return Err("no JSON object".into());
    }
    let kind = root["type"].as_str().unwrap_or("?").to_string();
    let event_id = root["event_id"].as_str().unwrap_or("?").to_string();
    let mut emptied = Vec::new();
    let mut done: Vec<Vec<Step>> = Vec::new();
    loop {
        let now = serde_json::to_string(&root).expect("a value serializes");
        if now.len() <= max {
            return Ok(Fitted {
                text: now,
                kind,
                event_id,
                emptied,
            });
        }
        let over = now.len() - max;
        let Some(pick) = pick(&root, over, limit, &done) else {
            return Err(format!(
                "with every field that may be emptied emptied it is {} bytes still: its type, \
                 its ids and its numbers alone",
                now.len()
            ));
        };
        let slot = at(&mut root, &pick.path).expect("the picked field is there");
        *slot = pick.replacement;
        done.push(pick.path.clone());
        emptied.push(Emptied {
            path: path_text(&pick.path),
            size: pick.size,
        });
    }
}

/// One step of a field's path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(usize),
}

/// A field that may be emptied.
#[derive(Debug, Clone)]
struct Candidate {
    path: Vec<Step>,
    /// Its JSON size, and what emptying it saves.
    size: usize,
    saving: usize,
    container: bool,
    replacement: Value,
}

/// The field to empty for an event `over` bytes over its limit (module
/// doc), if any may be.
fn pick(root: &Value, over: usize, limit: &str, done: &[Vec<Step>]) -> Option<Candidate> {
    let mut all = Vec::new();
    walk(root, &mut Vec::new(), limit, &mut all);
    // A field already emptied (its marker is a string still) stays so.
    all.retain(|c| !done.contains(&c.path));
    // Largest first; the first of equals in the event's order.
    let mut best = all
        .iter()
        .fold(None::<&Candidate>, |b, c| match b {
            Some(b) if b.saving >= c.saving => Some(b),
            _ => Some(c),
        })?
        .clone();
    // The narrowest field on the way in whose emptying alone suffices.
    while best.container {
        let inner = all
            .iter()
            .filter(|c| c.path.len() == best.path.len() + 1 && c.path.starts_with(&best.path))
            .fold(None::<&Candidate>, |b, c| match b {
                Some(b) if b.saving >= c.saving => Some(b),
                _ => Some(c),
            });
        match inner {
            Some(i) if i.saving >= over => best = i.clone(),
            _ => break,
        }
    }
    Some(best)
}

/// Walk `v` at `path`, collecting every field that may be emptied (module
/// doc): its JSON size, and whether an id is inside it.
fn walk(v: &Value, path: &mut Vec<Step>, limit: &str, out: &mut Vec<Candidate>) -> (usize, bool) {
    let (size, has_id) = match v {
        Value::Null => (4, false),
        Value::Bool(b) => (if *b { 4 } else { 5 }, false),
        Value::Number(n) => (n.to_string().len(), false),
        Value::String(s) => (string_len(s), false),
        Value::Array(items) => {
            let (mut size, mut has_id) = (2 + items.len().saturating_sub(1), false);
            for (i, item) in items.iter().enumerate() {
                path.push(Step::Index(i));
                let (s, id) = walk(item, path, limit, out);
                path.pop();
                size += s;
                has_id |= id;
            }
            (size, has_id)
        }
        Value::Object(map) => {
            let (mut size, mut has_id) = (2 + map.len().saturating_sub(1), false);
            for (k, item) in map {
                // Only a string under an id key is an id (protocol ids are
                // strings); a schema's `properties.id` is an object.
                let id = is_id(k) && item.is_string();
                let protected = id || (path.is_empty() && (k == "type" || k == "event_id"));
                has_id |= id;
                path.push(Step::Key(k.clone()));
                let (s, id) = if protected {
                    // Nothing at it or inside it is emptied.
                    let mut ignored = Vec::new();
                    walk(item, path, limit, &mut ignored)
                } else {
                    walk(item, path, limit, out)
                };
                path.pop();
                size += string_len(k) + 1 + s;
                has_id |= id;
            }
            (size, has_id)
        }
    };
    // Never the root. A protected field and everything inside it is walked
    // into a throwaway list (above), so it is never a candidate either.
    if !path.is_empty() {
        let replacement = match v {
            Value::String(_) => Some(Value::String(marker(size, limit))),
            Value::Array(_) if !has_id => Some(Value::Array(Vec::new())),
            Value::Object(_) if !has_id => Some(Value::Object(Default::default())),
            _ => None,
        };
        if let Some(r) = replacement {
            let saving = size.saturating_sub(serde_json::to_string(&r).map_or(0, |t| t.len()));
            if saving > 0 {
                out.push(Candidate {
                    path: path.clone(),
                    size,
                    saving,
                    container: !v.is_string(),
                    replacement: r,
                });
            }
        }
    }
    (size, has_id)
}

/// Whether `key` names an id, which is never emptied.
fn is_id(key: &str) -> bool {
    key == "id" || key.ends_with("_id")
}

/// What an emptied string says instead.
fn marker(size: usize, limit: &str) -> String {
    format!(
        "[omitted: {} over this call's {limit} event limit]",
        super::human(size)
    )
}

/// A JSON string's size as serde_json writes it, quotes included.
fn string_len(s: &str) -> usize {
    2 + s.chars().map(escaped_len).sum::<usize>()
}

/// The value at `path`.
fn at<'a>(root: &'a mut Value, path: &[Step]) -> Option<&'a mut Value> {
    path.iter().try_fold(root, |v, step| match step {
        Step::Key(k) => v.get_mut(k),
        Step::Index(i) => v.get_mut(*i),
    })
}

/// `path` as a field's name: keys joined by `.`, indexes as `[n]`.
fn path_text(path: &[Step]) -> String {
    let mut out = String::new();
    for step in path {
        match step {
            Step::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Step::Index(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}
