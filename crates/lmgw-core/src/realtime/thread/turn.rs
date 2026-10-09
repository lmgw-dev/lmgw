//! A bound session's response (chat-voice design §8.2): its LLM step is the
//! Chat's own turn, so the thread's model, prompt (with §8.5's voice
//! block), sampling, reasoning, attachments, knowledge bases and MCP tools
//! apply through the same code as a text send.
//!
//! In order:
//! 1. **The barrier**: the journal's answer to the response's user entry —
//!    the user message's id once every earlier entry is written (§8.3).
//! 2. **The thread, re-read**: gone is `chat_thread_not_found`, an Admin
//!    Chat thread `chat_thread_admin` (§8.1's check before each response;
//!    a thread's kind never changes, so the bind's check holds and this
//!    one catches a thread that went away). A speaking response then plans
//!    its speech as the read-aloud does (`web::chat_voice::speech::plan`):
//!    the thread's TTS, voice, style, language, announcements and seed, as
//!    re-read — a chip changed since applies now — or the refusal
//!    (`tts_not_configured`, `voice_not_found`, …).
//! 3. **The turn**: `start_turn_into` with the response's stop, the
//!    voice-turn flag and the thread's language — spoken or read, every
//!    bound turn is asked to answer in it (§8.5, 2026-10-04). Every frame is relayed as `lmgw.chat.frame`; `delta`
//!    text goes to the session's clause splitter (speaking) or straight to
//!    the core (text output), as a realtime response's deltas do; a tool's
//!    start flushes the clause in progress. Tool calls never become
//!    `function_call` items: the client must not run them.
//! 4. **What it saved** goes to the journal ([`journal::In::Saved`]):
//!    always, on every path, so the reply slot finalizes.
//!
//! A cancel is the response's stop: the turn ends as `Interrupted`, saves
//! its partial reply and still says `done`, whose id the journal gets. A
//! voice that fails stops the turn the same way, and is the response's
//! error.
//!
//! **A continuation** (MCP Tasks design §3.4, [`Job::answer`]) has no user
//! message: its turn is the one `POST …/answer` runs, which starts only on
//! an idle thread and only while a job result is still unanswered once its
//! start delivered what waits (`web::chat_voice::bound::start_turn`).
//!
//! **A heard response** (voice-audio-input design §3.5) goes with the
//! user's audio first, and once more as its transcript when that attempt
//! is refused (`audio`).

use futures::FutureExt;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::super::protocol::ErrorObject;
use super::super::responder::{self, speak, Mark, Msg, Speaker, Splitter};
use super::super::writer::Progress;
use super::journal::{self, In};
use crate::agent::DeltaSink;
use crate::error::GatewayError;
use crate::ingress::ClientProto;
use crate::ir::{Completion, ContentPart, FinishReason, StreamDelta, Usage};
use crate::proxy::{stop_pair, RequestCtx, StopHandle, StopSignal};
use crate::state::SharedState;
use crate::store::ServedModel;
use crate::web::chat_voice::bound::{self, TurnFrame, VoiceTurn};

pub(crate) mod audio;

/// How a speaking response is paced: the session's writer and bounds.
pub(crate) struct Speaking {
    pub progress: Progress,
    /// `synthesis_ahead_s` in samples at 24 kHz; `None` for no bound.
    pub ahead: Option<u64>,
    /// `longest_pause_ms`; 0 keeps the engine's silences.
    pub longest_pause_ms: u32,
    pub speed: Option<f64>,
}

/// One bound response's turn.
pub(crate) struct Job {
    pub state: SharedState,
    pub ctx: RequestCtx,
    pub gen: u64,
    /// What the log lines start with: `realtime <session>`.
    pub label: String,
    pub thread_id: i64,
    pub tx: responder::Tx,
    /// The response's cooperative stop.
    pub stop: StopSignal,
    /// The journal's answer to the response's user entry (module doc).
    pub user: oneshot::Receiver<journal::UserAnswer>,
    pub journal: Option<journal::Tx>,
    /// `None` for text output.
    pub speaking: Option<Speaking>,
    /// `realtime.tag_hint`: the voice turn's prompt says what brackets do.
    pub hint: bool,
    /// A heard response's audio input (`audio`); `None`: its turns go as
    /// their transcripts.
    pub audio: Option<audio::Launch>,
    /// Its turns go as their transcripts because the model lacks audio
    /// input: what its request rows say (`request_logs.degraded`).
    pub degraded: Option<String>,
    /// The response resumes the thread's gated reply with the session's
    /// decisions (client-apps design §6.4): what it resumes, once decided.
    pub resume: Option<bound::Resume>,
    /// The session's answers to the thread's waiting calls, decided before
    /// the turn starts (`thread::approvals`).
    pub verdicts: Option<Vec<crate::store::Verdict>>,
    /// A continuation (MCP Tasks design §3.4): no new words, and job
    /// results nothing answered yet — the turn `POST …/answer` runs.
    pub answer: bool,
}

