//! One read-aloud (chat-voice design §6.3, §6.4): realtime's speech pipeline
//! without a writer, its output as the Chat's frames.
//!
//! The task owns realtime's clause splitter (`responder::Splitter`, with the
//! Chat's announcements of skipped code and tables) and its speaker
//! (`responder::speak`): text comes in as [`Feed`] — a stored reply at once,
//! a streaming one delta by delta, with a [`Feed::Flush`] at a tool call —
//! and each synthesized batch of clauses goes out as it is made, one frame
//! per clause (`responder::speak` joins the sentences of a paragraph into
//! one TTS request where it can wait for them). The speaker may wait for
//! more text while the page still has audio to play, never once the text has
//! ended; synthesis itself waits for no playback: the page plays what it is
//! sent, and the TTS claim ends with the last clause (`out` keeps the audio
//! raw until the page takes it, and says a page that falls behind playback).
//!
//! The frames, in order:
//!
//! | event | data |
//! |---|---|
//! | `state` | §4.3's shape, stage `tts`: `loading` before an opening that has to start or load the model, then `ready` with its time (or `fallback`, `held`, `failed`) — one `loading` and one end, whether the caller's warm or the opening says them |
//! | `voice` | `{tts, voice, tts_answered_by}` once the route is open |
//! | `speech` | `{seq, text, pcm}`: a clause as said, its audio base64 PCM16-LE 24 kHz mono; `seq` counts from 0 |
//! | `speech_error` | `{code, message}`: the voice failed; it ends the speech |
//! | `speech_done` | `{chars, audio_ms, first_audio_ms, tts, tts_answered_by, stopped}`: it ended — whole, or stopped (`speech/stop`, or the reader gone) |
//!
//! A clause the TTS had nothing to say for (only inline tags) sends no
//! `speech` frame. `first_audio_ms` is from the caller's start (the speak
//! request, or the turn's) to the first `speech` frame. The response's one
//! TTS row is the speaker's (`proxy::synthesize`), labelled as the thread's
//! turns are.
//!
//! **One `loading` and one end per stage** (WP4 review m3). A turn read as
//! it streams warms the TTS beside the prefill, and the route's opening at
//! the first clause may find it still loading. Whoever says `loading`
//! first owns it: the warm's is ended by the warm — or by the opening, if
//! the route opens first, with the time since that `loading` — and the
//! opening says no second one. A warm that ended without loading (`held`,
//! `skipped`) leaves the load to the opening, which says its own. The
//! warm's frames after the route opened are not said.

use std::time::Instant;

use lmgw_api_types::chat_frames as frames;
use tokio::sync::mpsc;

use crate::agent::DeltaSink;
use crate::error::GatewayError;
use crate::ir::StreamDelta;
use crate::proxy::{is_canceled, stop_pair, RequestCtx, StopSignal};
use crate::realtime::audio::resample::INPUT_RATE;
use crate::realtime::responder::{speak, Msg, Speaker, Splitter, TtsEvent};
use crate::realtime::warm::{ModelState, WarmOutcome};
use crate::state::SharedState;

use super::super::super::chat_turn::TurnFrame;
use super::out::Out;
use super::plan::{Plan, Refusal};

/// The text a read-aloud is fed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Feed {
    /// More of the reply, as the model wrote it.
    Text(String),
    /// A tool call starts: the clause in progress is said now (§7.5).
    Flush,
}

/// What one read-aloud speaks with, and when its caller started.
pub(crate) struct Run {
    pub plan: Plan,
    /// Raised by `speech/stop`. The reader going away stops it too.
    pub stop: StopSignal,
    /// The caller's start: `first_audio_ms` counts from it.
    pub started: Instant,
    /// Whose request it speaks for: its TTS call is checked against this
    /// context's key and charged to it — a device's (client-apps design L4),
    /// or the in-process default the Chat has always used.
    pub ctx: RequestCtx,
}

