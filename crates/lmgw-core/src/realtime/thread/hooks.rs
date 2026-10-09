//! The session core's side of a binding (chat-voice design §8): what the
//! core does differently for a bound session, called from its own modules
//! at the points where it differs. What needs a response's own state is
//! `lifecycle::bound`'s.

use serde_json::{Map, Value};
use tokio::sync::mpsc;

use super::super::protocol::{ChatThreadRef, ErrorObject, ServerEvent};
use super::super::responder::{Msg, TtsEvent};
use super::super::session::Core;
use super::super::transcribe::Done;
use super::super::warm::{ModelState, WarmOutcome};
use super::journal::{In, Journal};

/// An `lmgw.model.state` event from §4.3's shape.
fn state_event(s: &impl serde::Serialize) -> ServerEvent {
    let state = match serde_json::to_value(s) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    ServerEvent::LmgwModelState { state }
}

impl Core {
    /// Start a bound session's journal (§8.3); its events, the connect
    /// warm's states and the audio-input verdicts judged off the core
    /// (`verdict`) go to the session's loop.
    pub(in crate::realtime) fn start_bound(
        &mut self,
        events: mpsc::UnboundedSender<ServerEvent>,
        states: mpsc::UnboundedSender<ModelState>,
        verdicts: mpsc::UnboundedSender<(u64, crate::web::chat_voice::bound::Shown)>,
    ) {
        let label = format!("realtime {}", self.id());
        let state = self.state.clone();
        let caller = crate::web::chat_voice::bound::Caller::of(&self.ctx);
        if let Some(b) = self.bound.as_mut() {
            let fence = b.fence.take();
            b.journal = Some(Journal::spawn(
                state,
                b.thread_id,
                label,
                events,
                fence,
                caller,
            ));
            b.states = Some(states);
            b.verdict_tx = Some(verdicts);
        }
    }

    /// An event the journal sent: `lmgw.chat.thread` — a heard turn's row
    /// named an untitled thread (voice-audio-input design §3.3) — goes
    /// through [`Self::thread_seen`]'s compare, so it is said once and
    /// `session.lmgw.resolved` follows; any other goes out as it came.
    pub(in crate::realtime) fn journal_event(&mut self, ev: ServerEvent) {
        match ev {
            ServerEvent::LmgwChatThread { chat_thread } => self.thread_seen(chat_thread),
            ev => self.ob.send(ev),
        }
    }

    /// Another window bound the thread (§8.1): said once, before the close,
    /// naming who (client-apps design §1.7).
    pub(in crate::realtime) fn taken_over(&mut self) {
        let id = self.bound.as_ref().map_or(0, |b| b.thread_id);
        let reason = super::taken_over_reason(self.taken_by().as_deref());
        self.error(ErrorObject::invalid(
            "chat_thread_taken_over",
            format!("{reason}: it bound chat thread {id}, and this session closes"),
        ));
    }

    /// The session's thread left its device's reach (client-apps design
    /// L3, review W3-1): said once, before the close, as the bind answers a
    /// thread the device cannot see — and as neutrally (review W4-18): not
    /// why.
    pub(in crate::realtime) fn out_of_reach(&mut self) {
        let id = self.bound.as_ref().map_or(0, |b| b.thread_id);
        self.error(ErrorObject::invalid(
            "chat_thread_not_found",
            format!("chat thread {id} is out of reach for this key, and this session closes"),
        ));
    }

    /// Who took the session's thread over, once it was.
    pub(in crate::realtime) fn taken_by(&self) -> Option<String> {
        self.bound.as_ref().and_then(|b| b.taken_by.get())
    }

    /// A model state the connect warm said (§4.3): `lmgw.model.state`.
    pub(in crate::realtime) fn model_state(&mut self, s: ModelState) {
        self.ob.send(state_event(&s));
    }

    /// A bound turn's frame (§8.2): relayed as `lmgw.chat.frame`, whatever
    /// became of its response, and read for the response's models and, at
    /// `done`, for the job results its saved reply answered.
    pub(in crate::realtime) fn bound_frame(&mut self, gen: u64, event: &'static str, data: Value) {
        let Some(b) = self.bound.as_mut() else {
            return;
        };
        // A response that has ended is read no more; its frames still go out.
        if let Some(served) = b.responses.get_mut(&gen) {
            match event {
                "done" => {
                    served.chat_answered_by = data["answered_by"].as_str().map(str::to_string);
                }
                "state" if data["stage"] == "chat" && data["state"] == "loading" => {
                    served.chat_cold = true;
                }
                _ => {}
            }
        }
        // A reply saved answers every job result stored before it (MCP
        // Tasks design §3.4): it was in its request.
        if event == "done" && data["saved"] == true {
            if let Some(id) = data["message_id"].as_i64().filter(|id| *id > 0) {
                b.tasks.answered_through(id);
            }
        }
        let response_id = b.response_ids.get(&gen).cloned().unwrap_or_default();
        if event == "state" {
            // A model loading is the session's model state too (§4.3).
            self.ob.send(state_event(&data));
        }
        // A gated call: OpenAI's item too (client-apps design §6.4).
        let approval = event == "tool" && data["event"] == "approval";
        if approval {
            self.approval_requested(&data);
        }
        self.ob.send(ServerEvent::LmgwChatFrame {
            response_id,
            event: event.to_string(),
            data,
        });
    }