/// What the turn saved, for the journal — sent once, on every path.
struct Saved {
    journal: Option<journal::Tx>,
    gen: u64,
    sent: bool,
}

impl Saved {
    fn send(
        &mut self,
        message_id: Option<i64>,
        generation: Option<u64>,
        chat: Option<ServedModel>,
    ) {
        if std::mem::replace(&mut self.sent, true) {
            return;
        }
        if let Some(j) = &self.journal {
            let _ = j.send(In::Saved {
                gen: self.gen,
                message_id,
                generation,
                chat,
            });
        }
    }
}

impl Drop for Saved {
    fn drop(&mut self) {
        self.send(None, None, None);
    }
}

/// Run `job` to its end: the result goes to the core as
/// [`Msg::Finished`], what was saved to the journal. A panic is
/// `Err(Internal)`, as the realtime responder's.
pub(crate) async fn run(mut job: Job) {
    let mut saved = Saved {
        journal: job.journal.take(),
        gen: job.gen,
        sent: false,
    };
    let result = std::panic::AssertUnwindSafe(call(&mut job, &mut saved))
        .catch_unwind()
        .await
        .unwrap_or_else(|panic| {
            let why = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "no message".into());
            tracing::error!("{}: the bound response's turn panicked: {why}", job.label);
            Err(GatewayError::Internal(format!(
                "the response failed inside lmgw: {why}"
            )))
        });
    drop(saved);
    let _ = job.tx.send((job.gen, Msg::Finished(Box::new(result))));
}

/// A refusal of this response by its own code: a request error
/// (`invalid_request_error`, as the stock session says `tts_not_configured`),
/// not a permission one — the thread is gone or is an Admin Chat, another
/// turn took it, the reply cannot be saved, or its speech cannot be planned
/// (WP11 binding review NIT 2). It names no `param`: what it is about is
/// the thread's, which a bound client does not set.
fn refused(code: &'static str, message: String) -> GatewayError {
    GatewayError::InvalidRequest { code, message }
}

/// An `ErrorObject` the journal answered with, as the response's error: a
/// thread that went, or a history write the store refused (WP8 review m11),
/// which is lmgw's own failure (`server_error`, `Failure::of_call`).
fn from_object(e: ErrorObject) -> GatewayError {
    match e.code.as_deref() {
        Some("chat_thread_not_found") => refused("chat_thread_not_found", e.message),
        _ => GatewayError::Refused {
            status: 500,
            code: "chat_history_write_failed",
            message: e.message,
        },
    }
}

