//! Transcribing committed turns (realtime design §4.2 steps 2–3, §5.2,
//! §10.3, §11).
//!
//! Each committed segment becomes one ASR call, made off the session core:
//! 1. the per-call policy check with the session's key (scope, budget, the
//!    per-minute counts — `policy::check_call`, the audio request class), so
//!    a key narrowed or revoked mid-session stops at its next turn;
//! 2. the segment, pre-roll included, resampled 24 → 16 kHz and written as a
//!    mono PCM16 WAV — the upload §3.1 measured fastest — on the blocking
//!    pool (~8 ms for a few seconds of speech, §22);
//! 3. the gateway's own transcription path (`proxy::transcribe_turn`): the gate
//!    (GPU hold, admission — the ASR hold is taken there for this call and
//!    dropped with the transcript, §9.1) and one `request_logs` row of class
//!    `audio` carrying the session's client key.
//!
//! **A turn the chat model hears** (voice-audio-input design §3.1) comes
//! with its WAV already being built at the commit ([`Wav`]): the call, and
//! the word check's second call, await that one instead of building their
//! own, and the request sends the same bytes.
//!
//! **One call at a time, in commit order.** A turn's transcript has to be in
//! the conversation before anything after it is answered (a response renders
//! every transcript before it, §7.2), and the next turn's call waits ~30 ms
//! at most (§3.1). The result goes back to the core with the item it
//! belongs to; the core is still the only writer of the conversation.
//!
//! **The barge-in word check** (§6.4, `check`) transcribes an unconfirmed
//! turn's audio with the same call, but at once — never queued behind the
//! turn calls, which the cut it decides must not wait for.
//!
//! **A second call for an empty turn** (§6.4, N3): when the turn's own
//! transcript comes back empty, but the word check that let the turn
//! through had heard words with another alias, the turn's audio goes once
//! more to that alias, through the same path — the key's policy check, the
//! gate, a row of its own — and its transcript is the turn's. Inside the
//! same job, so the turns after it still wait their turn. The check's own
//! words are not reused: it heard only the start of the turn.
//!
//! **The language** — the session's `audio.input.transcription.language`,
//! when set, as the two-letter ISO 639-1 code it names ("de" for "de-DE";
//! "german" names none and is not sent, fix package B6) — goes up with
//! every call, the turns' and the checks' alike, as the multipart
//! `language` field. audio.cpp's server takes it: its own
//! usage text lists `POST /v1/audio/transcriptions` with the fields "file,
//! model, language, prompt, stream" (checked in the audio-main build's
//! `audiocpp_server`, 2026-10-01). A model that refuses a value fails the
//! call visibly — a failed transcription for a turn, the duration rule for
//! a check.
//!
//! **Every call reports back.** One that panics answers `Err(Internal)`, or
//! `busy()` would stay true and every later turn would queue behind it for
//! good. And a call is never aborted: when the session ends — the
//! transcriber dropped with the core, and its stop with it — the call ends
//! at its next await, cooperatively, and still writes its row (`canceled`,
//! §11). Without the stop it would run, and keep the ASR model held, for as
//! long as the upstream's `request_timeout` allows, which 0 makes forever
//! (WP1c review #4).

use std::collections::VecDeque;

use futures::FutureExt;
use tokio::sync::mpsc;

use super::audio::pcm::{f32_to_pcm16, pcm16_to_f32, write_wav_pcm16_mono};
use super::audio::resample::{resample, INPUT_RATE, VAD_RATE};
use super::policy;
use crate::error::GatewayError;
use crate::proxy::{RequestCtx, StopHandle, StopSignal};
use crate::state::SharedState;
use crate::telemetry::RequestClass;
use crate::web::chat_voice::bound::AsrNow;

/// What the ASR rate is: the WAV goes up at 16 kHz (§4.2).
const ASR_RATE: u32 = VAD_RATE;

mod check;
mod wav;

pub(crate) use check::{CheckDone, CheckFailed};
pub(crate) use wav::{Upload, Wav};

/// A finished call, for the core.
pub(crate) struct Done {
    pub item_id: String,
    /// The segment's length in seconds: the transcription's `usage` (§11).
    pub seconds: f64,
    pub result: Result<String, GatewayError>,
    /// The turn was transcribed a second time (module doc).
    pub again: Option<Again>,
    /// How it was transcribed: what a bound session stores with the user
    /// message and times (chat-voice design §3, §8.7).
    pub facts: Facts,
}

