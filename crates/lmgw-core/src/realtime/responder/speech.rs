//! A speaking response's voice (realtime design §8.1, §8.2, §9.1, §10.3,
//! §11): the chat stream cut into clauses, each synthesized in order.
//!
//! [`Splitter`] is the chat stream's sink. Text goes through the clause
//! splitter (`ClauseAggregator`) and the speakable pass as it arrives;
//! every other delta the listener gets — a tool call's start and arguments,
//! the end of generation — is queued **behind the clauses before it**, so
//! the spoken preamble's item comes first and the call's item follows, as in
//! text mode. A call's start, and the end of the stream, first flush the
//! text still buffered as a clause of its own. Usage goes straight to the
//! core; its order does not matter. Reasoning is never spoken (§7.6).
//!
//! **A skipped block can be announced** ([`Announce`], chat-voice design
//! §6.2): fenced code and tables are never spoken, and a stock session
//! skips them silently. The Chat's callers pass an `Announce`, and the
//! splitter says one short clause where such a block begins ("Code block,
//! rust.", "Table."). Its [`Written`] is marked an announcement and carries
//! only what was written before the block, so the clause after it carries
//! the block: the model's history, heard whole, is what it would be
//! without it, and the heard table never writes an announcement's words
//! and counts its block heard when it was heard whole (`heard::written`).
//!
//! **Every clause carries what the model wrote for it** ([`Written`]): the
//! text the voice left out since the clause before (fenced code, markers, a
//! clause with nothing to say) and the clause as written. What is left out
//! after the last clause goes to the core as [`Msg::Unspoken`] at a flush.
//! The model's history is that text, up to what was heard (§7.2, B2 review
//! 2); the transcript stays what was said.
//!
//! **Each clause has two texts** ([`Spoken`], WP10 D9): the TTS gets its
//! inline tags, and the transcript — the delta, the item, the heard table —
//! is the clause without them; the history keeps them, as written, so the
//! model stays in its own style. An inline list's number opening a clause
//! goes to the TTS without its dot, which a voice read as a sentence end,
//! and stays in the transcript with it (R5 F2). **A clause of nothing but
//! tags** (D10) is not sent: its tags go before the next clause's words,
//! and its text to that clause's `before`. One at the end of the stream is
//! not voiced at all — an engine needs words around a sound — and the
//! history keeps it as unspoken text.
//!
//! **A clause carries its delivery cue** (`cue`, WP9b): the tags that open
//! it, and only those — a clause is a whole sentence or line, so the cue
//! covers exactly what it opens. The splitter does not know the route; the
//! route that answers decides whether it takes the cue
//! (`crate::audio::cues`), and on one that does the leading tags are the
//! clause's instructions instead of its text. The transcript never had
//! them, and the history keeps them as written.
//!
//! [`speak`] works through that queue in order:
//! - the first clause runs the key's per-call policy check for the TTS alias
//!   (§10.3) and opens its route **once** (`proxy::synthesize`): every
//!   clause of the response goes over that one route and hold (§9.1). At the
//!   first clause rather than at `response.created`: §9.1's "hold at the
//!   point of use" — a response that only calls a tool neither holds a
//!   voice it never uses nor fails on one it cannot get, and the warm at
//!   connect and at `speech_started` has started the model by then. The
//!   voice is settled there too, on that route (`settle`): a provisional one
//!   checked against the model's list, or one of the fallback's own when a
//!   fallback answers;
//! - **clauses are joined into one request** where they can wait (`batch`,
//!   2026-10-05): the first goes alone and at once; later sentences of one
//!   paragraph are gathered while the listener still has audio to hear, so
//!   the engine reads them as one text. Each clause of a batch is still one
//!   to the core, with its share of the audio;
//! - each batch is synthesized with the response's speech instructions and
//!   the session's seed ([`ClauseSpeech`], WP10), its WAV parsed — audio.cpp
//!   answers with a WAV even for `pcm` — resampled to 24 kHz when its rate
//!   differs, its silences cut to the longest pause (`audio::pauses`) and
//!   faded in and out (§8.2), on the blocking pool, and handed to the core
//!   whole. What shaping dropped or stripped for the first clause is logged
//!   once per response (D12);
//! - **back-pressure** (`room`): the next clause waits while more than
//!   `synthesis_ahead_s` of the response's audio is queued and not yet sent
//!   (WP3 review M3). Synthesis runs ~30× real time, so without it a
//!   looping model or a huge answer would hold its whole audio in memory.
//!   Nothing is dropped; the price is that the TTS hold spans all but that
//!   much of a long answer's playback — unless the GPU hold comes on or an
//!   admission waits for room, which lift the bound for the rest of the
//!   response (package B review 1). A client that stops reading keeps the
//!   bound: the model is let go while it does not read, and admitted again
//!   when it does (B2 review 3);
//! - **no writer, no bound** (chat-voice design §6.1): the Chat's read-aloud
//!   has no paced writer to wait for ([`Speech::progress`] `None`, `ahead`
//!   `None`). Nothing holds its synthesis back, and its TTS claim ends with
//!   the last clause; what it made is held by the reader's stream, bounded by
//!   the reply itself — the chat model's output maximum. It gathers batches
//!   all the same, by its own estimate of the page's playback (the first
//!   audio out, plus the audio made so far: `batch`), which needs no writer;
//! - the route's opening is reported ([`Msg::Tts`]): a caller that shows
//!   model state (the Chat's `state` and `voice` frames) reads it, and a
//!   realtime session ignores it;
//! - after the last clause the hold is dropped and the response's one TTS
//!   row is written (§11) — while its audio is still playing. Its label is
//!   the caller's ([`Speech::proto`]): `realtime` for a session, the Chat's
//!   for read-aloud;
//! - a cancel (the response's stop) ends it at its next await, and a failed
//!   clause fails the response: either way it stops the chat stream too, and
//!   the row records what was synthesized. A chat stream that fails is not
//!   the speaker's business: what it generated is said, and the response
//!   then fails as a text one would.

