//! `lmgw.event.part` (realtime calls design §6.3, §8.3): on a call bound to
//! a Chat thread, an event larger than the receiver's `max-message-size`
//! travels as consecutive parts `{id, index, count, data}`, each within that
//! limit, whose `data` joined is the event's JSON text. Both ways: lmgw
//! splits at the client's limit, a client at lmgw's.
//!
//! - [`Joiner`]: every `oai-events` message from lmgw passes through it
//!   before [`super::parse`]. A message that is no part passes as it is; a
//!   part is held until its set is whole, and the joined text is the one
//!   event to parse.
//! - [`Splitter`] (and [`split`]): a client's own event as the messages to
//!   send, parts where it is larger than lmgw's limit.
//! - [`max_message_size`]: the limit an SDP advertises.
//!
//! **Bounds.** The receiver's `max-message-size` is the real limit, and
//! nothing below it is imposed: the splitter fills each part up to it, and
//! a limit too small to carry a part at all is a [`SplitError`] naming it.
//! A set's parts come back to back, so a joiner holds one set at a time.
//! What a set joins to is bounded where the receiver says so: lmgw joins a
//! client's up to `realtime.max_message_mb` and refuses a larger one with
//! `event_too_large` naming the setting; a client's [`Joiner::bounded`]
//! refuses a set past the bound it names (its own setting) with
//! [`PartFault::TooLarge`], and [`Joiner::new`] holds what arrives. A broken
//! set is a [`PartError`] naming its `id`, never a dropped word.

use std::borrow::Cow;
use std::collections::HashSet;

use lmgw_api_types::realtime::{EventPart, EVENT_PART};
use serde::Deserialize;

#[cfg(test)]
mod tests;

/// What an SDP without `a=max-message-size` allows (RFC 8841 §6.1).
pub const DEFAULT_MAX_MESSAGE_SIZE: u64 = 65_536;

/// The largest message an SDP's sender takes on its data channel: its
/// `a=max-message-size`, else RFC 8841's default
/// ([`DEFAULT_MAX_MESSAGE_SIZE`]), and `0` — as in the SDP — for no limit.
/// Read it from lmgw's answer to split the client's events ([`Splitter`]).
pub fn max_message_size(sdp: &str) -> u64 {
    sdp.lines()
        .find_map(|l| l.trim().strip_prefix("a=max-message-size:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_MAX_MESSAGE_SIZE)
}

/// Joins lmgw's parts ([`Joiner::push`]).
#[derive(Debug, Clone, Default)]
pub struct Joiner {
    limit: Option<Limit>,
    open: Option<Open>,
    /// The sets that broke, by id: their parts, whenever they come, are
    /// dropped without another word. One id per broken set, for the
    /// joiner's life (a call's); a set's id is never another's.
    broken: HashSet<String>,
}

#[derive(Debug, Clone)]
struct Limit {
    bytes: u64,
    name: String,
}

/// The set being joined.
#[derive(Debug, Clone)]
struct Open {
    id: String,
    count: u32,
    next: u32,
    text: String,
}

/// What one message gave ([`Joiner::push`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Joined {
    /// Sets this message broke or that broke on it, in order, their parts
    /// dropped: show and log each; the call goes on.
    pub broken: Vec<PartError>,
    /// A whole event's text, to parse: the message itself when it is no
    /// part, or a set's parts joined at its last part. `None` while a set
    /// waits for its next part, and for a part that was dropped.
    pub event: Option<String>,
}

/// A set of parts that cannot be joined; the event it carried is lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartError {
    /// The set's `id`; empty for a part that named none.
    pub id: String,
    pub fault: PartFault,
}

/// What broke a set ([`PartError`]).
///
/// Non-exhaustive: a newer version of this crate may tell more apart.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PartFault {
    /// Another message came between the set's parts: one that is no part,
    /// or a part of another set.
    Interrupted,
    /// A part's `index` is not the next one (`expected`, 0 for a set's
    /// first).
    OutOfOrder { expected: u32, got: u32 },
    /// A part's `count` is not its set's.
    CountChanged { count: u32, got: u32 },
    /// A part whose fields do not read as one, or name no set (a `count`
    /// below 2, an `index` not below it): why.
    Unreadable { why: String },
    /// The joined text would be larger than the joiner's bound: `limit`
    /// bytes, the client's setting `name` ([`Joiner::bounded`]).
    TooLarge { limit: u64, name: String },
    /// The joined text is itself a part, which a set never carries.
    Nested,
}