/// How a committed turn was transcribed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// The ASR alias asked.
    pub alias: String,
    /// The alias that answered in its place (a fallback), when one did.
    pub answered_by: Option<String>,
    /// The call, admission included.
    pub ms: u64,
    /// The turn's audio, pre-roll included.
    pub audio_ms: u64,
    /// Its model was not up and loaded when the call started: it loaded
    /// during the turn. Known for a bound session only.
    pub cold: bool,
}

/// A transcript, and the fallback that answered for it.
pub(crate) struct Transcribed {
    pub text: String,
    pub answered_by: Option<String>,
}

/// A turn's second transcription (module doc), for the core's log.
pub(crate) struct Again {
    /// The turn's ASR alias, which heard nothing.
    pub asr: String,
    /// The word check's alias, which transcribed it again.
    pub check: String,
    /// Why the second call has no transcript, when it failed: the turn
    /// keeps its empty one then.
    pub failed: Option<GatewayError>,
}

/// What the session's ASR calls report to the core.
pub(crate) enum AsrMsg {
    /// A committed turn's transcript.
    Turn(Done),
    /// A barge-in word check's (`check`).
    Check(CheckDone),
}

pub(crate) type Tx = mpsc::UnboundedSender<AsrMsg>;

/// One committed segment waiting for its call.
struct Job {
    item_id: String,
    alias: String,
    /// The segment, or its WAV being built (a turn the chat model hears).
    upload: Upload,
    /// Its length in seconds: the transcription's `usage`.
    seconds: f64,
    /// The session's `audio.input.transcription.language` at the commit.
    language: Option<String>,
    /// The word check's alias, for a second call should this one come back
    /// empty (module doc).
    again: Option<String>,
}

/// The session's ASR calls: the one running and the ones queued behind it.
pub(crate) struct Transcriber {
    state: SharedState,
    ctx: RequestCtx,
    /// The session's id, for the log.
    sid: String,
    tx: Tx,
    queue: VecDeque<Job>,
    /// The item whose call is running. The call itself is detached (module
    /// doc).
    running: Option<String>,
    /// Raised when the transcriber is dropped with the session (module doc),
    /// or by [`Self::stop`].
    stop: StopHandle,
    signal: StopSignal,
    /// The chat thread a bound session transcribes for (chat-voice design
    /// §8.1): its ASR alias and language are read again when each call
    /// starts, so a chip or a language changed since applies from the next
    /// turn.
    thread: Option<i64>,
}

impl Transcriber {
    pub fn new(state: SharedState, ctx: RequestCtx, tx: Tx, sid: String) -> Self {
        let (stop, signal) = crate::proxy::stop_pair();
        Self {
            state,
            ctx,
            sid,
            tx,
            queue: VecDeque::new(),
            running: None,
            stop,
            signal,
            thread: None,
        }
    }

    /// Transcribe for chat thread `id` (a bound session's).
    pub fn bind(&mut self, id: i64) {
        self.thread = Some(id);
    }

    /// Transcribe `upload` (the 24 kHz segment, or its WAV being built for
    /// a turn the chat model hears) for `item_id` with `alias`; `seconds`:
    /// the segment's length; `language`: the session's
    /// `audio.input.transcription.language`, when set; `again`: the alias to
    /// transcribe it with once more should `alias` hear nothing (module doc).
    #[cfg(test)]
    pub fn push(
        &mut self,
        item_id: String,
        alias: String,
        upload: impl Into<Upload>,
        language: Option<String>,
        again: Option<String>,
    ) {
        let upload = upload.into();
        let seconds = match &upload {
            Upload::Samples(s) => s.len() as f64 / f64::from(INPUT_RATE),
            Upload::Wav(_) => 0.0,
        };
        self.push_timed(item_id, alias, (upload, seconds), language, again);
    }

    /// [`Self::push`] with the segment's length given: a WAV being built
    /// does not say it.
    pub fn push_timed(
        &mut self,
        item_id: String,
        alias: String,
        (upload, seconds): (Upload, f64),
        language: Option<String>,
        again: Option<String>,
    ) {
        self.queue.push_back(Job {
            item_id,
            alias,
            upload,
            seconds,
            language,
            again,
        });
        self.next();
    }

