//! A response of a session bound to a chat thread (chat-voice design §8.2,
//! §8.3): where the response's lifecycle meets the journal.
//!
//! - **Launch** ([`Core::launch_bound`]): instead of rendering the
//!   conversation, the response hands the journal its user entry and its
//!   reply slot — the committed turns no earlier response wrote — and
//!   starts the Chat's turn (`thread::turn`).
//! - **`empty_turn`**: a response that answers no owed turn and has no new
//!   words is refused — before `response.created` when its transcripts are
//!   in by then, else when they are (with `response.done {failed}`, as
//!   `transcription_failed` is). Realtime decides over every owed turn
//!   (`pending`), so a cough that cut a reply nobody heard still answers
//!   the question it cut.
//! - **End** ([`Core::bound_ended`]): at drain or cancel the reply slot
//!   gets what was heard — the whole reply, or its heard table's written
//!   text up to the cut — with the response's timing and served models.
//!
//! **A response that hears the user's audio** (voice-audio-input design
//! §3.1) launches with its new turns as spoken parts, and its user row
//! deferred until they are heard (`hearing`).

use std::time::Instant;

use super::super::input::has_words;
use super::super::protocol::{ContentPart, ErrorObject, Item, ResponseStatus, Role};
use super::super::thread::journal::{Ended, In, UserTurn};
use super::super::thread::reply::Heard;
use super::super::thread::turn::{self, Speaking};
use super::{Active, Core, Failure};
use crate::proxy::StopHandle;
use crate::store::{InputPath, ServedModel, VoiceModels};

impl Core {
    /// The committed turns no earlier response of this bound session handed
    /// to the journal, in conversation order (a bound session's user items
    /// are all committed turns: item creation is the thread's).
    fn unwritten_turns(&self) -> Vec<String> {
        let Some(b) = &self.bound else {
            return Vec::new();
        };
        self.conversation
            .items()
            .iter()
            .filter_map(|i| match i {
                Item::Message(m) if m.role == Role::User => m.id.clone(),
                _ => None,
            })
            .filter(|id| !b.submitted.contains(id))
            .collect()
    }

    /// The committed turns `ids`, for a user entry: their words, how each
    /// was transcribed and why it was not, and whether the chat model hears
    /// it (its transcript not in yet).
    pub(super) fn user_turns(&self, ids: &[String]) -> Vec<UserTurn> {
        ids.iter()
            .map(|id| UserTurn {
                item_id: id.clone(),
                text: transcript(self.conversation.get(id)),
                asr: self.bound.as_ref().and_then(|b| b.asr.get(id).cloned()),
                error: self
                    .bound
                    .as_ref()
                    .and_then(|b| b.asr_errors.get(id).cloned()),
                heard: self.hears(id),
            })
            .collect()
    }

    /// The session is ending (§8.6; WP8 review m9): the committed turns no
    /// launched response handed to the journal — one a response still
    /// awaiting its transcript answers, one committed while a response ran,
    /// or a push-to-talk commit not asked to be answered — are written as
    /// one user message, so "thanks, bye" and Esc keep the words. Their
    /// transcripts are awaited first: each ASR call ends on its own, as a
    /// turn's does (the session's stop goes with the core, after this).
    /// Their answer is not: no response starts now, and the thread's next
    /// turn answers them (§7.4 merges adjacent user messages). The turn the
    /// user was still speaking when the session ended was never committed,
    /// and is not written.
    ///
    /// The wait is bounded by `bound` — `realtime.ping_interval_s`, which
    /// bounds the writer's last events too; `None` when it is 0, the
    /// owner's "no bound" (WP11 binding review m1). An ASR call that hangs
    /// or loads a cold model for longer would hold the thread's binding —
    /// Keep refused, the page's close not answered, a re-entered window's
    /// first turn waiting behind the fence — with nothing said. Past the
    /// bound the calls are stopped, with a WARN naming how many turns lose
    /// their words.
    pub(in crate::realtime) async fn end_bound_turns(
        &mut self,
        asr: &mut tokio::sync::mpsc::UnboundedReceiver<super::super::transcribe::AsrMsg>,
        bound: Option<std::time::Duration>,
    ) {
        use super::super::transcribe::AsrMsg;
        let thread_id = self.bound.as_ref().map_or(0, |b| b.thread_id);
        let pending = self.transcriber.items().len();
        if pending > 0 {
            tracing::info!(
                "realtime {}: the session ends with {pending} committed turn(s) still being \
                 transcribed; it writes them to chat thread {thread_id} once they are{}",
                self.id(),
                bound.map_or(String::new(), |b| format!(
                    " (for up to {} s, realtime.ping_interval_s)",
                    b.as_secs_f64()
                ))
            );
        }
        let deadline = bound.map(|b| tokio::time::Instant::now() + b);
        let mut stopped = false;
        while self.transcriber.busy() {
            let msg = match deadline.filter(|_| !stopped) {
                Some(at) => match tokio::time::timeout_at(at, asr.recv()).await {
                    Ok(msg) => msg,
                    Err(_) => {
                        tracing::warn!(
                            "realtime {}: {} committed turn(s) were still being transcribed \
                             {} s after the session ended (realtime.ping_interval_s); their \
                             calls are stopped, and their words are not written to chat thread \
                             {thread_id}",
                            self.id(),
                            self.transcriber.items().len(),
                            bound.map_or(0.0, |b| b.as_secs_f64())
                        );
                        self.transcriber.stop();
                        stopped = true;
                        continue;
                    }
                },
                None => asr.recv().await,
            };
            match msg {
                Some(AsrMsg::Turn(done)) => self.on_last_transcript(done),
                Some(AsrMsg::Check(_)) => {}
                None => break,
            }
        }
        let ids = self.unwritten_turns();
        let turns = self.user_turns(&ids);
        if !turns.iter().any(|t| !t.text.trim().is_empty()) {
            return;
        }
        let Some(b) = self.bound.as_mut() else {
            return;
        };
        b.submitted.extend(ids);
        if let Some(j) = &b.journal {
            j.send(In::User { turns });
        }
    }