use std::time::Duration;

use tokio::sync::mpsc;

use super::super::audio::pauses::limit_pauses;
use super::super::audio::pcm::{f32_to_pcm16, fade_edges, parse_wav, pcm16_to_le_bytes};
use super::super::audio::resample::{resample, INPUT_RATE};
use super::super::clauses::{Announce, Block, ClauseAggregator, Item, Placed, Spoken};
use super::super::expressive;
use super::super::heard::Written;
use super::super::protocol::Voice;
use super::super::voice::{SpeakVoice, VoiceFacts};
use super::super::writer::Progress;
use super::{Mark, Msg, TtsEvent, Tx};
use crate::agent::DeltaSink;
use crate::error::GatewayError;
use crate::ingress::ClientProto;
use crate::ir::StreamDelta;
use crate::proxy::synthesize::{self, ClauseSpeech, SessionSeed, Synthesis};
use crate::proxy::{RequestCtx, StopHandle, StopSignal};
use crate::state::SharedState;
use crate::telemetry::RequestClass;

/// The fade at each clause's start and end, in milliseconds (§8.2, §23
/// L11): long enough to take the click off a clause that starts at full
/// amplitude, far too short to hear as a fade.
const CLAUSE_FADE_MS: u32 = 5;

/// The sample rates a TTS answer may have: anything audible that a WAV
/// header can honestly claim. A header outside it is a broken answer, and
/// resampling from it would turn a small body into a huge clip.
const RATES: std::ops::RangeInclusive<u32> = 1_000..=384_000;