async fn call(job: &mut Job, saved: &mut Saved) -> Result<Completion, GatewayError> {
    let user = tokio::select! {
        biased;
        () = job.stop.raised() => return Err(crate::proxy::canceled("stopped before the turn began")),
        answer = &mut job.user => answer,
    };
    let (user_message_id, sent) = match user {
        Ok(Ok(Some(w))) => (Some(w.id), w.sent),
        Ok(Ok(None)) => (None, None),
        Ok(Err(e)) => return Err(from_object(e)),
        Err(_) => return Err(GatewayError::Internal("the journal is gone".into())),
    };
    let (state, gen, tx) = (&job.state, job.gen, &job.tx);
    let caller = bound::Caller::of(&job.ctx);
    let Some(thread) = bound::thread(state, job.thread_id).await else {
        return Err(refused(
            "chat_thread_not_found",
            super::not_there(job.thread_id, caller.is_device()),
        ));
    };
    if bound::is_admin(&thread) {
        return Err(refused(
            "chat_thread_admin",
            "voice mode is not available in Admin Chat".into(),
        ));
    }
    // A device's thread that left its reach since the bind (the toolset
    // attached, or its admin-tools switch turned off) is gone for it (L3,
    // review W3-1), in the words a deleted one gets; its session is being
    // closed.
    if !caller.sees(&state.snapshot(), &thread) {
        return Err(refused(
            "chat_thread_not_found",
            super::not_there(job.thread_id, true),
        ));
    }
    let plan = match &job.speaking {
        Some(sp) => {
            let plan = bound::plan_speech(state, &thread).await.map_err(|r| {
                let code = refusal_code(&r.code);
                // A code the session has no name for keeps its own in the
                // message (WP8 review NIT 5).
                let message = match code {
                    "speech_unavailable" => format!("{} ({})", r.message, r.code),
                    _ => r.message,
                };
                refused(code, message)
            })?;
            Some((plan, sp))
        }
        None => None,
    };
    // The session's own voice as the thread resolves it now: a profile
    // switch or edit since the last response reaches the session object
    // (personality-profiles design D21).
    let snap = state.snapshot();
    let voice =
        super::reshape::Reshape::of(&bound::voice(&snap, &thread), plan.as_ref().map(|(p, _)| p));
    let _ = tx.send((
        gen,
        Msg::Planned {
            chat: thread.model_alias.clone(),
            tts: plan.as_ref().map(|(p, _)| p.speech.alias.clone()),
            thread: bound::thread_ref(&snap, &thread, &bound::Caller::of(&job.ctx)),
            voice: Box::new(voice),
        },
    ));
    drop(snap);

    // A response that answers the thread's waiting calls decides them
    // first, as the session's principal; the resumed turn runs as the
    // principal that started it (client-apps design §6.4, L13).
    // Its starter's concurrency slot goes with it, unless the session holds
    // that key's already.
    let mut runs_as = bound::Caller::of(&job.ctx);
    let mut slot = None;
    if let Some(verdicts) = job.verdicts.take() {
        let approver = bound::Caller::of(&job.ctx);
        match bound::approve(state, &thread, &verdicts, &approver).await {
            Ok(a) => {
                runs_as = a.starter;
                job.resume = Some(a.resume);
                slot = a.slot;
            }
            Err((code, message)) => return Err(refused(code, message)),
        }
    }
    let (turn_stop, turn_signal) = stop_pair();
    let starter = audio::Starter {
        state,
        thread: &thread,
        voice: plan.as_ref().map(|(p, _)| VoiceTurn {
            hint: super::super::expressive::hint(&p.expressive, job.hint),
        }),
        language: bound::turn_language(&state.snapshot(), &thread, plan.is_some()),
        stop: turn_signal,
        degraded: job.degraded.take(),
        caller: runs_as,
        resume: job.resume.take(),
        slot: std::sync::Mutex::new(slot),
        answer: job.answer,
        sent: std::sync::Mutex::new(sent),
    };
    let mut attempt = audio::Attempt::new(job.audio.take(), gen, tx, saved.journal.clone());
    let (mut frames, mut began) = attempt.first(&starter, user_message_id, &job.stop).await?;

    // The speaker stops the turn through this when its voice fails; the
    // response's stop does too (module doc). Dropping it stops the turn as
    // well, so a text-output turn keeps it until the turn has ended.
    let (stop_chat, chat_stop) = stop_pair();
    let mut stop_chat = Some(stop_chat);
    // And the turn stops the speaker through this when it was superseded or
    // not saved: what it generated is no reply, and is not said on (§8.3).
    let (halt, halted) = stop_pair();
    let (work, queue) = mpsc::unbounded_channel();
    let mut relay = Relay::new(gen, tx, &halt);
    let speech = plan.map(|(mut p, sp)| {
        p.speech.label = job.label.clone();
        p.speech.proto = ClientProto::Realtime;
        p.speech.progress = Some(sp.progress.clone());
        p.speech.ahead = sp.ahead;
        p.speech.longest_pause_ms = sp.longest_pause_ms;
        p.speech.speed = sp.speed;
        p
    });
    let stop = job.stop.clone();
    let chat = async {
        let mut splitter;
        let mut text_sink;
        let sink: &mut dyn DeltaSink = match &speech {
            Some(p) => {
                splitter = Splitter::new(
                    gen,
                    tx,
                    work,
                    chat_stop.clone(),
                    &job.label,
                    Some(p.announce),
                );
                &mut splitter
            }
            None => {
                drop(work);
                text_sink = TextSink { gen, tx };
                &mut text_sink
            }
        };
        let mut stopping = false;
        loop {
            tokio::select! {
                biased;
                () = stop.raised(), if !stopping => {
                    stopping = true;
                    turn_stop.stop();
                    attempt.halted();
                }
                () = chat_stop.raised(), if !stopping => {
                    stopping = true;
                    turn_stop.stop();
                    attempt.halted();
                }
                f = frames.recv() => match f {
                    Some(f) => {
                        for f in attempt.screen(f) {
                            let end = f.event == "stop";
                            relay.frame(f, sink);
                            // A heard response's turn says `done` only once
                            // its user row is written, after the transcript:
                            // its last clause is cut at the end of
                            // generation, so it is synthesized during the
                            // hold (voice-audio-input design §3.2).
                            if end && attempt.heard() {
                                sink.flush();
                            }
                        }
                    }
                    None => match attempt.next(&starter, &stop).await {
                        audio::Next::Again(next) => (frames, began) = next,
                        audio::Next::End(last) => {
                            for f in last {
                                relay.frame(f, sink);
                            }
                            break;
                        }
                    },
                },
            }
        }
        if relay.whole() {
            sink.on_delta(&StreamDelta::Stop(
                relay.stop.clone().unwrap_or(FinishReason::Stop),
            ));
        }
        attempt.ended(relay.whole());
        // The last clause, and the end of the speaker's queue.
        sink.flush();
        relay
    };
    let speaker = async {
        match &speech {
            Some(p) => {
                let speaking = speak(Speaker {
                    state,
                    ctx: &job.ctx,
                    gen,
                    speech: &p.speech,
                    queue,
                    tx,
                    stop: &halted,
                    stop_chat: stop_chat.take().expect("the one speaker takes it"),
                });
                tokio::pin!(speaking);
                // The response's stop is the speaker's too.
                tokio::select! {
                    spoken = &mut speaking => spoken,
                    () = job.stop.raised() => {
                        halt.stop();
                        speaking.await
                    }
                }
            }
            None => {
                drop(queue);
                Ok(())
            }
        }
    };
    let (relay, spoken) = tokio::join!(chat, speaker);
    drop(stop_chat);
    let served = ServedModel {
        alias: thread.model_alias.clone(),
        answered_by: relay.answered_by.clone(),
        voice: None,
    };
    saved.send(relay.saved_id(), began, Some(served));
    if relay.halted {
        // The speaker was stopped for the turn's own error, which is the
        // response's.
        return relay.result(&thread.model_alias, &job.stop);
    }
    spoken.and(relay.result(&thread.model_alias, &job.stop))
}

