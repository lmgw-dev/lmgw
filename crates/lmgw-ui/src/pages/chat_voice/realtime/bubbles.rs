//! Voice turns in the thread's transcript (chat-voice §9.5), from a bound
//! session's events (§8.7):
//! - `lmgw.chat.user` adds the user bubble, with its mic badge, as the
//!   journal wrote it — before its response's reply bubble when the
//!   response heard the turn as audio: that row is written once the turn is
//!   transcribed, racing the reply's release (voice-audio-input §3.3);
//! - `lmgw.chat.frame` streams the reply as a text send does — the frames
//!   are parsed into the page's `ChatEvent` (`chat_stream::frame`) — faster
//!   than the voice says it. A response with no new words sends no `turn`
//!   frame (WP8 NIT 11): nothing here waits for one. The bubble carries a
//!   provisional `voice {via: realtime}` from its first frame, so its cues
//!   are chips while it streams (§9.7.4);
//! - `lmgw.chat.reply` re-renders the reply from the stored row: the heard
//!   text, the greyed rest, the timing; `removed` takes the bubble out;
//!   `skipped` keeps the text whole and adds a small note.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use leptos::prelude::*;
use lmgw_api_types::chat_frames::ToolFrame;
use lmgw_client::realtime::ChatReply;
use serde_json::Value;

use super::super::super::chat::{in_owner, new_msg, scroll_down, Msg};
use super::super::super::chat_reply::Finished;
use super::super::super::chat_stream::{frame as parse_frame, ChatEvent, ToolEvent, VoiceEvent};
use super::super::super::chat_turn::apply_tool_frame;
use super::super::spoken::MsgVoice;
use super::live::Live;

/// A new bubble, owned by the page (its signals live as long as it does).
fn bubble(l: &Live, role: &str, content: String) -> Option<Msg> {
    let p = l.rt().parts;
    in_owner(p.owner, || {
        let key = p.next_key.get_value();
        p.next_key.set_value(key + 1);
        new_msg(key, role, content)
    })
}

/// Still the thread on screen (a switch ends voice mode, but an event may
/// already be on its way).
fn here(l: &Live) -> bool {
    l.rt()
        .parts
        .current
        .try_with_untracked(|c| c.as_ref().map(|t| t.id))
        .flatten()
        == Some(l.tid)
}

pub(super) fn user(
    l: &Rc<Live>,
    message_id: i64,
    content: String,
    voice: &lmgw_api_types::chat_threads::MessageVoice,
    response_id: Option<&str>,
) {
    if !here(l) {
        return;
    }
    let Some(m) = bubble(l, "user", content) else {
        return;
    };
    m.db_id.set(Some(message_id));
    m.voice.set(MsgVoice::of_typed(voice));
    // Its response's reply may already stream: the turn goes before it.
    let reply = response_id.and_then(|rid| l.bubbles.borrow().get(rid).map(|r| r.key));
    l.rt().parts.msgs.try_update(|v| {
        let at = reply.and_then(|key| v.iter().position(|x| x.key == key));
        match at {
            Some(i) => v.insert(i, m),
            None => v.push(m),
        }
    });
    scroll_down(true);
}

/// The reply bubble of response `rid`, made at its first frame.
fn reply_bubble(l: &Rc<Live>, rid: &str) -> Option<Msg> {
    if let Some(m) = l.bubbles.borrow().get(rid) {
        return Some(m.clone());
    }
    if !here(l) {
        return None;
    }
    let m = bubble(l, "assistant", String::new())?;
    m.streaming.set(true);
    m.voice.set(Some(MsgVoice::provisional_reply()));
    l.rt().parts.msgs.try_update(|v| v.push(m.clone()));
    l.bubbles.borrow_mut().insert(rid.to_string(), m.clone());
    scroll_down(true);
    Some(m)
}

/// A relayed frame read into the page's event. The one frame that is not
/// read and still counts is a `done` that is not a whole `DoneFrame` (a
/// gateway that sent it a key this page does not know, say, on a tab left
/// open across an upgrade): in voice mode only `done` (or the stored
/// `reply`) clears `streaming`, so without this the bubble would spin for
/// good. It ends the bubble's streaming state and applies nothing else,
/// as `run_turn` ends its own at the end of the stream whatever was read.
fn read_frame(
    bubbles: &RefCell<HashMap<String, Msg>>,
    rid: &str,
    event: &str,
    data: Value,
) -> Option<ChatEvent> {
    let ev = parse_frame(event, data);
    if ev.is_none() && event == "done" {
        if let Some(m) = bubbles.borrow().get(rid) {
            m.streaming.set(false);
        }
    }
    ev
}

