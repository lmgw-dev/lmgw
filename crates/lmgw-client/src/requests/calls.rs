//! A voice session over WebRTC (realtime calls design §2): OpenAI's
//! `POST /v1/realtime/calls` bound to a Chat thread or to an ongoing
//! folder's current thread, and its hangup. The session behind it is a
//! WebSocket session's ([`crate::realtime`]); its events travel on the
//! call's `oai-events` data channel, the audio on RTP.
//!
//! - [`call`]: the offer, as `application/sdp`, or as `multipart/form-data`
//!   with the `session` part, the client's own fields
//!   ([`BoundSession`](crate::realtime::BoundSession)) that lmgw applies as
//!   the session's first update.
//!   The query binds as the WebSocket's does ([`CallBind`]), with
//!   `takeover` ([`Takeover`]); `ask` is the calls' own. Read the answer
//!   with [`read_call`]: the call id from `Location`, the SDP answer from
//!   the body, a refusal through [`read_refusal`], with the holder in
//!   [`ApiError::held_by`] when that is why.
//! - [`hangup`]: `POST /v1/realtime/calls/{call_id}/hangup`, taking no
//!   concurrency slot, so it works at the key's limit. For a call whose
//!   connection is gone (Reconnect starts with it); a client ends a live
//!   call with `lmgw.response.stop` and closing `oai-events`. Read with
//!   [`read_hangup`]: a 404 means lmgw ended the call already.
//!
//! A call only checks at the POST; it binds when `oai-events` opens, and
//! says how on the channel: `session.created`, `lmgw.session.bind_pending`
//! under `takeover=ask`, or `lmgw.session.closed` after a refusal at the
//! open ([`crate::realtime::ServerEvent`]).

use super::{read_refusal, ApiError, Method, Request};
use crate::realtime::BoundSession;

/// What a call binds (§2.2), as `?chat_thread=` or `?chat_folder=` do on
/// the WebSocket upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallBind {
    /// `chat_thread=<id>` (negative: a temporary thread).
    Thread(i64),
    /// `chat_folder=<id>[&new=true]`: the ongoing folder's current thread,
    /// which lmgw decides at the bind by `current`'s rules, under the
    /// folder's lock: kept, or a new one when there is none, it is out of
    /// reach, it idled past the folder's idle minutes, or with `new` (a new
    /// conversation) unless it is empty. The thread bound is
    /// `session.lmgw.resolved.chat_thread`. Refusals: 404
    /// `chat_folder_not_found`, 409 `not_ongoing`, 409 `folder_no_model`.
    Folder { id: i64, new: bool },
}

/// `takeover` (§2.8): what the bind does with a thread another session
/// holds when the call's channel opens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Takeover {
    /// Left out of the query: take it over. The holder closes as taken over.
    #[default]
    Always,
    /// `never`: refused, at the POST with 409 `chat_thread_bound`, whose
    /// [`ApiError::held_by`] names the holder, or at the open with
    /// `lmgw.session.closed {kind: "thread_bound", by}`.
    Never,
    /// `ask` (calls only): the call connects and waits with
    /// `lmgw.session.bind_pending {held_by}` for the client's
    /// `lmgw.session.take_over`, or binds by itself when the holder ends.
    /// The Android client always binds so.
    Ask,
}

impl Takeover {
    fn query(self) -> Option<&'static str> {
        match self {
            Takeover::Always => None,
            Takeover::Never => Some("never"),
            Takeover::Ask => Some("ask"),
        }
    }
}

/// `POST /v1/realtime/calls?<bind>[&takeover=…]` with the SDP offer: the
/// offer alone as `application/sdp` when `session` is `None`, else
/// `multipart/form-data` with the parts `sdp` and `session` (its JSON,
/// which lmgw applies as the session's first update, right after
/// `session.created`). Send it with the device's credentials; read the
/// answer with [`read_call`].
pub fn call(
    bind: CallBind,
    takeover: Takeover,
    offer_sdp: &str,
    session: Option<&BoundSession>,
) -> Request {
    let mut query = match bind {
        CallBind::Thread(id) => format!("chat_thread={id}"),
        CallBind::Folder { id, new: false } => format!("chat_folder={id}"),
        CallBind::Folder { id, new: true } => format!("chat_folder={id}&new=true"),
    };
    if let Some(t) = takeover.query() {
        query.push_str("&takeover=");
        query.push_str(t);
    }
    let r = Request::new(Method::Post, format!("{PATH}?{query}"));
    match session {
        None => {
            let mut r = r.header("Content-Type", "application/sdp");
            r.body = Some(offer_sdp.to_string());
            r
        }
        Some(s) => {
            let session = s.to_value().to_string();
            let (boundary, body) = multipart(&[
                ("sdp", "application/sdp", offer_sdp),
                ("session", "application/json", &session),
            ]);
            let mut r = r.header(
                "Content-Type",
                &format!("multipart/form-data; boundary={boundary}"),
            );
            r.body = Some(body);
            r
        }
    }
}