/// A speech refusal's code, as the response's error says it.
fn refusal_code(code: &str) -> &'static str {
    match code {
        "tts_not_configured" => "tts_not_configured",
        "voice_not_configured" => "voice_not_configured",
        "voice_not_found" => "voice_not_found",
        "instructions_required" => "instructions_required",
        "voice_needs_transcript" => "voice_needs_transcript",
        _ => "speech_unavailable",
    }
}

/// A text-output response's sink: the deltas straight to the core.
struct TextSink<'a> {
    gen: u64,
    tx: &'a responder::Tx,
}

impl DeltaSink for TextSink<'_> {
    fn on_delta(&mut self, d: &StreamDelta) {
        let _ = self.tx.send((self.gen, Msg::Delta(d.clone())));
    }
}

/// The `done` frame's facts.
#[derive(Default)]
struct Done {
    message_id: i64,
    saved: bool,
    aborted: bool,
}

/// The turn's frames, relayed and read.
struct Relay<'a> {
    gen: u64,
    tx: &'a responder::Tx,
    /// Stops the speaker (`call`).
    halt: &'a StopHandle,
    /// The turn was superseded or not saved, and the speaker stopped.
    halted: bool,
    /// Who answered in the chat model's place (its `done` frame).
    answered_by: Option<String>,
    text: String,
    usage: Usage,
    /// A `usage` frame came: the `done` frame's counts are not merged in.
    usage_framed: bool,
    stop: Option<FinishReason>,
    /// The turn's last `error` frame: its code, its message, and the
    /// gateway error itself when the frame was one's (`TurnFrame::error`).
    error: Option<(Option<String>, String, Option<GatewayError>)>,
    done: Option<Done>,
    marked: bool,
    /// A `reasoning` frame came: its first is the reasoning's mark.
    reasoned: bool,
}

impl<'a> Relay<'a> {
    fn new(gen: u64, tx: &'a responder::Tx, halt: &'a StopHandle) -> Self {
        Self {
            gen,
            tx,
            halt,
            halted: false,
            answered_by: None,
            text: String::new(),
            usage: Usage::default(),
            usage_framed: false,
            stop: None,
            error: None,
            done: None,
            marked: false,
            reasoned: false,
        }
    }

    fn send(&self, m: Msg) {
        let _ = self.tx.send((self.gen, m));
    }

