//! Text-to-speech as an in-process call over **one held route** — a realtime
//! response's voice (realtime design §8.2, §9.1, §10.3, §11).
//!
//! `POST /v1/audio/speech` opens its route per request. A spoken answer is a
//! run of clauses, and opening the gate per clause would let a GPU hold
//! switched on mid-answer move the second half of a sentence to a fallback
//! voice (§9.1). So a response opens [`Synthesis`] once — the gate's routing,
//! hold swap and admission, exactly as the HTTP route does — and sends every
//! clause over that [`Opened`](crate::gate::Opened):
//! - each clause is OpenAI's speech JSON (`model` rewritten to the route's
//!   upstream model, `input`, `voice`, `instructions`, `response_format:
//!   "wav"`), shaped for the route that answers as `POST /v1/audio/speech`
//!   shapes it, through the audio routes' own send
//!   ([`super::audio::audio_send`]), so a dead container is recovered the
//!   same way. The caller hands over semantic fields ([`ClauseSpeech`]);
//!   how each route takes them is shaping's (`crate::audio::shape`), a
//!   seed goes only to an lmgw row that reads one ([`sends_seed`]), and a
//!   delivery cue only to a route that takes cues (`crate::audio::cues`,
//!   WP9b) — decided on the route that answers, a fallback too;
//! - every wait — the gate, the answer's headers, its body — is raced against
//!   the caller's [`StopSignal`], like `stream_once_on`'s: a cancel ends the
//!   work at its next await instead of when the upstream is done;
//! - the hold is dropped by [`Synthesis::release`] as soon as the last clause
//!   is in hand — not when the listener has heard it, seconds later — and
//!   let go while the client does not read, to be taken again on the same
//!   route when it does (`claim`);
//! - **one `request_logs` row per response** (§11), written by
//!   [`Synthesis::finish`] — also for a response stopped half-way, carrying
//!   what it synthesized. Per-clause rows would drown the Usage page in five
//!   or more rows per answer. The row carries the characters its answered
//!   clauses were sent — as sent, after the cue and shaping took what they
//!   take — as `chars_in`, and their count as its upstream requests, which a
//!   `per_request` fee prices (billable-units design §4.3, §4.5; this
//!   supersedes §11's "no new columns in v1" for characters). Neither is
//!   known when a stop or a failed send left a clause with the upstream
//!   unanswered ([`tally`]). The audio seconds that came back are output,
//!   which no unit prices, and go to the log line beside them.
//!
//! The key's policy is the caller's to check before [`open`] (§10.3); this
//! only records.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use bytes::Bytes;
use serde_json::{json, Value};

use crate::audio::cues;
use crate::audio::engine_errors::explain_speech;
use crate::audio::language::{Field, SpeechLanguage};
use crate::audio::preflight::{refuse_row, refuse_speech, refuse_undescribed, refuse_unsayable};
use crate::audio::profile::{InstructionsMode, SpeechProfile, Unvoiced};
use crate::audio::shape::ShapeReport;
use crate::audio::transcript;
use crate::audio::voices::RowSpeech;
use crate::config::{AudioModel, Route};
use crate::egress::apply_bearer_auth;
use crate::error::GatewayError;
use crate::gate::GateHeaders;
use crate::ingress::ClientProto;
use crate::ir::Usage;
use crate::state::SharedState;
use crate::telemetry::RequestClass;
use crate::vram::LocalHold;

use super::audio::measure::speech_chars;
use super::audio::{audio_send, local_speech, rules_on, shape_on, voices_body};
use super::stop::{canceled, stopped};
use super::{record, LogParams, RequestCtx, StopSignal};
pub(crate) use chars::CharsSeen;
use tally::Tally;