/// How a response speaks.
pub(crate) struct Speech {
    /// What the log lines start with: `realtime <session id>`, or the
    /// Chat's `chat thread <id>`.
    pub label: String,
    /// The label of the response's TTS row: `realtime` for a session's
    /// response, the Chat's in-process label for read-aloud (chat-voice
    /// design §6.1). The key's per-call check is made under it too.
    pub proto: ClientProto,
    /// The TTS alias.
    pub alias: String,
    /// The voice the response asked for — the session's, or its
    /// `response.create`'s own — which a fallback resolves again (§9.2).
    pub requested: Voice,
    /// The voice resolved for `alias`, verified or provisional (§5.3).
    pub voice: SpeakVoice,
    /// What the session knows `alias` speaks.
    pub facts: VoiceFacts,
    /// `audio.output.speed`.
    pub speed: Option<f64>,
    /// The language: a session's, a hint for the TTS row (audio-class gap
    /// 7); the Chat's configured one, a request (chat-voice design §2.1).
    pub language: Option<crate::audio::language::SpeechLanguage>,
    /// The speech instructions every clause carries (WP10 D4); `None` sends
    /// none.
    pub instructions: Option<String>,
    /// The session's TTS reads no instructions, and the resolution said so:
    /// its shaping dropping them is no news (WP10 D12).
    pub dropped: bool,
    /// The session's seed, for a row that reads one (WP10 D6).
    pub seed: Option<SessionSeed>,
    /// `synthesis_ahead_s`, in samples at 24 kHz: how much audio may be
    /// queued and not yet sent before the next clause waits; `None` for no
    /// bound (§8.2).
    pub ahead: Option<u64>,
    /// What the writer has sent of each generation's audio, and when the
    /// client runs dry; `None` with no writer (the Chat's read-aloud): no
    /// bound and no stall rule (module doc).
    pub progress: Option<Progress>,
    /// `longest_pause_ms`: no silence in the audio lasts longer, at a join
    /// between two requests or inside one (`audio::pauses`); 0 keeps the
    /// engine's.
    pub longest_pause_ms: u32,
}

/// What the stream hands the speaker, in stream order.
pub(crate) enum Work {
    /// A clause to say (module doc): what the TTS gets, inline tags kept;
    /// what was said, the transcript; and what the model wrote for it.
    Clause {
        tts: String,
        said: String,
        written: Written,
        /// Its delivery cue (`cue`, WP9b).
        cue: Option<String>,
    },
    /// What the model wrote after the last clause and nothing says, for
    /// the core after the clauses before it.
    Unspoken(String),
    /// A delta for the core, after the clauses before it.
    Pass(StreamDelta),
    /// A server-side call's report for the core, after the clauses and the
    /// deltas before it: the call runs at once, and its report must not
    /// overtake its own item (`tools`, realtime-server-tools §2.4).
    Report(Msg),
    /// The text stopped — a tool call, a flush, the stream's end: no clause
    /// after it joins one before it, and nothing is waited for (`batch`).
    Break,
}

/// The chat stream's sink for a speaking response (module doc).
pub(crate) struct Splitter<'a> {
    gen: u64,
    tx: &'a Tx,
    work: mpsc::UnboundedSender<Work>,
    clauses: ClauseAggregator,
    /// Every text delta of the stream, as the model wrote it — what the
    /// clauses' places point into.
    raw: String,
    /// How much of `raw` has gone to the speaker with a clause.
    handed: usize,
    /// The tags of clauses that were nothing but tags, for the next clause's
    /// words (WP10 D10).
    carry: String,
    /// The chat's stop: raised by the speaker (module doc).
    stop: StopSignal,
    /// The timing line's moments already reported (§11, [`Mark`]).
    marked: (bool, bool),
    /// What the log lines start with ([`Speech::label`]).
    label: &'a str,
    /// What is said where a skipped block begins; `None` skips it silently
    /// (module doc).
    announce: Option<Announce>,
}

impl<'a> Splitter<'a> {
    /// `label`: what the log lines start with; `announce`: the Chat's
    /// announcements of skipped blocks, `None` for a stock session.
    pub fn new(
        gen: u64,
        tx: &'a Tx,
        work: mpsc::UnboundedSender<Work>,
        stop: StopSignal,
        label: &'a str,
        announce: Option<Announce>,
    ) -> Self {
        Self {
            gen,
            tx,
            work,
            clauses: ClauseAggregator::new(),
            raw: String::new(),
            handed: 0,
            carry: String::new(),
            stop,
            marked: (false, false),
            label,
            announce,
        }
    }