    /// One frame (module doc).
    fn frame(&mut self, f: TurnFrame, sink: &mut dyn DeltaSink) {
        let data: Value = serde_json::from_str(&f.data).unwrap_or(Value::Null);
        self.send(Msg::ChatFrame {
            event: f.event,
            data: data.clone(),
        });
        match f.event {
            "delta" => {
                let t = data["text"].as_str().unwrap_or_default().to_string();
                self.text.push_str(&t);
                self.marked = true;
                sink.on_delta(&StreamDelta::TextDelta(t));
            }
            // Reasoning is never spoken (chat-voice §8.5): relayed above for
            // the bubble's block, timed, and kept away from the sink — the
            // clause cutting, the speakable pass and the heard table see the
            // reply's text alone, whatever the model reasons at.
            "reasoning" => {
                if !std::mem::replace(&mut self.reasoned, true) {
                    self.send(Msg::Mark(Mark::Reasoning));
                }
            }
            "tool" if data["event"] == "start" => {
                if !std::mem::replace(&mut self.marked, true) {
                    self.send(Msg::Mark(Mark::FirstToken));
                }
                // The preamble is spoken before the tool runs (§7.5).
                sink.flush();
            }
            "usage" => {
                let u = Usage {
                    prompt_tokens: data["prompt_tokens"].as_u64(),
                    completion_tokens: data["completion_tokens"].as_u64(),
                    ..Default::default()
                };
                self.usage.merge(&u);
                self.usage_framed = true;
                sink.on_delta(&StreamDelta::Usage(u));
            }
            "stop" => self.stop = data["reason"].as_str().map(finish_reason),
            "error" => {
                let code = data["code"].as_str().map(str::to_string);
                // Another turn took the thread, or the reply cannot be
                // saved: nothing of it is said on (§8.3, WP8 review m1).
                if matches!(code.as_deref(), Some("superseded" | "not_saved")) {
                    self.halted = true;
                    self.halt.stop();
                }
                self.error = Some((
                    code,
                    data["message"].as_str().unwrap_or_default().to_string(),
                    f.failure,
                ))
            }
            "done" => {
                self.answered_by = data["answered_by"].as_str().map(str::to_string);
                // A tool thread says its counts in `done` alone (WP11 server
                // review m1): they are the response's usage then.
                if !self.usage_framed {
                    let u = Usage {
                        prompt_tokens: data["prompt_tokens"].as_u64(),
                        completion_tokens: data["completion_tokens"].as_u64(),
                        ..Default::default()
                    };
                    if u.prompt_tokens.is_some() || u.completion_tokens.is_some() {
                        self.usage.merge(&u);
                        sink.on_delta(&StreamDelta::Usage(u));
                    }
                }
                self.done = Some(Done {
                    message_id: data["message_id"].as_i64().unwrap_or(0),
                    saved: data["saved"].as_bool().unwrap_or(false),
                    aborted: data["aborted"].as_bool().unwrap_or(true),
                })
            }
            _ => {}
        }
    }

    /// The turn ended as generated: saved, not stopped, no error.
    fn whole(&self) -> bool {
        self.error.is_none() && self.done.as_ref().is_some_and(|d| !d.aborted)
    }

    /// The reply the turn saved, if it saved one.
    fn saved_id(&self) -> Option<i64> {
        self.done
            .as_ref()
            // A temporary thread's ids count down from -1.
            .filter(|d| d.saved && d.message_id != 0)
            .map(|d| d.message_id)
    }

    /// The response's result (module doc).
    fn result(self, model: &str, stop: &StopSignal) -> Result<Completion, GatewayError> {
        if self.whole() {
            return Ok(Completion {
                content: vec![ContentPart::text(self.text)],
                reasoning: String::new(),
                finish_reason: self.stop.unwrap_or(FinishReason::Stop),
                usage: self.usage,
                model: model.to_string(),
                timings: None,
            });
        }
        if stop.is_raised() {
            return Err(crate::proxy::canceled("stopped by the caller"));
        }
        Err(match self.error {
            Some((Some(code), message, _)) if code == "superseded" => {
                refused("superseded", message)
            }
            Some((Some(code), message, _)) if code == "not_saved" => refused("not_saved", message),
            // The turn's own refusal or failure, as the gateway error it
            // was: the client gets the code, type and message the stock
            // session sends for it — `gpu_hold`, `context_length_exceeded`,
            // `vram_queue_timeout` (WP11 server review M1).
            Some((_, _, Some(e))) => e,
            // A frame with no gateway error behind it: a stream that broke
            // mid-way, a thread with no usable tool.
            Some((_, message, None)) => GatewayError::Upstream {
                status: 502,
                provider_type: None,
                message,
            },
            None if self.done.is_none() => {
                GatewayError::Internal("the chat turn ended without its done frame".into())
            }
            None => crate::proxy::canceled("the turn was stopped"),
        })
    }
}

/// A `stop` frame's reason (`FinishReason::to_openai`'s words).
fn finish_reason(r: &str) -> FinishReason {
    match r {
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        "tool_calls" => FinishReason::ToolUse,
        "stop" => FinishReason::Stop,
        other => FinishReason::Other(other.to_string()),
    }
}
