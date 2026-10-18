//! A bound client's `lmgw.event.part`s joined before the session parses
//! them (calls design §6.3): the call's reader passes every text message on
//! `oai-events` through a [`Joiner`].
//!
//! - A message that is no part passes as it is.
//! - On a bound call a set's parts arrive back to back, in index order, all
//!   with one `count` of at least 2; the joined text is the one event they
//!   carry, which is never itself a part, and goes on stamped as its last
//!   part arrived. A set broken by another message (which then goes on as
//!   the event it is), a wrong index or count, a part that does not read,
//!   or a set that joins to a part is answered `error` `invalid_event`
//!   naming the set's `id`, and dropped.
//! - The joined text is bounded by `realtime.max_message_mb`, the setting
//!   that bounds a WebSocket message (0: no bound); a set past it is
//!   answered `event_too_large` naming the setting, and dropped.
//! - On an unbound call a part is answered `invalid_event` naming its set,
//!   as §6.4 answers every bound call's event there; the set is dropped.
//!
//! Each set is answered once: the rest of a set that broke, should it still
//! come, is dropped without another word (a part 0 with the same `id` is a
//! retry and opens a new set). The joiner remembers the latest
//! set that broke, not every one, so a client that floods broken sets grows
//! nothing: a conforming client's remaining parts follow the break at once,
//! and an interleaving client's sets may each be answered more than once.

use std::borrow::Cow;

use lmgw_api_types::realtime::{EventPart, EVENT_PART};
use serde::Deserialize;

use super::super::super::protocol::ErrorObject;

/// What one message gave ([`Joiner::push`]), in order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Joined {
    /// A whole event's text, for the session: the message itself, or a
    /// set's parts joined.
    Event(String),
    /// A set refused, and why: an `error` for the session to send.
    Refused(ErrorObject),
}

/// A call's joiner (module doc).
#[derive(Debug)]
pub(crate) struct Joiner {
    /// The call is bound to a Chat thread: parts are joined, else refused.
    bound: bool,
    /// `realtime.max_message_mb` in bytes; `None` at 0, no bound.
    limit: Option<usize>,
    open: Option<Open>,
    /// The latest set answered as broken (module doc).
    broken: Option<String>,
}

/// The set being joined.
#[derive(Debug)]
struct Open {
    id: String,
    count: u32,
    next: u32,
    text: String,
}

/// A part as it read: its fields and its own `event_id`, or its set's id
/// (empty when it named none) and why it is no part of a set.
type Read = Result<(EventPart, Option<String>), (String, Option<String>, String)>;

impl Joiner {
    pub fn new(bound: bool, limit: Option<usize>) -> Self {
        Self {
            bound,
            limit,
            open: None,
            broken: None,
        }
    }

    /// The set still open: its id and how many parts of its count came.
    pub fn open_set(&self) -> Option<(&str, u32, u32)> {
        self.open.as_ref().map(|o| (o.id.as_str(), o.next, o.count))
    }

    /// One text message as it arrived on `oai-events`, in order.
    pub fn push(&mut self, text: String) -> Vec<Joined> {
        let mut out = Vec::new();
        let part = is_part(&text).then(|| read(&text));
        if let Some(open) = self.open.take() {
            if part.as_ref().map(id_of) == Some(open.id.as_str()) {
                match part.expect("a part of the open set") {
                    Ok((p, event_id)) => self.next(open, p, event_id, &mut out),
                    Err((id, event_id, why)) => self.fail(id, event_id.as_deref(), &why, &mut out),
                }
                return out;
            }
            let why = "another message came between its parts";
            self.fail(open.id, None, why, &mut out);
        }
        let Some(part) = part else {
            out.push(Joined::Event(text));
            return out;
        };
        let id = id_of(&part).to_string();
        if !id.is_empty() && self.broken.as_deref() == Some(id.as_str()) {
            // The rest of the broken set goes unsaid; a part 0 is a retry
            // (a client may reuse its event's `event_id` as the set's id)
            // and opens a new set.
            if !matches!(&part, Ok((p, _)) if p.index == 0) {
                return out;
            }
            self.broken = None;
        }
        match part {
            _ if !self.bound => {
                let event_id = match &part {
                    Ok((_, e)) | Err((_, e, _)) => e.clone(),
                };
                self.broken = Some(id.clone());
                out.push(Joined::Refused(
                    ErrorObject::invalid(
                        "invalid_event",
                        format!(
                            "{EVENT_PART} applies to a call bound to a Chat thread (POST \
                             /v1/realtime/calls with chat_thread or chat_folder); the parts of \
                             event {} are dropped",
                            shown(&id)
                        ),
                    )
                    .with_param("type")
                    .for_event(event_id.as_deref()),
                ));
            }
            Err((id, event_id, why)) => self.fail(id, event_id.as_deref(), &why, &mut out),
            Ok((p, event_id)) if p.index != 0 => {
                let why = format!("part {} came where part 0 was due", p.index);
                self.fail(p.id, event_id.as_deref(), &why, &mut out);
            }
            Ok((p, event_id)) => {
                let open = Open {
                    id: p.id,
                    count: p.count,
                    next: 1,
                    text: String::new(),
                };
                self.append(open, p.data, event_id.as_deref(), &mut out);
            }
        }
        out
    }