    /// What a bound response answers and speaks with, once planned — and
    /// the thread as it re-read it: when that differs from what the session
    /// said (a title its first spoken turn named, the admin tools switched
    /// on or off), the session's `lmgw.resolved.chat_thread` takes it and
    /// `lmgw.chat.thread` says it (§8.7, WP8 review m10).
    pub(in crate::realtime) fn bound_planned(
        &mut self,
        gen: u64,
        (chat, tts): (String, Option<String>),
        thread: ChatThreadRef,
        voice: super::reshape::Reshape,
    ) {
        if let Some(served) = self.bound.as_mut().and_then(|b| b.responses.get_mut(&gen)) {
            served.chat = Some(chat);
            served.tts = tts;
        }
        self.thread_seen(thread);
        self.reshape(voice);
    }

    /// The thread as it was re-read: when that differs from what the
    /// session said, `session.lmgw.resolved.chat_thread` takes it and
    /// `lmgw.chat.thread` says it.
    fn thread_seen(&mut self, thread: ChatThreadRef) {
        let resolved = self
            .session
            .lmgw
            .get_or_insert_with(Default::default)
            .resolved
            .get_or_insert_with(Default::default);
        if resolved.chat_thread.as_ref() != Some(&thread) {
            resolved.chat_thread = Some(thread.clone());
            self.ob.send(ServerEvent::LmgwChatThread {
                chat_thread: thread,
            });
        }
    }

    /// What a bound response's speaker said that the core keeps (§4.3,
    /// §8.4, §8.7): the TTS route's opening — `lmgw.model.state` for a model
    /// that has to load, and who answered — and whether its first clause
    /// was an announcement. Read whatever became of the response.
    pub(in crate::realtime) fn bound_note(&mut self, gen: u64, msg: &Msg) {
        let state = self.state.clone();
        let Some(served) = self.bound.as_mut().and_then(|b| b.responses.get_mut(&gen)) else {
            return;
        };
        let alias = served.tts.clone().unwrap_or_default();
        let mut said = None;
        match msg {
            Msg::Tts(TtsEvent::Opening) => {
                if crate::web::chat_voice::bound::cold(&state, &alias) {
                    served.tts_cold = true;
                    served.tts_loading = Some(std::time::Instant::now());
                    said = Some(ModelState::loading("tts", &alias));
                }
            }
            Msg::Tts(TtsEvent::Opened { answered_by, voice }) => {
                served.tts_answered_by = answered_by.clone();
                served.voice = voice.clone();
                if let Some(at) = served.tts_loading.take() {
                    let outcome = match answered_by {
                        Some(a) => WarmOutcome::Fallback {
                            answered_by: a.clone(),
                        },
                        None => WarmOutcome::Ready {
                            ms: Some(at.elapsed().as_millis() as u64),
                        },
                    };
                    said = ModelState::of("tts", &alias, &outcome);
                }
            }
            Msg::Clause { written, .. } => {
                served.first_announced.get_or_insert(written.announcement);
            }
            _ => {}
        }
        if let Some(s) = said {
            self.ob.send(state_event(&s));
        }
    }

    /// A committed turn's transcription is in: how it was transcribed, for
    /// its user message and the response's timing — and, for a turn a
    /// response went with as audio (`launched`), why its transcription
    /// failed, when it was one (`input::attempted`): what its row says
    /// once the response's attempt is known to have carried it
    /// (voice-audio-input design §3.2, `lifecycle::hearing`).
    pub(in crate::realtime) fn bound_transcribed(&mut self, done: &Done, launched: bool) {
        if let Some(b) = self.bound.as_mut() {
            b.asr.insert(done.item_id.clone(), done.facts.clone());
            match &done.result {
                Err(e) if launched && super::super::input::attempted(e) => {
                    b.asr_errors.insert(done.item_id.clone(), e.to_string());
                }
                _ => {}
            }
        }
    }

    /// `item_id` was truncated: when it is the reply of a response that has
    /// ended, the journal re-cuts it (§8.3). The response still running
    /// is cut by its cancel, which follows.
    pub(in crate::realtime) fn bound_truncated(&mut self, item_id: &str) {
        let Some(b) = self.bound.as_ref() else {
            return;
        };
        let Some(&gen) = b.replies.get(item_id) else {
            return;
        };
        let heard = self.heard_part(item_id);
        if let Some(j) = &b.journal {
            j.send(In::Cut {
                gen,
                heard,
                ended: None,
            });
        }
    }

    /// The session is ending (§8.6): the response still running is stopped
    /// — its turn saves its partial reply — and its slot gets the last heard
    /// cut; the turns committed and not yet answered are written, once
    /// transcribed (`asr`: the session's ASR answers; `bound`: how long
    /// for, `end_bound_turns`); then the journal drains, so no write is
    /// lost or left half done. The drain is said at INFO with its time: the
    /// page's close, Keep and a re-entered window wait for it.
    pub(in crate::realtime) async fn end_bound(
        &mut self,
        asr: &mut mpsc::UnboundedReceiver<super::super::transcribe::AsrMsg>,
        bound: Option<std::time::Duration>,
    ) {
        let Some(thread_id) = self.bound.as_ref().map(|b| b.thread_id) else {
            return;
        };
        let started = std::time::Instant::now();
        self.end_bound_active();
        self.end_bound_turns(asr, bound).await;
        let journal = self.bound.as_mut().and_then(|b| b.journal.take());
        if let Some(mut journal) = journal {
            journal.drain().await;
        }
        tracing::info!(
            "realtime {}: chat thread {thread_id} holds everything the session said ({} ms \
             after its end)",
            self.id(),
            started.elapsed().as_millis()
        );
    }
}