    /// Stop every call, running or queued: each ends at its next await and
    /// reports back as failed — a bound session's end that waited its
    /// bound for them (WP11 binding review m1).
    pub fn stop(&self) {
        self.stop.stop();
    }

    /// Whether any committed turn still waits for its transcript.
    pub fn busy(&self) -> bool {
        self.running.is_some() || !self.queue.is_empty()
    }

    /// Whether a committed turn a response must wait for still waits for its
    /// transcript: every one but those the chat model `hears` — a turn it
    /// hears goes as audio, and its response launches at once
    /// (voice-audio-input design §3.1). The session's end still waits on
    /// [`Self::busy`].
    pub fn busy_for_launch(&self, hears: impl Fn(&str) -> bool) -> bool {
        self.items().iter().any(|id| !hears(id))
    }

    /// The committed turns still waiting for their transcripts, in order.
    pub fn items(&self) -> Vec<String> {
        self.running
            .iter()
            .cloned()
            .chain(self.queue.iter().map(|j| j.item_id.clone()))
            .collect()
    }

    /// Whether `item_id` still waits for its transcript.
    pub fn has(&self, item_id: &str) -> bool {
        self.running.as_deref() == Some(item_id) || self.queue.iter().any(|j| j.item_id == item_id)
    }

    /// The call for `item_id` reported back: the next one starts. `false`
    /// when it was not the running call (nothing to do with it).
    pub fn finished(&mut self, item_id: &str) -> bool {
        if self.running.as_deref() != Some(item_id) {
            return false;
        }
        self.running = None;
        self.next();
        true
    }

    fn next(&mut self) {
        if self.running.is_some() {
            return;
        }
        let Some(job) = self.queue.pop_front() else {
            return;
        };
        let (state, ctx, tx) = (self.state.clone(), self.ctx.clone(), self.tx.clone());
        let (stop, sid) = (self.signal.clone(), self.sid.clone());
        let seconds = job.seconds;
        let thread = self.thread;
        self.running = Some(job.item_id.clone());
        tokio::spawn(async move {
            let mut job = job;
            let started = std::time::Instant::now();
            // A bound session's turn: the thread's ASR alias as it is now —
            // or, when the thread names none now (its chip cleared since),
            // the turn fails rather than keep the bind's alias (WP11
            // binding review NIT 1) — and its language as it is now, as the
            // reply's and the voice's are (chat-voice §2.1, 2026-10-04). A
            // thread gone keeps the bind's: its response says
            // `chat_thread_not_found`.
            let now = match thread {
                Some(id) => crate::web::chat_voice::bound::asr_now(&state, id)
                    .await
                    .map(|now| (id, now)),
                None => None,
            };
            let cold = match now {
                Some((
                    _,
                    AsrNow {
                        alias: Some(alias),
                        language,
                    },
                )) => {
                    job.alias = alias;
                    job.language = language;
                    crate::web::chat_voice::bound::cold(&state, &job.alias)
                }
                Some((id, AsrNow { alias: None, .. })) => {
                    let _ = tx.send(AsrMsg::Turn(Done {
                        facts: Facts {
                            alias: job.alias.clone(),
                            answered_by: None,
                            ms: 0,
                            audio_ms: (seconds * 1000.0).round() as u64,
                            cold: false,
                        },
                        item_id: job.item_id,
                        seconds,
                        result: Err(no_asr(id)),
                        again: None,
                    }));
                    return;
                }
                None if thread.is_some() && !job.alias.is_empty() => {
                    crate::web::chat_voice::bound::cold(&state, &job.alias)
                }
                None => false,
            };
            let item_id = job.item_id.clone();
            let alias = job.alias.clone();
            let (result, again) = turn_call(&state, &ctx, &sid, job, &stop).await;
            let facts = Facts {
                alias,
                answered_by: result.as_ref().ok().and_then(|t| t.answered_by.clone()),
                ms: started.elapsed().as_millis() as u64,
                audio_ms: (seconds * 1000.0).round() as u64,
                cold,
            };
            let _ = tx.send(AsrMsg::Turn(Done {
                item_id,
                seconds,
                result: result.map(|t| t.text),
                again,
                facts,
            }));
        });
    }
}