    /// The open set's next message, which is one of its parts.
    fn next(&mut self, open: Open, p: EventPart, event_id: Option<String>, out: &mut Vec<Joined>) {
        let event_id = event_id.as_deref();
        if p.count != open.count {
            let why = format!("a part says {} parts, the set {}", p.count, open.count);
            return self.fail(open.id, event_id, &why, out);
        }
        if p.index != open.next {
            let why = format!("part {} came where part {} was due", p.index, open.next);
            return self.fail(open.id, event_id, &why, out);
        }
        let open = Open {
            next: open.next + 1,
            ..open
        };
        self.append(open, p.data, event_id, out);
    }

    /// `data` onto `open`, which it completes or keeps open.
    fn append(
        &mut self,
        mut open: Open,
        data: String,
        event_id: Option<&str>,
        out: &mut Vec<Joined>,
    ) {
        if let Some(limit) = self.limit {
            if open.text.len() + data.len() > limit {
                self.broken = Some(open.id.clone());
                out.push(Joined::Refused(
                    ErrorObject::invalid(
                        "event_too_large",
                        format!(
                            "the parts of event {} join to more than realtime.max_message_mb \
                             ({limit} bytes), the largest event a session takes; the set is \
                             dropped",
                            shown(&open.id)
                        ),
                    )
                    .for_event(event_id),
                ));
                return;
            }
        }
        open.text.push_str(&data);
        if open.next < open.count {
            self.open = Some(open);
        } else if is_part(&open.text) {
            let why = format!("they join to another {EVENT_PART}");
            self.fail(open.id, event_id, &why, out);
        } else {
            out.push(Joined::Event(open.text));
        }
    }

    /// Set `id` is broken: answered once, its remaining parts dropped.
    fn fail(&mut self, id: String, event_id: Option<&str>, why: &str, out: &mut Vec<Joined>) {
        out.push(Joined::Refused(
            ErrorObject::invalid(
                "invalid_event",
                format!(
                    "the parts of event {} cannot be joined: {why}; the set is dropped",
                    shown(&id)
                ),
            )
            .for_event(event_id),
        ));
        if !id.is_empty() {
            self.broken = Some(id);
        }
    }
}

/// A set's id as an error names it.
fn shown(id: &str) -> &str {
    if id.is_empty() {
        "(no id)"
    } else {
        id
    }
}

/// The set a part names, read or not.
fn id_of(part: &Read) -> &str {
    match part {
        Ok((p, _)) => &p.id,
        Err((id, _, _)) => id,
    }
}

/// Whether `text` is an `lmgw.event.part`: a JSON object of that `type`. A
/// message that does not even contain the type's name — every other event —
/// is not parsed here.
fn is_part(text: &str) -> bool {
    #[derive(Deserialize)]
    struct Kind<'a> {
        #[serde(rename = "type", borrow, default)]
        kind: Option<Cow<'a, str>>,
    }
    text.contains(EVENT_PART)
        && text.trim_start().starts_with('{')
        && serde_json::from_str::<Kind>(text)
            .ok()
            .and_then(|k| k.kind)
            .is_some_and(|k| k == EVENT_PART)
}

/// A part's fields (module doc).
fn read(text: &str) -> Read {
    #[derive(Deserialize)]
    struct Part {
        #[serde(default)]
        event_id: Option<String>,
        #[serde(flatten)]
        part: EventPart,
    }
    match serde_json::from_str::<Part>(text) {
        Ok(Part { part, event_id }) if part.count < 2 => {
            let why = format!(
                "its count is {}, and a set has at least 2 parts",
                part.count
            );
            Err((part.id, event_id, why))
        }
        Ok(Part { part, event_id }) if part.index >= part.count => {
            let why = format!(
                "its index {} is not below its count {}",
                part.index, part.count
            );
            Err((part.id, event_id, why))
        }
        Ok(Part { part, event_id }) => Ok((part, event_id)),
        Err(e) => {
            let v = serde_json::from_str::<serde_json::Value>(text).ok();
            let field = |k: &str| v.as_ref().and_then(|v| v[k].as_str()).map(str::to_string);
            let why = format!("a part does not read: {e}");
            Err((field("id").unwrap_or_default(), field("event_id"), why))
        }
    }
}
