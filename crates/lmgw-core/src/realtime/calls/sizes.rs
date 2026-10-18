//! A call's size bounds (calls design §6.3, C14): what a message on
//! `oai-events` may be, each way.
//!
//! **Out** ([`Shaper`]). The largest message a call sends is the smaller of
//! the client's `max-message-size` and `realtime.max_message_mb` (§3.7),
//! and nothing below it is imposed. An event larger than it cannot be one
//! message:
//! - **on a bound call it travels as parts** (`split`): consecutive
//!   `lmgw.event.part {id, index, count, data}` messages, `id` the event's
//!   `event_id`, `data` a slice of its JSON text cut at character
//!   boundaries so that each part's own JSON, with `data` escaped, fits;
//!   each part carries an `event_id` of its own, as every event of lmgw's
//!   does. They are handed to the driver as one write, so they go out back
//!   to back, nothing between them, and the event keeps its place;
//! - **on an unbound call** (a stock client cannot join parts) the event is
//!   sent with its largest fields emptied, largest first, until it fits
//!   (`empty`), and one `error` `event_too_large` per emptied field follows
//!   it, its `param` the field's path. Its `type`, `event_id` and every id
//!   are never emptied.
//!
//! What cannot be made to fit at all — a limit too small to carry a part,
//! an event whose ids alone are over it — is not sent, and said at WARN
//! naming the limit; the session goes on.
//!
//! **In** (`join`, [`Joiner`]). str0m advertises 256 KiB, and a conforming
//! sender refuses a larger message itself. A bound client may send an event
//! larger than that as parts of the same shape; the call's reader joins
//! them before the session parses anything, up to `realtime.max_message_mb`
//! (0: no bound), and refuses a broken set, a set joined past the setting
//! and a part on an unbound call with an `error` naming the set's `id`.

use std::sync::Arc;

use super::super::ids::Ids;
use super::super::protocol::{ErrorObject, ServerEvent, ServerFrame};
use super::handle::Limit;

mod empty;
mod join;
mod split;
#[cfg(test)]
mod tests;

pub(crate) use join::{Joined, Joiner};

/// What a call's outgoing messages are shaped to (module doc): cheap to
/// clone, one for the session's writer and one for the frame clock.
#[derive(Clone)]
pub(crate) struct Shaper {
    /// The call's id, for the log.
    call: Arc<str>,
    /// The largest message the call sends, and whose bound it is.
    max: usize,
    by: Limit,
    /// The call is bound to a Chat thread: an oversized event travels as
    /// parts; else its largest fields are emptied.
    bound: bool,
    /// The session's ids: every part and every error minted here carries
    /// an `event_id` of the session's.
    ids: Arc<Ids>,
}

impl Shaper {
    pub fn new(call: Arc<str>, (max, by): (usize, Limit), bound: bool, ids: Arc<Ids>) -> Self {
        Self {
            call,
            max,
            by,
            bound,
            ids,
        }
    }

    /// `text`, one event's JSON, as the messages that carry it (module doc):
    /// itself when it fits, else its parts, or the event emptied and its
    /// errors. Empty when it cannot be sent at all, which is said at WARN.
    pub fn shape(&self, text: String) -> Vec<String> {
        if text.len() <= self.max {
            return vec![text];
        }
        let shaped = if self.bound {
            split::split(&text, self.max, || self.ids.event())
        } else {
            self.emptied(&text)
        };
        match shaped {
            Ok(messages) => messages,
            Err(why) => {
                tracing::warn!(
                    "WebRTC call {}: an event of {} bytes ({}) is over {} and was not sent: {why}",
                    self.call,
                    text.len(),
                    split::kind_of(&text),
                    self.by.named(self.max)
                );
                Vec::new()
            }
        }
    }

    /// An unbound call's oversized event with its largest fields emptied,
    /// then one `event_too_large` per field (module doc). An error that is
    /// itself over the limit is said at WARN and not sent.
    fn emptied(&self, text: &str) -> Result<Vec<String>, String> {
        let fitted = empty::fit(text, self.max, &human(self.max))?;
        let mut messages = vec![fitted.text];
        for f in fitted.emptied {
            let message = format!(
                "{} {}: its field {} ({}) was emptied to fit {}",
                fitted.kind,
                fitted.event_id,
                f.path,
                human(f.size),
                self.by.named(self.max)
            );
            tracing::warn!("WebRTC call {}: {message}", self.call);
            let error = ErrorObject::invalid("event_too_large", message).with_param(f.path);
            let frame = ServerFrame {
                event_id: self.ids.event(),
                event: ServerEvent::error(error),
            };
            let Ok(t) = serde_json::to_string(&frame) else {
                continue;
            };
            if t.len() <= self.max {
                messages.push(t);
            } else {
                tracing::warn!(
                    "WebRTC call {}: the event_too_large error for {} is over {} itself and \
                     was not sent",
                    self.call,
                    fitted.event_id,
                    self.by.named(self.max)
                );
            }
        }
        Ok(messages)
    }
}

/// `bytes` as a person reads a size: bytes below 1 KiB, whole KiB below
/// 1 MiB, MiB to one decimal above.
pub(crate) fn human(bytes: usize) -> String {
    const KIB: usize = 1024;
    if bytes < KIB {
        format!("{bytes} bytes")
    } else if bytes < KIB * KIB {
        format!("{} KiB", (bytes + KIB / 2) / KIB)
    } else {
        format!("{:.1} MiB", bytes as f64 / (KIB * KIB) as f64)
    }
}
