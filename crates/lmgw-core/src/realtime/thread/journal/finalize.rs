//! The journal's reply slot (chat-voice design §8.3, §8.4): the finalize
//! with what was heard, a late truncate's re-cut, and the conditional
//! writes under the thread's generation.

use serde_json::{json, Map, Value};

use super::super::super::protocol::ServerEvent;
use super::super::reply::{decide, Heard, Write};
use super::{Done, Slot, Task};
use crate::store::{MessageVoice, VoiceTiming, VIA_REALTIME};
use crate::web::chat_voice::bound::{self, Guarded};

impl Task {
    /// A reply slot with both inputs in (module doc); then the response's
    /// timing, as stored with the reply.
    pub(super) async fn finalize(&mut self, gen: u64, slot: Slot) {
        let ended = slot.ended.map(|e| *e).unwrap_or_default();
        let (message_id, generation) = slot.saved.unwrap_or((None, None));
        let mut timing = ended.timing.clone();
        timing.response_id.get_or_insert(slot.response_id);
        timing.message_id = message_id;
        // What the core could not see of a cancelled response (NIT 6).
        if let Some(said) = slot.chat {
            match &mut timing.models.chat {
                None => timing.models.chat = Some(said),
                Some(chat) if chat.answered_by.is_none() => chat.answered_by = said.answered_by,
                Some(_) => {}
            }
        }
        // Nothing saved: no write — the next user message is merged with
        // this one when a request is built (§7.4).
        if let Some(id) = message_id {
            let voice = MessageVoice {
                via: VIA_REALTIME.into(),
                tts: ended.tts,
                tts_answered_by: ended.tts_answered_by,
                voice: ended.voice,
                timing: Some(timing.clone()),
                ..Default::default()
            };
            // Without its cut — the session ended before the core could say
            // (it always says) — the reply is annotated as saved.
            let heard = slot.heard.unwrap_or(Heard::Whole);
            let row = match bound::message(&self.state, self.thread_id, id).await {
                Ok(row) => row,
                // A store that refused the read (WP11 binding review NIT
                // 3): said, as a refused cut is, not taken for a reply
                // that is gone.
                Err(e) => {
                    tracing::warn!(
                        "{}: reply {id} of chat thread {} could not be read to write what was \
                         heard of it ({e}); it keeps its whole text and no voice",
                        self.label,
                        self.thread_id
                    );
                    None
                }
            };
            if let Some(row) = row {
                let mut done = Done {
                    message_id: id,
                    generation,
                    record: bound::record_text(&row),
                    original: row.content,
                    voice,
                    gone: false,
                    settled: false,
                    moved: false,
                };
                let write = decide(&done.original, done.record.as_deref(), &heard);
                self.write(&mut done, write, true).await;
                self.done.insert(gen, done);
            }
        }
        // A vetoed response ends quietly (voice-audio-input design §3.2).
        if !slot.vetoed {
            self.event(timing_event(&timing));
        }
    }

    /// A truncate after the slot finalized (module doc).
    pub(super) async fn recut(&mut self, gen: u64, heard: Heard) {
        let Some(mut done) = self.done.remove(&gen) else {
            return;
        };
        if done.moved && !done.gone {
            self.skipped(&mut done, false, "another turn started").await;
        } else if !done.gone && !done.settled {
            let write = decide(&done.original, done.record.as_deref(), &heard);
            if write != Write::Annotate {
                self.write(&mut done, write, false).await;
            }
        }
        self.done.insert(gen, done);
    }