    /// What the splitter cut, in stream order: a clause is said, a skipped
    /// block announced when the caller asked for that.
    fn hand(&mut self, items: Vec<Item>) {
        for item in items {
            match item {
                Item::Clause(c) => self.say(&c),
                Item::Block(b, at) => self.announce(&b, at),
            }
        }
    }

    /// One clause where `block` begins, at `at` in the stream (module doc):
    /// nothing the model wrote but what came before the block — the block's
    /// text is the clause after it's — and no cue.
    fn announce(&mut self, block: &Block, at: usize) {
        let Some(a) = self.announce else {
            return;
        };
        let text = a.say(block);
        if !std::mem::replace(&mut self.marked.1, true) {
            let _ = self.tx.send((self.gen, Msg::Mark(Mark::FirstClause)));
        }
        let at = at.clamp(self.handed, self.raw.len());
        let written = Written::announcing(&self.raw[self.handed..at]);
        self.handed = at;
        let _ = self.work.send(Work::Clause {
            tts: text.clone(),
            said: text,
            written,
            cue: None,
        });
    }

    /// Say `clause`, with what the model wrote since the clause before. A
    /// clause with nothing to say leaves its text to the next one; one of
    /// nothing but tags its tags too (module doc).
    fn say(&mut self, clause: &Placed) {
        let spoken = Spoken::of(clause);
        if spoken.tts.is_empty() {
            return;
        }
        if spoken.only_tags() {
            if !self.carry.is_empty() {
                self.carry.push(' ');
            }
            self.carry.push_str(&spoken.tags());
            return;
        }
        let spoken = match std::mem::take(&mut self.carry) {
            carry if carry.is_empty() => spoken,
            carry => Spoken::after(&carry, clause),
        };
        if !std::mem::replace(&mut self.marked.1, true) {
            let _ = self.tx.send((self.gen, Msg::Mark(Mark::FirstClause)));
        }
        // A clause whose text began before a block announced inside it (a
        // line with no words, then the block) starts where the announcement
        // left off: every byte is one clause's, once.
        let start = clause.start.max(self.handed);
        let end = clause.end.max(start);
        let written = Written::said(&self.raw[self.handed..start], &self.raw[start..end]);
        self.handed = end;
        let cue = cue::of(&spoken.tts);
        let _ = self.work.send(Work::Clause {
            tts: spoken.tts,
            said: spoken.said,
            written,
            cue,
        });
    }

    /// The stream is over, or a call starts: what is buffered is a clause,
    /// and what nothing says after it goes to the core as it is.
    pub fn finish(&mut self) {
        if let Some(unspoken) = self.clauses.unclosed_fence() {
            // The model never closed it: CommonMark runs it to the end, and
            // code is not spoken — but not silently either.
            tracing::info!(
                "{}: the answer ended inside a code fence it never closed; its {unspoken} bytes \
                 of code were not spoken (§8.1)",
                self.label
            );
        }
        let rest = self.clauses.flush_items();
        self.hand(rest);
        let carry = std::mem::take(&mut self.carry);
        if !carry.is_empty() {
            // A sound needs words after it: the history keeps it, unvoiced.
            tracing::debug!(
                "{}: {carry} at the end of the answer has no words to go with, and is not voiced",
                self.label
            );
        }
        debug_assert_eq!(self.clauses.taken(), self.raw.len());
        if self.handed < self.raw.len() {
            let rest = self.raw[self.handed..].to_string();
            self.handed = self.raw.len();
            let _ = self.work.send(Work::Unspoken(rest));
        }
        let _ = self.work.send(Work::Break);
    }

    /// The stream is over and every clause handed: once the speaker has
    /// said them, the core hears so ([`Msg::SpeakerDone`],
    /// realtime-server-tools §2.5). A speaker that is gone says nothing
    /// more anyway.
    pub(super) fn said_all(&self) {
        let _ = self.work.send(Work::Report(Msg::SpeakerDone));
    }

    /// Where the response's server-side calls report: behind everything
    /// handed to the speaker so far (`tools`).
    pub(super) fn report(&self) -> super::tools::Report<'a> {
        super::tools::Report::queued(self.gen, self.tx, self.work.clone())
    }
}