/// One response's speech: the route it opened once, and what it sent so far.
pub(crate) struct Synthesis {
    state: SharedState,
    ctx: RequestCtx,
    /// The caller's label on its request row: `realtime` for a session's
    /// response, the Chat's in-process label for read-aloud (chat-voice
    /// design §6.1).
    proto: ClientProto,
    /// What the log lines start with — `realtime <session id>`, `chat
    /// thread <id>`: two callers on one TTS alias would otherwise read
    /// alike.
    label: String,
    alias: String,
    route: Route,
    hold: Option<LocalHold>,
    headers: GateHeaders,
    /// The lmgw audio row the route lands on and what it speaks — the
    /// shaping every clause gets, as on `POST /v1/audio/speech`
    /// ([`crate::audio::shape`]). `None` for any other route, which gets
    /// the expressive half only (its alias's override, or tags stripped).
    speech: Option<(AudioModel, RowSpeech)>,
    started: Instant,
    /// The first clause's answer, relative to `started`.
    ttfb_ms: Option<i64>,
    /// The clauses answered, and the characters of `input` they were sent:
    /// the row's quantities ([`tally`]).
    tally: Tally,
    /// The characters shaping replaced or dropped because the row's engine
    /// cannot say them, logged once when the response ends ([`chars`]).
    chars: CharsSeen,
    /// The claim was let go while the client did not read, and the next
    /// clause takes it again (`claim`).
    regain: bool,
    /// The row was written (and the in-flight gauge closed).
    finished: bool,
}

/// One clause, as the caller means it (WP10 D13): what to say and how. The
/// route that answers decides what it takes of it ([`Synthesis::speak`]).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ClauseSpeech<'a> {
    /// The text, inline tags in the canonical form.
    pub input: &'a str,
    /// `None` only for a row whose own default preset speaks (§5.3).
    pub voice: Option<&'a str>,
    /// Sent only when it is not 1.
    pub speed: Option<f64>,
    /// The language, and whose it is: a realtime session's, a hint — sent
    /// only to an lmgw audio row that declares languages, in its vocabulary
    /// ([`crate::audio::language::hint_language`]); the Chat's configured
    /// one, a request — sent wherever the row takes it, in its spelling
    /// ([`crate::audio::language::tts_fit`]). A remote route never gets it.
    pub language: Option<&'a SpeechLanguage>,
    /// The speech instructions: a style, or the description a voice-design
    /// row designs from.
    pub instructions: Option<&'a str>,
    /// The clause's delivery cue (WP9b), normalised (`crate::audio::cues`):
    /// a route that takes cues gets it after the instructions, and the
    /// clause's leading tags left out of `input`; any other route never
    /// sees it, and its tags are shaped as ever.
    pub cue: Option<&'a str>,
    /// The session's seed, for a row that reads one ([`sends_seed`]).
    pub seed: Option<SessionSeed>,
}

/// A realtime session's TTS seed (WP10 D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionSeed {
    pub value: u32,
    /// The client pinned it (`session.lmgw.speech_seed`); otherwise lmgw
    /// drew it for the session.
    pub pinned: bool,
}

/// Whether a clause carries the session's seed (WP10 D6): only to a row
/// whose engine `reads_seed`, and then when the client pinned one, or when
/// the row's voice comes from its seed (`seeds_voice`) and it does not pin
/// its own (`row_pins`: a `seed` under its default request options, which
/// wins). A voice comes from the seed on a row that designs it (voice
/// design) and on one that draws a speaker when it is named none
/// ([`Unvoiced::DrawsSpeaker`], OmniVoice, R4 M1): without a seed
/// Qwen3-TTS draws one per request and OmniVoice keeps a random one, and
/// the voice changes from one clause to the next.
pub(crate) fn sends_seed(
    reads_seed: bool,
    seeds_voice: bool,
    row_pins: bool,
    pinned: bool,
) -> bool {
    reads_seed && (pinned || (seeds_voice && !row_pins))
}

/// A row's voice comes from its seed ([`sends_seed`]).
pub(crate) fn seeds_voice(profile: &SpeechProfile) -> bool {
    profile.instructions == InstructionsMode::VoiceDesign
        || profile.unvoiced == Unvoiced::DrawsSpeaker
}

