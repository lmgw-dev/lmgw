//! A bound call's oversized event as `lmgw.event.part`s (calls design
//! §6.3): `{event_id, type, id, index, count, data}`, `id` the event's own
//! `event_id`, `data` a slice of its JSON text cut at a character boundary,
//! each part's JSON within the limit with `data` escaped. Every part has an
//! `event_id` of its own, minted as it is made; the room for it is the
//! longest id the session mints, so a part never runs over.
//!
//! lmgw-client's splitter is a twin, kept apart on purpose: the calls
//! suites join lmgw's parts with lmgw-client's joiner and split the phone's
//! with lmgw-client's splitter, so each side is checked against the other's
//! reading of the contract.

use std::borrow::Cow;

use lmgw_api_types::realtime::EVENT_PART;
use serde::Deserialize;

use super::super::super::ids::EVENT_ID_JSON_MAX as EVENT_ID_JSON;

/// A part's own text with no ids, numbers or data characters.
const FRAME: &str = r#"{"event_id":,"type":"lmgw.event.part","id":,"index":,"count":,"data":""}"#;

/// `text`'s parts for a receiver that takes messages of up to `max` bytes,
/// each part's `event_id` from `mint` (module doc). Err: why it cannot be
/// split — the limit leaves no room for a part's data, or the event needs
/// more parts than a `count` holds.
pub(super) fn split(
    text: &str,
    max: usize,
    mut mint: impl FnMut() -> String,
) -> Result<Vec<String>, String> {
    let id = event_id(text).unwrap_or_else(&mut mint);
    let id_json = serde_json::to_string(&id).expect("a string serializes");
    let frame = FRAME.len() + EVENT_ID_JSON + id_json.len();
    // Room for `index` and `count` at the digits of the count; grows when
    // the parts outnumber them.
    let mut digits = 1;
    loop {
        let overhead = frame + 2 * digits;
        let room = max.saturating_sub(overhead);
        let mut cuts = Vec::new();
        let (mut start, mut used) = (0, 0);
        for (at, c) in text.char_indices() {
            let width = escaped_len(c);
            if width > room {
                return Err(format!(
                    "an lmgw.event.part of it needs {} bytes, more than the limit",
                    overhead + width
                ));
            }
            if used + width > room {
                cuts.push(start..at);
                (start, used) = (at, 0);
            }
            used += width;
        }
        cuts.push(start..text.len());
        let needs = decimal_digits(cuts.len());
        if needs > digits {
            digits = needs;
            continue;
        }
        let count = u32::try_from(cuts.len())
            .map_err(|_| format!("it needs more than {} parts", u32::MAX))?;
        return Ok(cuts
            .into_iter()
            .enumerate()
            .map(|(index, r)| {
                let event_id = serde_json::to_string(&mint()).expect("a string serializes");
                let data = serde_json::to_string(&text[r]).expect("a string serializes");
                format!(
                    r#"{{"event_id":{event_id},"type":"{EVENT_PART}","id":{id_json},"index":{index},"count":{count},"data":{data}}}"#
                )
            })
            .collect());
    }
}

/// An event's `event_id`, read from its text.
fn event_id(text: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Id<'a> {
        #[serde(borrow, default)]
        event_id: Option<Cow<'a, str>>,
    }
    serde_json::from_str::<Id>(text)
        .ok()
        .and_then(|i| i.event_id)
        .map(Cow::into_owned)
}

/// An event's `type`, for a log line; `?` when it has none.
pub(super) fn kind_of(text: &str) -> String {
    #[derive(Deserialize)]
    struct Kind<'a> {
        #[serde(rename = "type", borrow, default)]
        kind: Option<Cow<'a, str>>,
    }
    serde_json::from_str::<Kind>(text)
        .ok()
        .and_then(|k| k.kind)
        .map_or_else(|| "?".to_string(), Cow::into_owned)
}

/// The bytes `c` takes in a JSON string as serde_json writes it: `"` and
/// `\` and the five short control escapes two, other control characters
/// six (`\u001f`), everything else its UTF-8.
pub(super) fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
        '\0'..='\u{1f}' => 6,
        _ => c.len_utf8(),
    }
}

fn decimal_digits(n: usize) -> usize {
    n.checked_ilog10().map_or(1, |d| d as usize + 1)
}