impl DeltaSink for Splitter<'_> {
    fn on_delta(&mut self, d: &StreamDelta) {
        let first = matches!(
            d,
            StreamDelta::TextDelta(_) | StreamDelta::ToolCallStart { .. }
        );
        if first && !std::mem::replace(&mut self.marked.0, true) {
            let _ = self.tx.send((self.gen, Msg::Mark(Mark::FirstToken)));
        }
        match d {
            StreamDelta::TextDelta(t) => {
                self.raw.push_str(t);
                let items = self.clauses.push_items(t);
                self.hand(items);
            }
            StreamDelta::ToolCallStart { .. }
            | StreamDelta::ToolCallArgsDelta { .. }
            | StreamDelta::Stop(_) => {
                self.finish();
                let _ = self.work.send(Work::Pass(d.clone()));
            }
            StreamDelta::Usage(_) => {
                let _ = self.tx.send((self.gen, Msg::Delta(d.clone())));
            }
            _ => {}
        }
    }

    fn stop(&self) -> Option<StopSignal> {
        Some(self.stop.clone())
    }

    /// The clause in progress is closed and said, as at a tool call.
    fn flush(&mut self) {
        self.finish();
    }
}

/// What [`speak`] works with.
pub(crate) struct Speaker<'a> {
    pub state: &'a SharedState,
    pub ctx: &'a RequestCtx,
    pub gen: u64,
    pub speech: &'a Speech,
    pub queue: mpsc::UnboundedReceiver<Work>,
    pub tx: &'a Tx,
    /// The response's stop.
    pub stop: &'a StopSignal,
    /// Stops the chat stream; dropped (which stops it too) when the speaker
    /// is done — by then the stream has ended, or must.
    pub stop_chat: StopHandle,
}