    /// Whether a response answering `answers` would answer nothing
    /// (module doc): no owed turn among them, and no new words.
    fn bound_empty(&self, answers: &[String]) -> bool {
        let Some(b) = &self.bound else {
            return false;
        };
        let owed = answers.iter().any(|id| b.submitted.contains(id));
        let hearing = Some(&b.hearing);
        let words = self.unwritten_turns().iter().any(|id| {
            self.conversation
                .get(id)
                .is_some_and(|i| has_words(i, hearing))
        });
        !owed && !words
    }

    /// The `empty_turn` refusal (module doc).
    fn empty_turn() -> ErrorObject {
        ErrorObject::invalid(
            "empty_turn",
            "this response would answer nothing: no new words were said, and no turn is owed \
             an answer — speak, then ask for a response",
        )
    }

    /// Before `response.created`: a bound response that would answer
    /// nothing, when the transcripts are in (module doc).
    pub(super) fn bound_refusal(&self, answers: &[String]) -> Option<ErrorObject> {
        let decidable = self.bound.is_some() && !self.busy_for_launch();
        (decidable && self.bound_empty(answers)).then(Self::empty_turn)
    }

    /// Launch the active bound response (module doc): `None` when it was
    /// refused (`empty_turn`), and has ended.
    pub(super) fn launch_bound(&mut self, speaking: Option<Speaking>) -> Option<StopHandle> {
        let active = self.active.as_ref()?;
        let (gen, response_id, answers) = (
            active.output.gen,
            active.output.id.clone(),
            active.answers.clone(),
        );
        if self.bound_empty(&answers) {
            self.end_call(Err(Failure {
                error: Self::empty_turn(),
                log: "it answers no owed turn, and no new words were said".into(),
            }));
            return None;
        }
        let ids = self.unwritten_turns();
        let turns = self.user_turns(&ids);
        let (stop, signal) = crate::proxy::stop_pair();
        let (reply, user) = tokio::sync::oneshot::channel();
        let hint = super::super::expressive::hint_on(
            &self.session,
            &self.state.snapshot().settings.realtime,
        );
        let label = format!("realtime {}", self.id());
        let Some(journal_tx) = self
            .bound
            .as_ref()
            .and_then(|b| b.journal.as_ref())
            .and_then(|j| j.tx())
        else {
            self.end_call(Err(Failure {
                error: ErrorObject {
                    kind: "server_error".into(),
                    ..ErrorObject::invalid("internal", "the session's journal is not running")
                },
                log: "the journal is not running".into(),
            }));
            return None;
        };
        let (row, audio) = self.launch_heard(gen, &ids, &answers).unzip();
        let b = self.bound.as_mut()?;
        b.submitted.extend(ids);
        b.responses.insert(gen, Default::default());
        b.response_ids.insert(gen, response_id.clone());
        let _ = journal_tx.send(In::Response {
            gen,
            response_id: response_id.clone(),
            turns,
            reply,
            row,
        });
        self.launch_input(gen, &response_id, audio.is_some());
        let b = self.bound.as_ref()?;
        tokio::spawn(turn::run(turn::Job {
            state: self.state.clone(),
            ctx: self.ctx.clone(),
            gen,
            label,
            thread_id: b.thread_id,
            tx: self.responder_tx.clone(),
            stop: signal,
            user,
            journal: Some(journal_tx),
            speaking,
            hint,
            audio,
        }));
        Some(stop)
    }