/// What `route`'s lmgw audio row cannot speak at all, before anything is
/// started for it: a task its package does not run
/// ([`crate::audio::preflight::refuse_row`]), a voice-design row with no
/// description — none in `instructions`, the description every clause will
/// carry, and none of its own ([`refuse_undescribed`]) — or `voice`, the
/// voice every clause will name, a library clip without the transcript its
/// engine refuses to clone without ([`transcript::refuse_clip`]). `Ok` for
/// any other route. A response's open checks it before admission, and the
/// session's warm before a start (`realtime::warm`).
pub(crate) async fn refuse_route(
    state: &SharedState,
    route: &Route,
    instructions: Option<&str>,
    voice: Option<&str>,
) -> Result<(), GatewayError> {
    match local_speech(state, route).await {
        Some((row, speech)) => refuse_row(&row, &speech.profile)
            .and_then(|()| refuse_undescribed(&row, &speech.profile, instructions))
            .and_then(|()| transcript::refuse_clip(&row, &speech, voice)),
        None => Ok(()),
    }
}

/// Open `alias` for one response's speech: routing, the hold swap, the audio
/// routes' protocol check, what the row cannot speak at all (`open_checked`)
/// and admission — raced against `stop`. `proto` labels the request row
/// (`ClientProto::Realtime` for a session's response, the Chat's
/// `ClientProto::Chat` — `AdminChat` on an Admin Chat thread — for
/// read-aloud); `label` is what the log lines
/// start with (`realtime <session id>`, `chat thread <id>`); `instructions`
/// and `voice` are what every clause will carry. A refusal is traffic, and
/// writes its row here (§11).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open(
    state: &SharedState,
    ctx: &RequestCtx,
    proto: ClientProto,
    label: &str,
    alias: &str,
    instructions: Option<&str>,
    voice: Option<&str>,
    stop: Option<&StopSignal>,
) -> Result<Synthesis, GatewayError> {
    let started = Instant::now();
    state.telemetry.request_started();
    let opened = tokio::select! {
        biased;
        () = stopped(stop) => Err(None),
        o = open_checked(state, alias, instructions, voice) => o.map_err(Some),
    };
    match opened {
        Ok(o) => Ok(Synthesis {
            state: state.clone(),
            ctx: ctx.clone(),
            proto,
            label: label.to_string(),
            alias: alias.to_string(),
            speech: local_speech(state, &o.route).await,
            route: o.route,
            hold: o.hold,
            headers: o.headers,
            started,
            ttfb_ms: None,
            tally: Tally::new(),
            chars: CharsSeen::default(),
            regain: false,
            finished: false,
        }),
        Err(f) => {
            let (route, headers, e) = match f {
                Some(f) => (f.route, f.headers, f.error),
                None => (
                    None,
                    GateHeaders::default(),
                    canceled("stopped by the caller during admission"),
                ),
            };
            let params = log(
                state,
                ctx,
                proto,
                alias,
                started,
                route.as_deref(),
                &headers,
            );
            record(
                params,
                super::stop::row_status(&e),
                None,
                Usage::default(),
                Some((e.kind(), e.to_string())),
            )
            .await;
            Err(e)
        }
    }
}

/// The gate as the speech route opens it: resolve, then what the resolved
/// row cannot speak at all is refused before admission starts it — or
/// evicts a resident for it ([`refuse_route`]) — then admission.
///
/// `instructions` are what every clause will carry ([`ClauseSpeech`]): a
/// voice-design row speaks when they, or its own default description,
/// describe the voice; every clause would otherwise be refused
/// `instructions_required` after the model was admitted. So is `voice`:
/// an untranscribed clip its engine cannot clone is refused
/// `voice_needs_transcript` here, not by the engine after a start.
async fn open_checked(
    state: &SharedState,
    alias: &str,
    instructions: Option<&str>,
    voice: Option<&str>,
) -> Result<crate::gate::Opened, crate::gate::OpenFailed> {
    let routed = crate::gate::resolve(state, alias, crate::gate::RouteCheck::Audio).await?;
    if let Err(error) = refuse_route(state, routed.resolved(), instructions, voice).await {
        return Err(crate::gate::OpenFailed {
            route: Some(Box::new(routed.resolved().clone())),
            headers: routed.headers().clone(),
            error,
        });
    }
    routed.admit(state).await
}