/// Say the queue's clauses in order (module doc). `Err`: why the voice
/// failed — the response's error.
pub(crate) async fn speak(mut s: Speaker<'_>) -> Result<(), GatewayError> {
    let mut synthesis: Option<Synthesis> = None;
    // The `voice` sent, settled at the first clause.
    let mut voice: Option<String> = None;
    let mut samples = 0u64;
    // What shaping lost is said for the first clause only (WP10 D12).
    let mut reported = false;
    let mut room = Room::new(s.speech.progress.clone(), s.speech.ahead);
    // The listener's clock and the synthesis it has seen (`batch`); an item
    // that ended a batch, to take next.
    let mut pace = Pace::default();
    let mut pending: Option<Work> = None;
    let (gen, tx) = (s.gen, s.tx);
    let route_event = |e: TtsEvent| {
        let _ = tx.send((gen, Msg::Tts(e)));
    };
    let state = s.state;
    let pressure = move |on: crate::runtime::Placement| room::gpu_pressure(state, on);
    let result = loop {
        // A stop raised while the last batch was made ends it here, before
        // an item it held back is passed on.
        if s.stop.is_raised() {
            break Err(crate::proxy::canceled("stopped by the caller"));
        }
        let next = match pending.take() {
            Some(w) => Some(w),
            None => tokio::select! {
                biased;
                () = s.stop.raised() => break Err(crate::proxy::canceled("stopped by the caller")),
                w = s.queue.recv() => w,
            },
        };
        let mut batch = match next {
            None => break Ok(()),
            // The text stopped: nothing to end here.
            Some(Work::Break) => continue,
            Some(Work::Pass(d)) => {
                let _ = s.tx.send((s.gen, Msg::Delta(d)));
                continue;
            }
            Some(Work::Report(msg)) => {
                let _ = s.tx.send((s.gen, msg));
                continue;
            }
            Some(Work::Unspoken(raw)) => {
                let _ = s.tx.send((s.gen, Msg::Unspoken(raw)));
                continue;
            }
            Some(Work::Clause {
                tts,
                said,
                written,
                cue,
            }) => Batch::new(tts, said, written, cue),
        };
        // Later clauses join while the listener has audio to hear (`batch`).
        let audio = Duration::from_micros(samples * 1_000_000 / u64::from(INPUT_RATE));
        match batch::gather(
            &mut batch,
            &mut s.queue,
            |c| pace.deadline(audio, c),
            s.stop,
        )
        .await
        {
            Ok(next) => pending = next,
            Err(e) => break Err(e),
        }
        // Back-pressure: no further ahead of the paced send than the
        // session allows (§8.2, WP3 review M3) — lifted when the GPU needs
        // the model let go; a client that stops reading keeps it, and the
        // model is let go until it reads again (`room`).
        let made = room::before_clause(
            &mut room,
            synthesis.as_mut(),
            (s.gen, samples),
            s.stop,
            &pressure,
            (&s.speech.alias, &s.speech.label),
        )
        .await;
        if let Err(e) = made {
            break Err(e);
        }
        if synthesis.is_none() {
            route_event(TtsEvent::Opening);
            let synth = match open(s.state, s.ctx, s.speech, s.stop).await {
                Ok(o) => synthesis.insert(o),
                Err(e) => break Err(e),
            };
            let alias = s.speech.alias.clone();
            let report = |names| {
                let _ = tx.send((gen, Msg::Voices { alias, names }));
            };
            match settle::settle(s.state, synth, s.speech, s.stop, report).await {
                Ok(v) => voice = v,
                Err(e) => break Err(e),
            }
            route_event(TtsEvent::Opened {
                answered_by: synth.fallback().map(str::to_string),
                voice: voice.clone(),
            });
        }
        let synth = synthesis.as_mut().expect("opened above");
        let clause = ClauseSpeech {
            input: &batch.tts,
            voice: voice.as_deref(),
            speed: s.speech.speed,
            language: s.speech.language.as_ref(),
            instructions: s.speech.instructions.as_deref(),
            cue: batch.cue.as_deref(),
            seed: s.speech.seed,
        };
        let began = std::time::Instant::now();
        let wav = match synth.speak(&clause, Some(s.stop)).await {
            Ok((w, shaped)) => {
                if !std::mem::replace(&mut reported, true) {
                    report(s.speech, synth, &shaped);
                }
                w
            }
            // A clause of nothing but inline tags (`[laughs]` on its own),
            // or of nothing but characters the row's engine cannot say (a
            // lone `😊` to Supertonic, logged with the response's other
            // such characters when it ends), has nothing to say: it stays
            // in the transcript as written, with no audio, and the answer
            // goes on.
            Err(GatewayError::InvalidRequest {
                code: "empty_input",
                ..
            }) => {
                for (part, pcm) in batch.split(bytes::Bytes::new()) {
                    let (text, written) = (part.said, part.written);
                    let _ = s.tx.send((s.gen, Msg::Clause { text, written, pcm }));
                }
                continue;
            }
            Err(e) => break Err(e),
        };
        let pcm = match to_output(wav, s.speech.longest_pause_ms).await {
            Ok(p) => p,
            Err(e) => break Err(e),
        };
        pace.took(batch.chars(), began.elapsed());
        samples += (pcm.len() / 2) as u64;
        // One row per clause, the batch's audio shared out (`batch`).
        for (part, pcm) in batch.split(pcm) {
            let (text, written) = (part.said, part.written);
            let _ = s.tx.send((s.gen, Msg::Clause { text, written, pcm }));
        }
        pace.out();
    };
    if result.is_err() {
        s.stop_chat.stop();
    }
    if let Some(synth) = synthesis {
        let audio_ms = samples * 1000 / u64::from(INPUT_RATE);
        synth.finish(result.as_ref().err(), audio_ms).await;
    }
    result
}

/// What the first clause's shaping dropped or stripped, once per response
/// (WP10 D12): a fallback is shaped by its own rules, and a TTS that
/// strips the model's tags is worth knowing about.
fn report(speech: &Speech, synth: &Synthesis, shaped: &crate::audio::shape::ShapeReport) {
    // The primary dropping the style was said when it was resolved.
    let expected = speech.dropped && synth.fallback().is_none();
    let lost = expressive::shaping_losses(shaped, expected);
    if !lost.is_empty() {
        let answering = synth.fallback().unwrap_or(&speech.alias);
        tracing::info!(
            "{}: TTS '{answering}': {} (first clause; said once per response)",
            speech.label,
            lost.join("; ")
        );
    }
}