    /// A bound response ended at `end` with `status` (module doc): its
    /// reply slot gets what was heard, its timing and served models.
    pub(super) fn bound_ended(&mut self, active: &Active, status: ResponseStatus, end: Instant) {
        let gen = active.output.gen;
        let Some(served) = self.bound.as_mut().and_then(|b| b.responses.remove(&gen)) else {
            // Never launched: no slot to finalize.
            return;
        };
        let item = active.output.message_item(&self.conversation);
        let played = matches!(
            status,
            ResponseStatus::Completed | ResponseStatus::Incomplete
        );
        let heard = match &item {
            Some(id) if played && !self.conversation.heard_clipped(id) => Heard::Whole,
            Some(id) => self.heard_part(id),
            None if played => Heard::Whole,
            None => Heard::nothing(),
        };
        let st = active.timing.stages(end);
        let turn = active.timing.turn.as_ref();
        let asr = turn.and_then(|t| self.bound.as_ref()?.asr.get(&t.item_id).cloned());
        let mut cold = Vec::new();
        if asr.as_ref().is_some_and(|a| a.cold) {
            cold.push("asr".to_string());
        }
        if served.chat_cold {
            cold.push("chat".to_string());
        }
        if served.tts_cold {
            cold.push("tts".to_string());
        }
        let timing = crate::store::VoiceTiming {
            response_id: Some(active.output.id.clone()),
            message_id: None,
            end_of_turn_ms: st.end_of_turn_ms,
            asr_ms: st.asr_ms,
            first_token_ms: st.first_token_ms,
            reasoning_ms: st.reasoning_ms,
            first_clause_ms: st.first_clause_ms,
            first_audio_ms: st.first_audio_ms,
            total_ms: Some(st.total_ms),
            to_first_audio_ms: st
                .first
                .filter(|(what, _)| *what == "audio")
                .map(|(_, ms)| ms),
            cold,
            first_clause: served
                .first_announced
                .filter(|a| *a)
                .map(|_| "announcement".to_string()),
            models: VoiceModels {
                asr: asr.map(|a| ServedModel {
                    alias: a.alias,
                    answered_by: a.answered_by,
                    voice: None,
                }),
                chat: served.chat.clone().map(|alias| ServedModel {
                    alias,
                    answered_by: served.chat_answered_by.clone(),
                    voice: None,
                }),
                tts: served.tts.clone().map(|alias| ServedModel {
                    alias,
                    answered_by: served.tts_answered_by.clone(),
                    voice: served.voice.clone(),
                }),
            },
            // How its turns reached the chat model, while audio input is on
            // (voice-audio-input design §5): absent with it off, so the
            // timing is byte for byte as before.
            input: served.input.as_ref().map(|(i, _)| *i),
            input_why: served.input.as_ref().and_then(|(_, w)| w.clone()),
            // How long the hold kept its first output: the audio path's
            // alone — a retry over the transcript waited for its row, not a
            // hold (§5, WP3 review #7).
            transcript_wait_ms: st
                .transcript_wait_ms
                .filter(|_| matches!(served.input, Some((InputPath::Audio, _)))),
        };
        let Some(b) = self.bound.as_mut() else {
            return;
        };
        if let Some(id) = item {
            b.replies.insert(id, gen);
        }
        // Its turns are answered: a failed transcription a model heard is
        // no later response's to count (`hearing`).
        if played {
            b.asr_errors.retain(|id, _| !active.answers.contains(id));
        }
        // The next turn's verdict, judged again now: push-to-talk has no
        // `speech_started` to judge it at (`thread::verdict`).
        let rejudge = b.verdict_tx.is_some();
        if let Some(j) = &b.journal {
            j.send(In::Cut {
                gen,
                heard,
                ended: Some(Box::new(Ended {
                    timing,
                    tts: served.tts,
                    tts_answered_by: served.tts_answered_by,
                    voice: served.voice,
                })),
            });
        }
        if rejudge {
            self.rejudge_audio_input();
        }
    }

    /// What was heard of assistant item `id`, as the journal cuts its reply
    /// to (§8.4): up to the cut, and up to the last clause heard whole.
    pub(in crate::realtime) fn heard_part(&self, id: &str) -> Heard {
        Heard::Part {
            exact: self.conversation.heard_written(id),
            whole: self.conversation.heard_written_whole(id),
        }
    }

    /// The session is ending (§8.6): the active response is stopped — its
    /// turn saves its partial reply — and its slot gets what was sent, as a
    /// cancel keeps it (§7.3). No events: nobody reads them now.
    pub(in crate::realtime) fn end_bound_active(&mut self) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        if let Some(stop) = active.call.take() {
            stop.stop();
        }
        let sent = self.out.purge(active.output.gen);
        active.output.keep_sent(&mut self.conversation, &sent.audio);
        let status = match active.ended {
            Some(Ok(_)) => ResponseStatus::Completed,
            _ => ResponseStatus::Cancelled,
        };
        self.held_over(&active);
        self.bound_ended(&active, status, Instant::now());
    }
}

/// A committed turn's transcript (empty: none, or no words).
pub(super) fn transcript(item: Option<&Item>) -> String {
    let Some(Item::Message(m)) = item else {
        return String::new();
    };
    m.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::InputAudio { transcript, .. } => transcript.as_deref(),
            ContentPart::InputText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