/// `/v1/realtime/calls`.
const PATH: &str = "/v1/realtime/calls";

/// A `multipart/form-data` body of text parts `(name, content type, text)`,
/// and its boundary: one that occurs in none of the parts.
fn multipart(parts: &[(&str, &str, &str)]) -> (String, String) {
    let mut boundary = "lmgw-call-part".to_string();
    let mut n = 0;
    while parts.iter().any(|(_, _, text)| text.contains(&boundary)) {
        n += 1;
        boundary = format!("lmgw-call-part-{n}");
    }
    let mut body = String::new();
    for (name, content_type, text) in parts {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\
             Content-Type: {content_type}\r\n\r\n{text}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    (boundary, body)
}

/// A call's answer ([`read_call`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallAnswer {
    /// The call's id (`rtc_…`), from `Location`. Keep it to hang up the call
    /// should its connection go without a word ([`hangup`]).
    pub call_id: String,
    /// The SDP answer, to set as the remote description.
    pub sdp: String,
}

impl CallAnswer {
    /// The largest message lmgw takes on `oai-events`, as the answer
    /// advertises it ([`crate::realtime::parts::max_message_size`]): what
    /// the client splits its own events at
    /// ([`crate::realtime::Splitter`]).
    pub fn max_message_size(&self) -> u64 {
        crate::realtime::parts::max_message_size(&self.sdp)
    }
}

/// [`call`]'s answer: its status, its `Location` header (`None` when it has
/// none) and its body. `201` carries the call: its id from the last
/// segment of `Location` (`/v1/realtime/calls/{call_id}`), the SDP answer
/// as the body. A refusal reads through [`read_refusal`]: §2.5's codes, the
/// folder's, the key's ([`super::key_refused`]), and the 409
/// `chat_thread_bound` of `takeover=never` with the holder in
/// [`ApiError::held_by`].
pub fn read_call(status: u16, location: Option<&str>, body: &str) -> Result<CallAnswer, ApiError> {
    if !(200..300).contains(&status) {
        return Err(read_refusal(status, body));
    }
    let unreadable = |message: String| ApiError {
        code: "unreadable".into(),
        message,
        ..ApiError::default()
    };
    let Some(call_id) = location.and_then(call_id_of) else {
        return Err(unreadable(format!(
            "the call's answer names no call in its Location (got {:?})",
            location.unwrap_or_default()
        )));
    };
    if !body.trim_start().starts_with("v=") {
        return Err(unreadable(
            "the call's answer carries no SDP answer".to_string(),
        ));
    }
    Ok(CallAnswer {
        call_id,
        sdp: body.to_string(),
    })
}

/// The call id in `location`: the segment after `calls/` at the end of its
/// path, of letters, digits, `_` and `-` (lmgw's are `rtc_` and hex).
fn call_id_of(location: &str) -> Option<String> {
    let path = location.split(['?', '#']).next()?;
    let mut segments = path.rsplit('/');
    let id = segments.next()?;
    let ok = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    (ok && segments.next() == Some("calls")).then(|| id.to_string())
}

/// `POST /v1/realtime/calls/{call_id}/hangup`: end call `call_id`, for a
/// call whose connection is gone. It answers once the call's binding is
/// let go, so a call posted right after it is neither refused nor left
/// pending by the call it ended. Read with [`read_hangup`].
pub fn hangup(call_id: &str) -> Request {
    Request::new(Method::Post, format!("{PATH}/{}/hangup", segment(call_id)))
}

/// `id` as one path segment: bytes beyond RFC 3986's unreserved ones
/// percent-encoded.
fn segment(id: &str) -> String {
    id.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// What a hangup did ([`read_hangup`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HungUp {
    /// `200`: the call's end has begun, and its binding is let go.
    Ended,
    /// `404` (`call_not_found`): there is no such call for this key — lmgw
    /// ended it already. As good as [`HungUp::Ended`].
    AlreadyEnded,
}

/// [`hangup`]'s answer: its status and body.
pub fn read_hangup(status: u16, body: &str) -> Result<HungUp, ApiError> {
    match status {
        200..=299 => Ok(HungUp::Ended),
        404 => Ok(HungUp::AlreadyEnded),
        _ => Err(read_refusal(status, body)),
    }
}

#[cfg(test)]
mod tests;