/// The key's check, then the response's one TTS route (§9.1, §10.3), judged
/// on the speech instructions and the voice its clauses carry (a
/// voice-design row needs a description; an engine that clones a clip, its
/// transcript). Both under the caller's label ([`Speech::proto`]), and
/// checked and counted against the caller's key: a session's, a device's
/// read-aloud in the Chat (client-apps design L4); the owner's read-aloud
/// carries no key, and nothing is counted for it.
async fn open(
    state: &SharedState,
    ctx: &RequestCtx,
    speech: &Speech,
    stop: &StopSignal,
) -> Result<Synthesis, GatewayError> {
    let alias = speech.alias.as_str();
    crate::proxy::policy_checked_call(state, speech.proto, ctx, alias, RequestClass::Audio).await?;
    let instructions = speech.instructions.as_deref();
    let synth = synthesize::open(
        state,
        ctx,
        speech.proto,
        &speech.label,
        alias,
        instructions,
        speech.voice.send.as_deref(),
        Some(stop),
    )
    .await?;
    if let Some(fallback) = synth.fallback() {
        tracing::info!(
            "{}: TTS '{alias}' is answered by its fallback '{fallback}'",
            speech.label
        );
    }
    Ok(synth)
}

/// A TTS answer as the session's output: PCM16-LE at 24 kHz (§8.2), the
/// form the writer paces it in, no silence in it longer than
/// `longest_pause_ms` (`audio::pauses`). Parsing and resampling are CPU
/// work — milliseconds for a clause — so they run on the blocking pool.
async fn to_output(wav: bytes::Bytes, longest_pause_ms: u32) -> Result<bytes::Bytes, GatewayError> {
    tokio::task::spawn_blocking(move || {
        decode(&wav, longest_pause_ms).map(|pcm| bytes::Bytes::from(pcm16_to_le_bytes(&pcm)))
    })
    .await
    .map_err(|e| GatewayError::Internal(format!("decoding the TTS answer failed: {e}")))?
}

fn decode(wav: &[u8], longest_pause_ms: u32) -> Result<Vec<i16>, GatewayError> {
    let bad = |why: String| GatewayError::Upstream {
        status: 502,
        provider_type: None,
        message: format!("the TTS answer is not usable audio: {why}"),
    };
    let w = parse_wav(wav).map_err(|e| bad(format!("{e:?}")))?;
    if !RATES.contains(&w.rate) {
        return Err(bad(format!(
            "its WAV header claims {} Hz, outside {}..={} Hz",
            w.rate,
            RATES.start(),
            RATES.end()
        )));
    }
    let pcm = if w.rate == INPUT_RATE {
        f32_to_pcm16(&w.samples)
    } else {
        // The exact length the conversion makes: the bound is the clip's
        // own, never a guessed cap.
        let exact = (w.samples.len() as u128 * u128::from(INPUT_RATE)).div_ceil(u128::from(w.rate));
        let exact = usize::try_from(exact).unwrap_or(usize::MAX);
        let out = resample(&w.samples, w.rate, INPUT_RATE, exact)
            .map_err(|e| bad(format!("resampling from {} Hz: {e}", w.rate)))?;
        f32_to_pcm16(&out)
    };
    // The engine's silence before, after and inside it is no longer than
    // the longest pause; the heard table counts what is left.
    let mut pcm = limit_pauses(pcm, INPUT_RATE, longest_pause_ms);
    // Every clause joins the one before without a click; the sample count —
    // and with it the heard table — is unchanged.
    fade_edges(&mut pcm, (INPUT_RATE * CLAUSE_FADE_MS / 1000) as usize);
    Ok(pcm)
}

mod batch;
mod cue;
mod room;
mod settle;
#[cfg(test)]
mod tests;

use batch::{Batch, Pace};
use room::Room;