impl std::fmt::Display for PartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let id = if self.id.is_empty() {
            "(no id)"
        } else {
            &self.id
        };
        write!(f, "the parts of event {id} cannot be joined: ")?;
        match &self.fault {
            PartFault::Interrupted => write!(f, "another message came between them"),
            PartFault::OutOfOrder { expected, got } => {
                write!(f, "part {got} came where part {expected} was due")
            }
            PartFault::CountChanged { count, got } => {
                write!(f, "a part says {got} parts, the set {count}")
            }
            PartFault::Unreadable { why } => write!(f, "a part does not read: {why}"),
            PartFault::TooLarge { limit, name } => {
                write!(f, "they join to more than {limit} bytes ({name})")
            }
            PartFault::Nested => write!(f, "they join to another part"),
        }
    }
}

impl std::error::Error for PartError {}

impl Joiner {
    /// A joiner bounded by what arrives: one set at a time, as large as its
    /// parts make it.
    pub fn new() -> Self {
        Self::default()
    }

    /// A joiner that refuses a set whose joined text would exceed `bytes`
    /// ([`PartFault::TooLarge`], naming `name`: the setting that bounds it,
    /// shown with the error). `0` is no bound, as `max-message-size:0` is
    /// none: the same as [`Joiner::new`].
    pub fn bounded(bytes: u64, name: &str) -> Self {
        Joiner {
            limit: (bytes > 0).then(|| Limit {
                bytes,
                name: name.to_string(),
            }),
            ..Self::default()
        }
    }

    /// One message as it arrived on `oai-events`, in order.
    pub fn push(&mut self, text: String) -> Joined {
        let mut out = Joined::default();
        let part = is_part(&text).then(|| read_part(&text));
        if let Some(open) = self.open.take() {
            if part.as_ref().map(part_id) == Some(open.id.as_str()) {
                match part.expect("a part of the open set") {
                    Ok(p) => self.next(open, p, &mut out),
                    Err((id, why)) => self.fail(id, PartFault::Unreadable { why }, &mut out),
                }
                return out;
            }
            // Its remaining parts, should they still come, go unsaid.
            self.fail(open.id, PartFault::Interrupted, &mut out);
        }
        match part {
            None => out.event = Some(text),
            // A part of a broken set.
            Some(p) if self.broken.contains(part_id(&p)) => {}
            Some(Err((id, why))) => self.fail(id, PartFault::Unreadable { why }, &mut out),
            Some(Ok(p)) if p.index != 0 => self.fail(
                p.id,
                PartFault::OutOfOrder {
                    expected: 0,
                    got: p.index,
                },
                &mut out,
            ),
            Some(Ok(p)) => {
                let open = Open {
                    id: p.id,
                    count: p.count,
                    next: 1,
                    text: String::new(),
                };
                self.append(open, p.data, &mut out);
            }
        }
        out
    }

    /// The open set's next message, which is one of its parts.
    fn next(&mut self, open: Open, p: EventPart, out: &mut Joined) {
        if p.count != open.count {
            let fault = PartFault::CountChanged {
                count: open.count,
                got: p.count,
            };
            return self.fail(open.id, fault, out);
        }
        if p.index != open.next {
            let fault = PartFault::OutOfOrder {
                expected: open.next,
                got: p.index,
            };
            return self.fail(open.id, fault, out);
        }
        let open = Open {
            next: open.next + 1,
            ..open
        };
        self.append(open, p.data, out);
    }