pub(super) fn frame(l: &Rc<Live>, rid: &str, event: &str, data: Value) {
    let Some(ev) = read_frame(&l.bubbles, rid, event, data) else {
        return;
    };
    // A model's state is the status line's: the gateway sends every chat
    // turn's `state` frame as `lmgw.model.state` too (`thread/hooks.rs`,
    // `bound_frame`), which the session applies (review NIT 12).
    if let ChatEvent::Voice(VoiceEvent::State(_)) = &ev {
        return;
    }
    // The user's own message is `lmgw.chat.user`'s.
    if matches!(ev, ChatEvent::Turn(_)) {
        return;
    }
    let Some(m) = reply_bubble(l, rid) else {
        return;
    };
    match ev {
        ChatEvent::Delta(text) => {
            m.content.update(|c| c.push_str(&text));
            scroll_down(false);
        }
        ChatEvent::Reasoning(text) => m.reasoning.update(|r| r.push_str(&text)),
        ChatEvent::Tool(step) => {
            apply_tool_frame(m.tools, &step);
            // The running tool shows on the status line; the visualisation
            // is `thinking` once the preamble played (§8.5, `machine.rs`).
            match &step {
                ToolEvent::Frame(ToolFrame::Start(f)) => l.tool(Some(f.name.clone())),
                ToolEvent::Frame(ToolFrame::Ready(f)) => l.tool(Some(f.name.clone())),
                ToolEvent::Frame(ToolFrame::Result(_)) => l.tool(None),
                _ => {}
            }
            scroll_down(false);
        }
        ChatEvent::Usage {
            prompt_tokens: Some(p),
            completion_tokens: Some(c),
        } => m.tokens.set(Some((p, c))),
        ChatEvent::Retrieval(c) => m.context.set(Some(c)),
        // The bubble keeps the gateway's message; the panel's line words it
        // by the response's `error` code (`live/errors.rs`).
        ChatEvent::Error { message: msg, .. } => {
            m.content.update(|c| {
                if !c.is_empty() {
                    c.push_str("\n\n");
                }
                c.push_str(&format!("⚠ {msg}"));
            });
        }
        ChatEvent::Superseded => {
            m.content.update(|c| {
                if !c.is_empty() {
                    c.push_str("\n\n");
                }
                c.push_str("⏹ stopped — the conversation moved on");
            });
        }
        ChatEvent::NotSaved(_) => m.unsaved.set(true),
        ChatEvent::Done(done) => {
            // The panel's line says it once: the model reasoned although
            // the voice turn asked for reasoning off.
            l.rt().status.reasoning(&done);
            let fin = Finished::from_done(&done);
            match fin.id {
                Some(id) => m.db_id.set(Some(id)),
                None if Finished::refused(&done) => m.unsaved.set(true),
                None => {}
            }
            if fin.model.is_some() {
                m.model.set(fin.model);
                m.answered_by.set(fin.answered_by);
            }
            m.streaming.set(false);
            l.tool(None);
        }
        _ => {}
    }
}

pub(super) fn reply(l: &Rc<Live>, r: &ChatReply) {
    let message_id = r.message_id;
    if !here(l) {
        return;
    }
    let msgs = l.rt().parts.msgs;
    let Some(m) = msgs
        .try_with_untracked(|v| {
            v.iter()
                .find(|m| m.db_id.get_untracked() == Some(message_id))
                .cloned()
        })
        .flatten()
    else {
        return;
    };
    m.streaming.set(false);
    if r.removed {
        // Heard by nobody, and no tool ran: the reply is gone from the
        // thread, and from the page.
        msgs.update(|v| v.retain(|x| x.key != m.key));
        return;
    }
    let mut voice = r.voice.as_ref().and_then(MsgVoice::of_typed);
    if let Some(why) = r.skipped.as_deref() {
        let mut v = voice
            .or_else(|| m.voice.get_untracked())
            .unwrap_or_else(MsgVoice::provisional_reply);
        v.note = Some(format!("not cut to what was heard: {why}"));
        m.voice.set(Some(v));
        return;
    }
    if let Some(content) = &r.content {
        m.content.set(content.clone());
    }
    if let Some(v) = voice.as_mut() {
        if v.unheard.is_none() {
            v.unheard = r.unheard.clone();
        }
    }
    if voice.is_some() {
        m.voice.set(voice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Response `r1`'s bubble, streaming, in an owner of its own.
    fn streaming_bubble() -> (RefCell<HashMap<String, Msg>>, Msg) {
        let m = new_msg(1, "assistant", String::new());
        m.streaming.set(true);
        let bubbles = RefCell::new(HashMap::from([("r1".to_string(), m.clone())]));
        (bubbles, m)
    }

    #[test]
    fn a_done_that_does_not_read_still_ends_the_bubbles_streaming() {
        Owner::new().with(|| {
            // An `aborted` frame a newer gateway gave a key: more than
            // `aborted` that is not a whole saved part is not a `DoneFrame`.
            let unread = json!({"aborted": true, "reason": "a newer gateway's key"});
            let (bubbles, m) = streaming_bubble();
            m.content.set("kept".into());
            assert!(read_frame(&bubbles, "r1", "done", unread).is_none());
            assert!(!m.streaming.get_untracked());
            // Nothing else was applied.
            assert_eq!(m.content.get_untracked(), "kept");
            assert_eq!(m.db_id.get_untracked(), None);
            assert!(!m.unsaved.get_untracked());
        });
    }

    #[test]
    fn only_a_done_ends_the_streaming_of_a_frame_that_does_not_read() {
        Owner::new().with(|| {
            let (bubbles, m) = streaming_bubble();
            // Another event that does not read leaves the bubble streaming,
            // and so does a `done` of another response.
            assert!(read_frame(&bubbles, "r1", "retrieval", json!({"hits": "x"})).is_none());
            assert!(read_frame(&bubbles, "r2", "done", json!({"aborted": 1})).is_none());
            assert!(m.streaming.get_untracked());
            // A `done` with no bubble yet makes none.
            let none = RefCell::new(HashMap::new());
            assert!(read_frame(&none, "r1", "done", json!({"aborted": 1})).is_none());
            assert!(none.borrow().is_empty());
        });
    }

    #[test]
    fn a_done_that_reads_is_left_to_the_frame_handler() {
        Owner::new().with(|| {
            let (bubbles, m) = streaming_bubble();
            let ev = read_frame(&bubbles, "r1", "done", json!({"aborted": true}));
            assert!(matches!(ev, Some(ChatEvent::Done(_))));
            // `frame` applies it, with the model and the tools; the read does not.
            assert!(m.streaming.get_untracked());
        });
    }
}
