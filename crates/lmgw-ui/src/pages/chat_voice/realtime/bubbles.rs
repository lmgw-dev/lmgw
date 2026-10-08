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

use std::rc::Rc;

use leptos::prelude::*;
use lmgw_client::realtime::ChatReply;
use serde_json::Value;

use super::super::super::chat::{in_owner, new_msg, scroll_down, Msg};
use super::super::super::chat_reply::Finished;
use super::super::super::chat_stream::{frame as parse_frame, ChatEvent};
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
    voice: &Value,
    response_id: Option<&str>,
) {
    if !here(l) {
        return;
    }
    let Some(m) = bubble(l, "user", content) else {
        return;
    };
    m.db_id.set(Some(message_id));
    m.voice.set(MsgVoice::of_value(voice));
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

pub(super) fn frame(l: &Rc<Live>, rid: &str, event: &str, data: Value) {
    let Some(ev) = parse_frame(event, data) else {
        return;
    };
    // A model's state is the status line's: the gateway sends every chat
    // turn's `state` frame as `lmgw.model.state` too (`thread/hooks.rs`,
    // `bound_frame`), which the session applies (review NIT 12).
    if let ChatEvent::Voice("state", _) = &ev {
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
        ChatEvent::Tool(v) => {
            apply_tool_frame(m.tools, &v);
            // The running tool shows on the status line; the visualisation
            // is `thinking` once the preamble played (§8.5, `machine.rs`).
            match v["event"].as_str().unwrap_or_default() {
                "start" | "ready" => {
                    l.tool(Some(v["name"].as_str().unwrap_or("a tool").to_string()));
                }
                "result" => l.tool(None),
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
    let mut voice = MsgVoice::of_value(&r.voice);
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