/// Speak what `feed` brings until it closes (module doc); the frames go to
/// `out`, and `states` (a warm's) are passed on as they come. Returns once
/// the speech is over: the feed closed and everything said, or the speech
/// stopped or failed first (the feed is dropped then).
pub(crate) async fn run(
    state: SharedState,
    run: Run,
    mut feed: mpsc::UnboundedReceiver<Feed>,
    states: Option<mpsc::UnboundedReceiver<ModelState>>,
    out: Out,
) {
    let Run {
        plan,
        stop: registered,
        started,
        ctx,
    } = run;
    let Plan {
        speech, announce, ..
    } = plan;
    let (tx, mut msgs) = mpsc::unbounded_channel::<(u64, Msg)>();
    let (work, queue) = mpsc::unbounded_channel();
    // The speaker stops the chat stream it reads when its voice fails. Here
    // that is nobody's: a failed voice leaves the text alone (§6.4).
    let (stop_chat, chat_stop) = stop_pair();
    // The speaker's stop: `speech/stop`, or the reader gone (§6.4's
    // closure) — the page going away ends the synthesis at its next await.
    let (raise, stop) = stop_pair();
    let feeding = async {
        let mut splitter = Splitter::new(0, &tx, work, chat_stop, &speech.label, Some(announce));
        while let Some(f) = feed.recv().await {
            match f {
                Feed::Text(t) => splitter.on_delta(&StreamDelta::TextDelta(t)),
                Feed::Flush => splitter.flush(),
            }
        }
        // The last clause, and the end of the speaker's queue.
        splitter.finish();
    };
    let speaking = async {
        let speaker = speak(Speaker {
            state: &state,
            ctx: &ctx,
            gen: 0,
            speech: &speech,
            queue,
            tx: &tx,
            stop: &stop,
            stop_chat,
        });
        tokio::pin!(speaker);
        let mut frames = Frames::new(&state, &out, &speech.alias, &speech.label, started);
        let mut states = states;
        let result = loop {
            tokio::select! {
                biased;
                Some((_, m)) = msgs.recv() => frames.msg(m),
                s = next_state(&mut states) => match s {
                    Some(s) => frames.warm(s),
                    None => states = None,
                },
                r = &mut speaker => break r,
            }
        };
        // Everything the speaker said is in the channel by now.
        while let Ok((_, m)) = msgs.try_recv() {
            frames.msg(m);
        }
        // A warm still loading is ended by the warm (module doc), unless
        // the speech was stopped: then nobody waits for it.
        while frames.warm_loading() {
            tokio::select! {
                biased;
                () = stop.raised() => break,
                s = next_state(&mut states) => match s {
                    Some(s) => frames.warm(s),
                    None => break,
                },
            }
        }
        frames.end(result);
    };
    // The speech over — whole, stopped, or its voice failed — what is still
    // fed is nobody's: the feed is dropped with it, so the text's tee stops
    // feeding, and the read-aloud's task (with its registration) ends now,
    // not when the text does.
    let work = async {
        tokio::pin!(feeding);
        tokio::pin!(speaking);
        tokio::select! {
            () = &mut feeding => speaking.await,
            () = &mut speaking => {}
        }
    };
    tokio::pin!(work);
    let watch = async {
        tokio::select! {
            () = registered.raised() => {}
            () = out.closed() => {}
        }
        raise.stop();
    };
    tokio::select! {
        () = &mut work => return,
        () = watch => {}
    }
    // Stopped: the speaker ends at its next await and writes its row.
    work.await;
}

/// A refusal before anything was spoken, as its `speech_error` frame.
pub(crate) fn refused(r: &Refusal) -> TurnFrame {
    TurnFrame::of(
        "speech_error",
        &frames::SpeechError {
            code: r.code.clone(),
            message: r.message.clone(),
        },
    )
}