    /// Write `write` for `done`'s reply; `first`: its finalize, which
    /// annotates a reply heard whole (a re-cut has nothing to add then).
    async fn write(&self, done: &mut Done, write: Write, first: bool) {
        let (tid, id) = (self.thread_id, done.message_id);
        let generation = done.generation;
        match write {
            Write::Annotate | Write::Unmatched => {
                if write == Write::Unmatched {
                    tracing::warn!(
                        "{}: reply {id} does not start with the text heard of it (the clause \
                         the cut fell in was said otherwise than written); it is left uncut",
                        self.label
                    );
                }
                if first && bound::annotate(&self.state, tid, id, &done.voice).await {
                    self.reply(id, &done.original, None, &done.voice);
                }
            }
            Write::Cut { content, unheard } => {
                let mut voice = done.voice.clone();
                voice.unheard = Some(unheard.clone());
                let Some(generation) = generation else {
                    return self
                        .skipped(done, first, "the turn's generation is not known")
                        .await;
                };
                match bound::cut_if(&self.state, tid, generation, id, &content, &voice).await {
                    Guarded::Written(true) => self.reply(id, &content, Some(&unheard), &voice),
                    Guarded::Written(false) => done.gone = true,
                    Guarded::Moved => self.skipped(done, first, "another turn started").await,
                }
            }
            Write::Delete => {
                let Some(generation) = generation else {
                    return self
                        .skipped(done, first, "the turn's generation is not known")
                        .await;
                };
                match bound::delete_if(&self.state, tid, generation, id).await {
                    Guarded::Written(written) => {
                        done.gone = true;
                        if written {
                            tracing::info!(
                                "{}: reply {id} was heard by nobody and ran no tools; deleted",
                                self.label
                            );
                            self.event(reply_event(id, json!({ "removed": true })));
                        }
                    }
                    Guarded::Moved => self.skipped(done, first, "another turn started").await,
                }
            }
        }
    }

    fn reply(&self, id: i64, content: &str, unheard: Option<&str>, voice: &MessageVoice) {
        self.event(reply_event(
            id,
            json!({ "content": content, "unheard": unheard, "voice": voice }),
        ));
    }

    /// A cut or delete that may not be written (module doc). At the
    /// finalize the reply still gets its `voice` — how it was spoken, its
    /// timing and models, no `unheard` — which moves nothing (WP8 review
    /// m3): it keeps its badge, and its text stays whole, as the turn that
    /// started meanwhile answered it. Said once per reply: the generation
    /// that moved never comes back, so a later re-cut has nothing to try.
    async fn skipped(&self, done: &mut Done, first: bool, why: &str) {
        let id = done.message_id;
        if std::mem::replace(&mut done.settled, true) {
            return;
        }
        tracing::warn!(
            "{}: reply {id} of chat thread {} was not cut to what was heard: {why} since it was \
             saved, and a voice finalize never rewrites a history another turn answers",
            self.label,
            self.thread_id
        );
        let annotated =
            first && bound::annotate(&self.state, self.thread_id, id, &done.voice).await;
        let mut body = json!({ "skipped": why });
        if annotated {
            body["voice"] = serde_json::to_value(&done.voice).unwrap_or(Value::Null);
        }
        self.event(reply_event(id, body));
    }
}

fn reply_event(message_id: i64, body: Value) -> ServerEvent {
    let Value::Object(reply) = body else {
        unreachable!("an object literal");
    };
    ServerEvent::LmgwChatReply { message_id, reply }
}

fn timing_event(timing: &VoiceTiming) -> ServerEvent {
    let timing = match serde_json::to_value(timing) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    ServerEvent::LmgwResponseTiming { timing }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_events_are_flat_and_read_back() {
        let ev = reply_event(7, json!({"removed": true}));
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({"type": "lmgw.chat.reply", "message_id": 7, "removed": true})
        );
        assert_eq!(serde_json::from_value::<ServerEvent>(v).unwrap(), ev);
        let timing = VoiceTiming {
            response_id: Some("resp_1".into()),
            asr_ms: Some(31),
            ..Default::default()
        };
        let v = serde_json::to_value(timing_event(&timing)).unwrap();
        assert_eq!(v["type"], "lmgw.response.timing");
        assert_eq!(
            (&v["response_id"], &v["asr_ms"]),
            (&json!("resp_1"), &json!(31))
        );
        assert!(v.get("first_clause").is_none(), "{v}");
    }
}