    /// `data` onto `open`, which it completes or keeps open.
    fn append(&mut self, mut open: Open, data: String, out: &mut Joined) {
        if let Some(l) = &self.limit {
            if (open.text.len() + data.len()) as u64 > l.bytes {
                let fault = PartFault::TooLarge {
                    limit: l.bytes,
                    name: l.name.clone(),
                };
                return self.fail(open.id, fault, out);
            }
        }
        open.text.push_str(&data);
        if open.next < open.count {
            self.open = Some(open);
        } else if is_part(&open.text) {
            self.fail(open.id, PartFault::Nested, out);
        } else {
            out.event = Some(open.text);
        }
    }

    /// Set `id` is broken: said once, and its remaining parts dropped. A
    /// part that names no id belongs to no set, and is said each time.
    fn fail(&mut self, id: String, fault: PartFault, out: &mut Joined) {
        if !id.is_empty() {
            self.broken.insert(id.clone());
        }
        out.broken.push(PartError { id, fault });
    }
}

/// The set a part names, read or not.
fn part_id(part: &Result<EventPart, (String, String)>) -> &str {
    match part {
        Ok(p) => &p.id,
        Err((id, _)) => id,
    }
}

/// Whether `text` is an `lmgw.event.part`: a JSON object of that `type`.
/// A message that does not even contain the type's name, which is every
/// other event, is not read here at all, only by [`super::parse`] next.
fn is_part(text: &str) -> bool {
    #[derive(Deserialize)]
    struct Kind<'a> {
        #[serde(rename = "type", borrow, default)]
        kind: Option<Cow<'a, str>>,
    }
    // An array would read as a struct by position.
    text.contains(EVENT_PART)
        && text.trim_start().starts_with('{')
        && serde_json::from_str::<Kind>(text)
            .ok()
            .and_then(|k| k.kind)
            .is_some_and(|k| k == EVENT_PART)
}

/// A part's fields, or its `id` (empty when it has none) and why they do
/// not read as a set's part.
fn read_part(text: &str) -> Result<EventPart, (String, String)> {
    match serde_json::from_str::<EventPart>(text) {
        Ok(p) if p.count < 2 => {
            let why = format!("its count is {}, and a set has at least 2 parts", p.count);
            Err((p.id, why))
        }
        Ok(p) if p.index >= p.count => {
            let why = format!("its index {} is not below its count {}", p.index, p.count);
            Err((p.id, why))
        }
        Ok(p) => Ok(p),
        Err(e) => {
            let id = serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .and_then(|v| v["id"].as_str().map(str::to_string))
                .unwrap_or_default();
            Err((id, e.to_string()))
        }
    }
}

/// A client's events split for a call ([`Splitter::split`]), under ids of
/// its own: `part_1`, `part_2`, … in the order of its sets.
#[derive(Debug, Clone)]
pub struct Splitter {
    max_message_size: u64,
    sets: u64,
}

impl Splitter {
    /// For a receiver that takes messages of up to `max_message_size`
    /// bytes, `0` for no limit: lmgw's, as its answer advertises it
    /// ([`max_message_size`]; libwebrtc's `RTCSctpTransport.maxMessageSize`
    /// says the same).
    pub fn new(max_message_size: u64) -> Self {
        Splitter {
            max_message_size,
            sets: 0,
        }
    }

    pub fn max_message_size(&self) -> u64 {
        self.max_message_size
    }

    /// The messages to send for `event` (its JSON text): the event itself
    /// when it fits, else its parts ([`split`]), back to back.
    pub fn split(&mut self, event: &str) -> Result<Vec<String>, SplitError> {
        if fits(event, self.max_message_size) {
            return Ok(vec![event.to_string()]);
        }
        self.sets += 1;
        split(event, &format!("part_{}", self.sets), self.max_message_size)
    }
}

/// An event that cannot be split for the receiver's limit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SplitError {
    /// The receiver's `max-message-size` leaves no room for data: a part
    /// of this event needs `needed` bytes (its own fields and the widest
    /// character it has to carry).
    LimitTooSmall { max_message_size: u64, needed: u64 },
    /// The event needs more parts than a `count` holds (2³² − 1).
    TooManyParts { max_message_size: u64 },
}