/// One clause as it is sent on `route` (WP10 D13): OpenAI's speech JSON
/// with `model` rewritten to the route's upstream model, the clause's
/// fields as the route takes them (`language` only to an lmgw audio row
/// that takes it, in the field it reads — [`SpeechLanguage::for_row`] —
/// `seed` only to one that reads it,
/// [`sends_seed`], its cue only to a route that takes cues,
/// [`crate::audio::cues::fold`]), checked by the speech route's preflight
/// and shaped for `answering` ([`crate::audio::shape`]). `speech` is the
/// lmgw audio row the route lands on, `None` for any other route; `label`
/// what the log lines start with. What
/// [`Synthesis::speak`] sends, and the session warm's load of the voice
/// (`warm`).
fn clause_body(
    state: &SharedState,
    label: &str,
    route: &Route,
    speech: Option<&(AudioModel, RowSpeech)>,
    answering: &str,
    clause: &ClauseSpeech<'_>,
) -> Result<(Value, ShapeReport), GatewayError> {
    let snap = state.snapshot();
    // The cue, on the route that answers (WP9b C5): before the preflight
    // and shaping, which then see the clause as it is sent.
    let rules = rules_on(&snap, speech, answering);
    let row = speech.map(|(row, _)| row);
    let folded = cues::fold(&rules, row, clause.input, clause.instructions, clause.cue);
    let (input, instructions) = match &folded {
        Some(f) => {
            tracing::debug!(
                "{label}: TTS '{answering}': cue '{}', instructions sent: '{}'",
                clause.cue.unwrap_or_default(),
                f.instructions
            );
            (f.input, Some(f.instructions.as_str()))
        }
        None => (clause.input, clause.instructions),
    };
    let mut body = json!({
        "model": route.upstream_model,
        "input": input,
        // audio.cpp answers with a WAV even for `pcm`; asking for one keeps
        // every upstream's answer parseable (§8.2).
        "response_format": "wav",
    });
    if let Some(v) = clause.voice {
        body["voice"] = Value::String(v.to_string());
    }
    if let Some(s) = clause.speed.filter(|s| (s - 1.0).abs() > f64::EPSILON) {
        body["speed"] = json!(s);
    }
    if let Some(i) = instructions.filter(|i| !i.trim().is_empty()) {
        body["instructions"] = Value::String(i.to_string());
    }
    if let Some((row, speech)) = speech {
        // In the field the row's engine reads: the request's `language`, or
        // `options.language` for a family that reads only that.
        match clause
            .language
            .and_then(|l| l.for_row(&speech.profile, clause.voice))
        {
            Some((Field::Language, l)) => body["language"] = Value::String(l),
            Some((Field::Options, l)) => body["options"]["language"] = Value::String(l),
            None => {}
        }
        let p = &speech.profile;
        if let Some(seed) = clause.seed.filter(|s| {
            sends_seed(
                p.reads_seed,
                seeds_voice(p),
                row.default_request_options.contains_key("seed"),
                s.pinned,
            )
        }) {
            body["seed"] = json!(seed.value);
        }
    }
    if let Some(obj) = body.as_object() {
        refuse_speech(obj, speech.map(|(r, s)| (r, &*s.profile)))?;
        if let Some((row, s)) = speech {
            transcript::refuse_body(row, s, obj)?;
        }
    }
    let shaped = shape_on(&snap, speech, answering, &mut body);
    if tracing::enabled!(tracing::Level::DEBUG) {
        // What goes up, its text left out: the reply is the owner's.
        let mut shown = body.clone();
        if let Some(i) = shown.get_mut("input") {
            *i = json!(format!(
                "<{} chars>",
                i.as_str().map_or(0, |t| t.chars().count())
            ));
        }
        tracing::debug!("{label}: TTS '{answering}' request: {shown}");
    }
    Ok((body, shaped))
}