/// The next frame of a warm, if there is one; pending for good once it
/// ended (the caller drops it on `None`).
async fn next_state(
    states: &mut Option<mpsc::UnboundedReceiver<ModelState>>,
) -> Option<ModelState> {
    match states {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// An open `loading` frame: when it went out, and whether the warm said it.
#[derive(Clone, Copy)]
struct Loading {
    at: Instant,
    warm: bool,
}

/// The frames of one read-aloud, and what `speech_done` sums up.
struct Frames<'a> {
    state: &'a SharedState,
    out: &'a Out,
    alias: &'a str,
    label: &'a str,
    started: Instant,
    seq: u64,
    chars: usize,
    samples: u64,
    first_audio_ms: Option<u64>,
    answered_by: Option<String>,
    /// The `loading` said and not yet ended (module doc).
    loading: Option<Loading>,
    /// The route's opening has begun.
    opening: bool,
    /// A reader behind playback was said (`out`).
    said_behind: bool,
}

impl<'a> Frames<'a> {
    fn new(
        state: &'a SharedState,
        out: &'a Out,
        alias: &'a str,
        label: &'a str,
        started: Instant,
    ) -> Self {
        Self {
            state,
            out,
            alias,
            label,
            started,
            seq: 0,
            chars: 0,
            samples: 0,
            first_audio_ms: None,
            answered_by: None,
            loading: None,
            opening: false,
            said_behind: false,
        }
    }

    fn send(&self, event: &'static str, data: &impl serde::Serialize) {
        self.out.frame(TurnFrame::of(event, data));
    }

    /// A frame of the caller's warm (module doc). A `ready` with no time
    /// (nothing had to load) says nothing, as a turn's own admission does
    /// not.
    fn warm(&mut self, s: ModelState) {
        if s.state == "ready" && s.ms.is_none() {
            return;
        }
        if s.state == "loading" {
            if self.loading.is_none() && !self.opening {
                self.send("state", &s);
                self.loading = Some(Loading {
                    at: Instant::now(),
                    warm: true,
                });
            }
            return;
        }
        match self.loading {
            // Its own `loading`, ended.
            Some(Loading { warm: true, .. }) => {
                self.send("state", &s);
                self.loading = None;
            }
            // What it found before the route opened: `held`, `skipped`.
            None if !self.opening => self.send("state", &s),
            _ => {}
        }
    }

    /// The warm's `loading` is open, for it to end.
    fn warm_loading(&self) -> bool {
        matches!(self.loading, Some(Loading { warm: true, .. }))
    }

    fn msg(&mut self, m: Msg) {
        match m {
            Msg::Tts(TtsEvent::Opening) => {
                self.opening = true;
                if self.loading.is_none() && cold(self.state, self.alias) {
                    self.send("state", &ModelState::loading("tts", self.alias));
                    self.loading = Some(Loading {
                        at: Instant::now(),
                        warm: false,
                    });
                }
            }
            Msg::Tts(TtsEvent::Opened { answered_by, voice }) => {
                if let Some(l) = self.loading.take() {
                    let outcome = match &answered_by {
                        Some(a) => WarmOutcome::Fallback {
                            answered_by: a.clone(),
                        },
                        None => WarmOutcome::Ready {
                            ms: Some(l.at.elapsed().as_millis() as u64),
                        },
                    };
                    self.outcome(&outcome);
                }
                let opened = frames::VoiceFrame {
                    tts: self.alias.to_string(),
                    voice,
                    tts_answered_by: answered_by.clone(),
                };
                self.send("voice", &opened);
                self.answered_by = answered_by;
            }
            Msg::Clause { text, pcm, .. } if !pcm.is_empty() => {
                self.first_audio_ms
                    .get_or_insert_with(|| self.started.elapsed().as_millis() as u64);
                self.chars += text.chars().count();
                self.samples += (pcm.len() / 2) as u64;
                self.out.speech(self.seq, text, pcm);
                self.seq += 1;
                self.behind();
            }
            _ => {}
        }
    }

    /// Say once that the page reads slower than it plays (`out`).
    fn behind(&mut self) {
        if self.said_behind {
            return;
        }
        if let Some(waiting) = self.out.behind() {
            self.said_behind = true;
            tracing::warn!(
                "{}: the page reads its speech slower than it plays: {waiting} ms of audio wait \
                 for it (kept whole, nothing is dropped)",
                self.label
            );
        }
    }

    fn outcome(&self, outcome: &WarmOutcome) {
        if let Some(s) = ModelState::of("tts", self.alias, outcome) {
            self.send("state", &s);
        }
    }

    /// The last frame: `speech_done`, or `speech_error` for a voice that
    /// failed (after the `state` its opening owes, when it was loading).
    fn end(mut self, result: Result<(), GatewayError>) {
        self.behind();
        let (peak, waiting) = self.out.waited();
        tracing::debug!(
            "{}: read-aloud over: {} clauses, {} ms of audio; at most {peak} ms waited for the \
             page, {waiting} ms wait now",
            self.label,
            self.seq,
            self.samples * 1000 / u64::from(INPUT_RATE),
        );
        let stopped = match result {
            Ok(()) => false,
            Err(e) if is_canceled(&e) => true,
            Err(e) => {
                if self.loading.take().is_some() {
                    self.outcome(&WarmOutcome::refused(&e));
                }
                let error = frames::SpeechError {
                    code: e.kind().to_string(),
                    message: e.to_string(),
                };
                self.send("speech_error", &error);
                return;
            }
        };
        let done = frames::SpeechDone {
            chars: self.chars as u64,
            audio_ms: self.samples * 1000 / u64::from(INPUT_RATE),
            first_audio_ms: self.first_audio_ms,
            tts: self.alias.to_string(),
            tts_answered_by: self.answered_by.clone(),
            stopped,
        };
        self.send("speech_done", &done);
    }
}

/// Whether opening `alias` for speech has to start or load its model: a
/// local model on this machine, not resident (an audio row: up and loaded),
/// and not held off it — a route the GPU hold or a benchmark swaps or
/// refuses says nothing here, as a chat turn's does not (`turn_state`); its
/// `voice` frame or its error names what happened. A candidate alias picks
/// at admission, and a cloud alias loads nothing.
pub(crate) fn cold(state: &SharedState, alias: &str) -> bool {
    let snap = state.snapshot();
    if snap.candidate_alias(alias).is_some() {
        return false;
    }
    let Ok(route) = snap.resolve(alias) else {
        return false;
    };
    let Some(t) = crate::vram::classify(&route) else {
        return false;
    };
    if snap
        .gpu_block_at(snap.placement(t.class, &t.model_id))
        .is_some()
    {
        return false;
    }
    !crate::realtime::warm::resident(state, &snap, t.class, &t.model_id)
}