impl std::fmt::Display for SplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SplitError::LimitTooSmall {
                max_message_size,
                needed,
            } => write!(
                f,
                "an lmgw.event.part of this event needs {needed} bytes, more than the \
                 receiver's max-message-size of {max_message_size} bytes"
            ),
            SplitError::TooManyParts { max_message_size } => write!(
                f,
                "this event needs more than {} parts at the receiver's max-message-size of \
                 {max_message_size} bytes",
                u32::MAX
            ),
        }
    }
}

impl std::error::Error for SplitError {}

/// A part's own text with no `id`, numbers or data characters: the frame
/// every part carries.
const PART_FRAME: &str = r#"{"type":"lmgw.event.part","id":,"index":,"count":,"data":""}"#;

// The frame spells the type [`split`] writes, so a part is as long as
// counted.
const _: () = assert!(frame_names(PART_FRAME.as_bytes(), EVENT_PART.as_bytes()));

/// Whether `frame` begins `{"type":"<kind>"`.
const fn frame_names(frame: &[u8], kind: &[u8]) -> bool {
    let head = br#"{"type":""#;
    if frame.len() < head.len() + kind.len() + 1 {
        return false;
    }
    let mut i = 0;
    while i < head.len() {
        if frame[i] != head[i] {
            return false;
        }
        i += 1;
    }
    let mut i = 0;
    while i < kind.len() {
        if frame[head.len() + i] != kind[i] {
            return false;
        }
        i += 1;
    }
    frame[head.len() + kind.len()] == b'"'
}

fn fits(event: &str, max_message_size: u64) -> bool {
    max_message_size == 0 || event.len() as u64 <= max_message_size
}

/// `event` (a JSON text) as the messages that carry it to a receiver whose
/// `max-message-size` is `max_message_size` (`0`: no limit): itself when it
/// fits, else its `lmgw.event.part`s under `id` — lmgw's are the event's
/// `event_id`, a client's an id of its own ([`Splitter`]). Each part's `data`
/// is cut at a character boundary and filled until the part's own JSON,
/// with `data` escaped, is as large as the limit allows; `count` is at
/// least 2.
pub fn split(event: &str, id: &str, max_message_size: u64) -> Result<Vec<String>, SplitError> {
    if fits(event, max_message_size) {
        return Ok(vec![event.to_string()]);
    }
    let id_json = serde_json::to_string(id).expect("a string serializes");
    let frame = (PART_FRAME.len() + id_json.len()) as u64;
    // Room for `index` and `count` at the digits of the count; grows when
    // the parts outnumber them.
    let mut digits = 1;
    loop {
        let overhead = frame + 2 * digits;
        let room = max_message_size.saturating_sub(overhead);
        let mut cuts = Vec::new();
        let (mut start, mut used) = (0, 0);
        for (at, c) in event.char_indices() {
            let width = escaped_len(c);
            if width > room {
                return Err(SplitError::LimitTooSmall {
                    max_message_size,
                    needed: overhead + width,
                });
            }
            if used + width > room {
                cuts.push(start..at);
                (start, used) = (at, 0);
            }
            used += width;
        }
        cuts.push(start..event.len());
        let needs = decimal_digits(cuts.len() as u64);
        if needs > digits {
            digits = needs;
            continue;
        }
        let count =
            u32::try_from(cuts.len()).map_err(|_| SplitError::TooManyParts { max_message_size })?;
        return Ok(cuts
            .into_iter()
            .enumerate()
            .map(|(index, r)| {
                let data = serde_json::to_string(&event[r]).expect("a string serializes");
                format!(
                    r#"{{"type":"{EVENT_PART}","id":{id_json},"index":{index},"count":{count},"data":{data}}}"#
                )
            })
            .collect());
    }
}

/// The bytes `c` takes in a JSON string as serde_json writes it: `"` and
/// `\` and the five short control escapes two, other control characters
/// six (`\u001f`), everything else its UTF-8.
fn escaped_len(c: char) -> u64 {
    match c {
        '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
        '\0'..='\u{1f}' => 6,
        _ => c.len_utf8() as u64,
    }
}

fn decimal_digits(n: u64) -> u64 {
    n.checked_ilog10().map_or(1, |d| u64::from(d) + 1)
}