impl Synthesis {
    /// The alias that answers when it is not the one asked for: the GPU
    /// hold's or admission's fallback (§9.2).
    pub fn fallback(&self) -> Option<&str> {
        self.headers.fallback()
    }

    /// Where the local model this response holds runs, if it holds one — a
    /// claim that keeps its container from being stopped. On the GPU that
    /// also keeps it from being evicted or stopped by the GPU hold's sweep;
    /// on the CPU only a benchmark's drain waits for it (the realtime
    /// back-pressure, `responder::speech::room`). `None` for a cloud route
    /// or a fallback.
    pub fn held_on(&self) -> Option<crate::runtime::Placement> {
        self.hold.as_ref().map(|h| h.placement())
    }

    /// The kind of upstream the route this response holds goes to — the
    /// fallback's, when one answers.
    pub fn kind(&self) -> crate::config::UpstreamKind {
        self.route.upstream.kind
    }

    /// The protocol of that upstream.
    pub fn protocol(&self) -> crate::config::Protocol {
        self.route.upstream.protocol
    }

    /// The voice names the model behind this route answers to. For an lmgw
    /// audio row, lmgw's own list ([`crate::audio::voices`]: presets, the
    /// voices the package ships, embeddings, the voice library) — no
    /// request, and the natives audio.cpp's list lacks are in it; the
    /// engine is asked as well only when the class's `voice_dir` is a
    /// directory lmgw cannot list. Otherwise the model's
    /// `GET /v1/audio/voices`, sent on the route and hold this response
    /// already has, so it starts nothing (realtime §5.3) — raced against
    /// `stop` like every other wait here. Metadata, so no row of its own.
    pub async fn voice_names(
        &self,
        stop: Option<&StopSignal>,
    ) -> Result<Vec<String>, GatewayError> {
        let mut names = Vec::new();
        if let Some((_, speech)) = &self.speech {
            names = speech.voices.names();
            if crate::web::voice_dir_is_library(&self.state.snapshot().settings.audio) {
                return Ok(names);
            }
        }
        let read = voices_body(&self.state, self.hold.as_ref(), &self.route);
        let body = tokio::select! {
            biased;
            () = stopped(stop) => return Err(canceled("stopped by the caller")),
            b = read => b?,
        };
        names.extend(parse_voice_names(&body));
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// Synthesize one clause; the answer is the upstream's WAV and what
    /// shaping changed in the request ([`crate::audio::shape`] — the same
    /// functions `POST /v1/audio/speech` applies, and its preflight: a
    /// clause of nothing but inline tags is `empty_input`, and so is one
    /// with nothing left once the characters the row's engine cannot say
    /// are fitted out; a voice-design row without a description
    /// `instructions_required`). Inline tags and
    /// instructions are shaped in what is sent, for the route that answers —
    /// a fallback by its own rules (WP10 D12) — never in the caller's text;
    /// so are the characters an lmgw row's engine cannot say, which
    /// [`Self::finish`] logs for the whole response.
    /// `language` goes only to an lmgw audio row that takes it
    /// ([`SpeechLanguage::for_row`]), and `seed` only to one that reads it
    /// ([`sends_seed`]): a remote route never gets either.
    pub async fn speak(
        &mut self,
        clause: &ClauseSpeech<'_>,
        stop: Option<&StopSignal>,
    ) -> Result<(Bytes, ShapeReport), GatewayError> {
        let answering = self.headers.fallback().unwrap_or(&self.alias);
        let (body, shaped) = clause_body(
            &self.state,
            &self.label,
            &self.route,
            self.speech.as_ref(),
            answering,
            clause,
        )?;
        self.chars.note(&shaped);
        // A clause shaping left nothing to say (a lone emoji): `empty_input`,
        // which the speaker skips, its characters noted above.
        let text = body.get("input").and_then(Value::as_str);
        let vocab = self
            .speech
            .as_ref()
            .and_then(|(_, s)| s.profile.char_vocab.as_deref());
        refuse_unsayable(text, &shaped, answering, vocab)?;
        let (state, route, hold) = (&self.state, &self.route, self.hold.as_ref());
        if let Some(hold) = hold {
            hold.note_sending();
        }
        // Whether the request may have gone out: a stop that is already
        // raised wins the biased race before the send is ever polled.
        let went_out = AtomicBool::new(false);
        let send = async {
            went_out.store(true, Ordering::Relaxed);
            audio_send(hold, route, |r| {
                let url = format!("{}/audio/speech", r.upstream.base());
                Ok(apply_bearer_auth(
                    state.http.post(url).json(&body),
                    &r.upstream,
                ))
            })
            .await
        };
        // What the TTS reads, not what the caller handed in: the cue and
        // the tags shaping strips are not said.
        let sent = speech_chars(&body);
        let answered = tokio::select! {
            biased;
            () = stopped(stop) => None,
            r = send => Some(r),
        };
        let resp = match answered {
            Some(Ok(resp)) => resp,
            None => {
                if went_out.load(Ordering::Relaxed) {
                    self.tally.in_doubt();
                }
                return Err(canceled("stopped by the caller"));
            }
            Some(Err(e)) => {
                if tally::leaves_doubt(&e) {
                    self.tally.in_doubt();
                }
                // audio.cpp's own refusal of the clip, or of an image
                // without eSpeak NG, said as lmgw's
                // (`crate::audio::engine_errors`).
                return Err(explain_speech(e, answering, clause.voice));
            }
        };
        // A 2xx: the upstream took the clause, whatever its body does next.
        self.tally.answered(sent);
        if self.ttfb_ms.is_none() {
            self.ttfb_ms = Some(self.started.elapsed().as_millis() as i64);
        }
        let wav = tokio::select! {
            biased;
            () = stopped(stop) => return Err(canceled("stopped by the caller")),
            b = resp.bytes() => b.map_err(|e| GatewayError::Transport(e.to_string()))?,
        };
        // Its header is the row's sample rate (`crate::audio::rates`).
        if let Some((row, _)) = &self.speech {
            self.state.audio_rates.learn(&row.model_id, &wav);
        }
        // The whole clause is in: the model is loaded and has run (realtime
        // design §9.4).
        if let Some(hold) = self.hold.as_ref() {
            hold.note_inference();
        }
        Ok((wav, shaped))
    }

    /// The last clause is in hand: the model is free for others now, not
    /// when the listener has heard it (§9.1).
    pub fn release(&mut self) {
        self.hold = None;
    }

    /// The response's one row (§11): `error` is why it ended early — a
    /// failed clause, or `canceled` for a stop — and `None` for a whole
    /// answer. The row carries the answered clauses' characters and count
    /// ([`tally`]). `audio_ms` is what was synthesized, for the log line.
    ///
    /// The contract: a response in which [`speak`](Self::speak) left a
    /// clause in doubt — a stop or a send with no answer — finishes with an
    /// error, as every caller does after a failed `speak` (the one it goes
    /// on after, `empty_input`, is refused before anything is sent). The
    /// row writer then keeps the unknown request count unknown; on a row
    /// finished as whole it would default it to 1. Checked in debug builds.
    pub async fn finish(mut self, error: Option<&GatewayError>, audio_ms: u64) {
        debug_assert!(
            error.is_some() || !self.tally.is_in_doubt(),
            "{}: a TTS response with a clause in doubt finished as whole",
            self.label
        );
        self.release();
        // A stop is a 200 `canceled` row, like the chat call's (`stop`).
        let status = error.map_or(200, super::stop::row_status);
        tracing::info!(
            "{}: TTS '{}': {} clause(s), {} characters sent, {:.1} s of audio, {} ms{}",
            self.label,
            self.alias,
            self.tally.clauses(),
            self.tally
                .chars()
                .map_or_else(|| "uncounted".to_string(), |c| c.to_string()),
            audio_ms as f64 / 1000.0,
            self.started.elapsed().as_millis(),
            error.map(|e| format!(" — {e}")).unwrap_or_default()
        );
        if let Some(line) = self.chars.line() {
            tracing::info!(
                "{}: TTS '{}': characters its engine cannot say: {line}",
                self.label,
                self.alias
            );
        }
        let params = LogParams {
            quantities: self.tally.quantities(),
            ..log(
                &self.state,
                &self.ctx,
                self.proto,
                &self.alias,
                self.started,
                Some(&self.route),
                &self.headers,
            )
        };
        record(
            params,
            status,
            self.ttfb_ms,
            Usage::default(),
            error.map(|e| (e.kind(), e.to_string())),
        )
        .await;
        self.finished = true;
    }
}

impl Drop for Synthesis {
    fn drop(&mut self) {
        // Dropped without its row — the task making it was torn down — the
        // request is abandoned rather than counted in flight for good.
        if !self.finished {
            self.state.telemetry.request_abandoned();
        }
    }
}

/// The row's parameters: the audio routes' own (`RequestClass::Audio`),
/// labelled with the caller's `proto` — `realtime` for a session's response
/// (realtime design §11), the Chat's own label, `chat` (`admin` on an Admin
/// Chat thread), for read-aloud (chat-voice design §6.1).
fn log<'a>(
    state: &'a SharedState,
    ctx: &'a RequestCtx,
    proto: ClientProto,
    alias: &str,
    started: Instant,
    route: Option<&'a Route>,
    headers: &GateHeaders,
) -> LogParams<'a> {
    LogParams {
        state,
        proto,
        ctx,
        alias: alias.to_string(),
        route,
        started,
        streamed: false,
        class: RequestClass::Audio,
        timings: None,
        max_tokens_clamped: None,
        fallback: headers.fallback_reason(),
        rung: None,
        degraded: None,
        quantities: Default::default(),
    }
}