/// A bound turn's refusal when its thread names no transcription model.
fn no_asr(thread_id: i64) -> GatewayError {
    GatewayError::InvalidRequest {
        code: "asr_not_configured",
        message: format!(
            "this turn cannot be transcribed: chat thread {thread_id} names no transcription \
             model (set one in the thread's voice settings, or under Settings → Chat → Voice)"
        ),
    }
}

/// A committed turn's call — and a second one with the word check's alias
/// when the first came back empty (module doc), unless the session is gone.
async fn turn_call(
    state: &SharedState,
    ctx: &RequestCtx,
    sid: &str,
    job: Job,
    stop: &StopSignal,
) -> (Result<Transcribed, GatewayError>, Option<Again>) {
    let Job {
        item_id,
        alias,
        upload,
        language,
        again,
        ..
    } = job;
    // Kept only when a second call may need them: the samples, or another
    // handle on the same WAV.
    let kept = again.as_ref().map(|_| upload.clone());
    let result = caught(
        state,
        ctx,
        &alias,
        upload,
        stop,
        (sid, &item_id),
        language.as_deref(),
    )
    .await;
    let empty = matches!(&result, Ok(t) if t.text.trim().is_empty());
    let (Some(check), Some(upload), true, false) = (again, kept, empty, stop.is_raised()) else {
        return (result, None);
    };
    let what = format!("{item_id} (again, with the word check's {check})");
    let second = caught(
        state,
        ctx,
        &check,
        upload,
        stop,
        (sid, &what),
        language.as_deref(),
    )
    .await;
    let (result, failed) = match second {
        Ok(text) => (Ok(text), None),
        // The turn keeps its empty transcript.
        Err(e) => (result, Some(e)),
    };
    let again = Again {
        asr: alias,
        check,
        failed,
    };
    (result, Some(again))
}

/// [`call`], a panic inside it answered as `Err(Internal)` (module doc);
/// `(sid, what)`: the session and the call, for the log; `language` the
/// spoken language when the caller passes one.
async fn caught(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: &str,
    upload: Upload,
    stop: &StopSignal,
    (sid, what): (&str, &str),
    language: Option<&str>,
) -> Result<Transcribed, GatewayError> {
    std::panic::AssertUnwindSafe(call(state, ctx, alias, upload, stop, language))
        .catch_unwind()
        .await
        .unwrap_or_else(|panic| {
            let why = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "no message".into());
            tracing::error!("realtime {sid}: the ASR call for {what} panicked: {why}");
            Err(GatewayError::Internal(format!(
                "the transcription call failed inside lmgw: {why}"
            )))
        })
}

async fn call(
    state: &SharedState,
    ctx: &RequestCtx,
    alias: &str,
    upload: Upload,
    stop: &StopSignal,
    language: Option<&str>,
) -> Result<Transcribed, GatewayError> {
    policy::check_call(state, ctx, alias, RequestClass::Audio).await?;
    let wav = upload.wav().await?;
    let t = crate::proxy::transcribe_turn(state, ctx, alias, wav, stop, language).await?;
    Ok(Transcribed {
        answered_by: t.answered_by().map(str::to_string),
        text: t.text,
    })
}

/// The 24 kHz segment as the 16 kHz mono PCM16 WAV the ASR call uploads.
/// The output length is exactly what the input makes — no cap of its own:
/// the input buffer's bound is the ASR engine's body limit (§10.4).
pub(crate) fn wav_16k(samples: &[i16]) -> Result<Vec<u8>, GatewayError> {
    let exact = (samples.len() as u128 * u128::from(ASR_RATE)).div_ceil(u128::from(INPUT_RATE));
    let exact = usize::try_from(exact).unwrap_or(usize::MAX);
    let resampled = resample(&pcm16_to_f32(samples), INPUT_RATE, ASR_RATE, exact)
        .map_err(|e| GatewayError::Internal(format!("resampling the turn for ASR: {e}")))?;
    write_wav_pcm16_mono(&f32_to_pcm16(&resampled), ASR_RATE)
        .map_err(|e| GatewayError::Internal(format!("writing the turn's WAV for ASR: {e}")))
}