/// The names in a voice list. audio.cpp answers `{"voices": [...]}`, whose
/// entries are names or objects naming one (`id`, `voice_id`, `name`);
/// `presets` is read the same way, as an array or as an object keyed by
/// name. Anything else contributes nothing.
fn parse_voice_names(body: &[u8]) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    let name = |e: &Value| -> Option<String> {
        if let Some(s) = e.as_str() {
            return Some(s.to_string());
        }
        ["id", "voice_id", "name"]
            .iter()
            .find_map(|k| e.get(k).and_then(Value::as_str).map(str::to_string))
    };
    let mut out: Vec<String> = Vec::new();
    let lists = [v.get("voices"), v.get("presets"), v.get("data"), Some(&v)];
    for list in lists.into_iter().flatten() {
        match list {
            Value::Array(a) => out.extend(a.iter().filter_map(name)),
            Value::Object(m) if !std::ptr::eq(list, &v) => out.extend(m.keys().cloned()),
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

mod chars;
mod claim;
mod tally;
pub(crate) mod warm;

#[cfg(test)]
mod tests {
    use super::parse_voice_names as voice_names;

    #[test]
    fn voice_lists_in_every_shape_seen() {
        assert_eq!(
            voice_names(br#"{"voices": ["alba", {"id": "cosette"}, {"voice_id": "x"}, 3]}"#),
            ["alba", "cosette", "x"]
        );
        assert_eq!(
            voice_names(br#"{"voices": [], "presets": {"narrator": {"voice_id": "alba"}}}"#),
            ["narrator"]
        );
        assert_eq!(voice_names(br#"["a", "b"]"#), ["a", "b"]);
        assert!(voice_names(b"not json").is_empty());
        assert!(voice_names(br#"{"error": "nope"}"#).is_empty());
    }
}
