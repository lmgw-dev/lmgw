//! The session's input audio (realtime design §4.1, §6.1, §6.2, §6.6): what
//! `input_audio_buffer.append` feeds, and the turns it yields.
//!
//! One timeline per session: every PCM16 sample at 24 kHz the client ever
//! appended, counted from the start — `audio_start_ms` / `audio_end_ms` are
//! positions on it, as OpenAI defines them, and a clear does not restart it.
//!
//! - **Turn detection on** (`server_vad`, and `semantic_vad` served by it
//!   until WP6, §6.3): the samples go to the detector ([`ServerVad`], which
//!   keeps the pre-roll and the open turn) and, resampled to 16 kHz by a
//!   streaming resampler aligned to the timeline, to Silero one 32 ms frame
//!   at a time, after the boost-only level normalizer (VAD input only — ASR
//!   gets the samples as they came). Each frame's probability goes back to
//!   the detector with the frame's span on the timeline. All of it is a
//!   fraction of a millisecond per frame (§22), so it runs inline on the
//!   session's task; only loading the model (~28 ms) goes to the blocking
//!   pool, once per session, on the first audio that needs it.
//! - **Manual** (`turn_detection: null`, §6.6): the samples collect in the
//!   uncommitted buffer until the client commits or clears it. The buffer
//!   grows until then, like OpenAI's: the ASR engine's own body limit is
//!   where a runaway one surfaces (§10.4). The detector only keeps counting
//!   the timeline, so a switch back finds it in step.
//!
//! The memory of the detected mode is bounded by what was received: idle,
//! the detector keeps the pre-roll only; in a turn it keeps the turn.
//!
//! **An append is taken in slices** of [`SLICE`] samples (200 ms), the
//! session's task yielding between them. A message may be as large as
//! `realtime.max_message_mb` allows — 64 MiB by default, ~33k Silero frames
//! and seconds of CPU — and taken whole it would hold a runtime worker for
//! all of it, and leave every buffer on its way (the resampler's input, its
//! output, the framer, the ring) at that size for the rest of the session
//! (WP2 review R2). Sliced, the buffers stay the size of a slice, and what
//! an earlier turn or append grew is released afterwards
//! ([`AudioIn::release`]). Every sample is still processed: a slice is a unit
//! of work, not a limit.
//!
//! **A detector that is down** — Silero failed to load or to run — keeps no
//! audio either: it is the timeline only, as in manual mode, until a clear
//! or a switch to manual and back starts it again (WP2 review R3). Holding
//! the audio would be a buffer nobody can commit. A turn open when it went
//! down goes with it at once, and [`DetectorDown`] says where on the
//! timeline, so the core can close the turn it announced (package A review
//! #5).
//!
//! **While the session speaks** (§6.4, §6.5) each frame is placed on the
//! wall clock by when its append arrived, and judged against the client's
//! playing window, which the core passes with every append ([`Listen`]):
//! inside it the barge-in gate decides whether speech is a turn, elsewhere
//! the detector's normal onset (`listen`, `turn::arbiter`).
//!
//! **`semantic_vad`** (§6.3): a pause's score request leaves with the
//! append as a [`ScoreJob`] — the 16 kHz audio the Silero stream already
//! made (`ring16`) — for the core to score off its task; the answer comes
//! back through [`AudioIn::turn_scored`], and may end the turn between two
//! appends.

use std::time::Instant;

use super::audio::pcm::pcm16_to_f32;
use super::audio::resample::{StreamResampler, INPUT_RATE};
use super::audio::vad::{level_normalize, Framer, Vad, VadError, FRAME, SILERO_VAD_ONNX};
use super::input::Barge;
use super::turn::arbiter::{CheckRequest, Dropped, Verdict};
use super::turn::semantic::{Decision, TurnEnd};
use super::turn::server_vad::{ParamError, ServerVad, ServerVadParams, TurnEvent};

mod listen;
mod ring16;
mod timeline;

use ring16::Ring16;

use listen::Listener;
pub(crate) use listen::{Listen, PlayView};

/// One Silero frame on the 24 kHz timeline: 512 samples at 16 kHz.
const FRAME_24K: u64 = FRAME as u64 * INPUT_RATE as u64 / 16_000;

/// The samples an append is processed in at a time (module doc): 200 ms at
/// 24 kHz, a few Silero frames.
pub(crate) const SLICE: usize = INPUT_RATE as usize / 5;

/// The detector's model half: Silero and the 24 → 16 kHz stream feeding it.
struct Dsp {
    vad: Vad,
    resampler: StreamResampler,
    framer: Framer,
    /// The timeline sample the resampler's output 0 stands for.
    base: u64,
    /// Frames scored since `base`.
    frames: u64,
    scratch: Vec<f32>,
    /// The 16 kHz stream since `base`, for Smart Turn (`ring16`).
    ring: Ring16,
}

impl Dsp {
    fn restart(&mut self, at: u64) {
        self.vad.reset();
        self.resampler.reset();
        self.framer.reset();
        self.base = at;
        self.frames = 0;
        self.ring.restart(at);
    }
}

/// A pause to score with Smart Turn (§6.3): its request's id and audio.
#[derive(Debug)]
pub(crate) struct ScoreJob {
    pub id: u64,
    /// 16 kHz, the turn's last 8 s up to 200 ms into the pause.
    pub samples: Vec<f32>,
    /// Where the pause began on the input timeline, in milliseconds — for
    /// the log.
    pub pause_ms: u64,
}

/// A failure of the detector's model — reported once, as an `error`.
#[derive(Debug)]
pub(crate) struct DetectorDown {
    pub why: String,
    /// Where on the timeline it went down: where a turn open then ends.
    pub at_ms: u64,
}

/// What one append did: the turn events it completed, in order, and the
/// detector's failure if it went down on the way (the events before it
/// still count).
#[derive(Default)]
pub(crate) struct Appended {
    pub events: Vec<Detected>,
    pub down: Option<DetectorDown>,
    /// Speech in the playing window that was deliberately no turn (§6.4):
    /// for the log.
    pub dropped: Vec<Dropped>,
    /// Unconfirmed barge-in turns whose words the session must check
    /// (§6.4, `turn::arbiter::check`).
    pub checks: Vec<CheckRequest>,
    /// Unconfirmed barge-in turns that went on past the playing window and
    /// are normal turns now: where each began, in milliseconds — for the
    /// log.
    pub promoted: Vec<u64>,
    /// Pauses to score with Smart Turn (`semantic_vad`, §6.3).
    pub scores: Vec<ScoreJob>,
}

/// A turn event, and for a turn's end when its last speech was heard: the
/// clock reading when that audio was processed — the start of the §11
/// timing line's "end of turn → commit".
pub(crate) struct Detected {
    pub event: TurnEvent,
    pub speech_end: Option<Instant>,
    /// When the frame that decided it was captured (`timeline`): what the
    /// core judges an interruption by (§6.4).
    pub at: tokio::time::Instant,
    /// The barge-in gate started this turn (§6.4).
    pub barge_in: bool,
    /// For a turn's end: which part of `semantic_vad`'s rule ended it
    /// (§6.3) — the timing line names it (§11).
    pub end: Option<TurnEnd>,
}

/// The session's input audio.
pub(crate) struct AudioIn {
    /// Samples received since the session began: the timeline's end.
    received: u64,
    detector: ServerVad,
    /// Whether turns are detected (`turn_detection` is set).
    detecting: bool,
    /// Loaded on the first audio that needs it.
    dsp: Option<Dsp>,
    /// The model failed to load or run; detection is off until a clear.
    down: bool,
    /// Manual mode's uncommitted buffer.
    manual: Vec<i16>,
    /// When the open turn's speech was last heard (`Detected::speech_end`):
    /// the last frame that left no silence counted, so a blip inside the
    /// silence window does not move it.
    speech_seen: Option<Instant>,
    /// The frames' capture instants and the barge-in arbiter (`listen`).
    listener: Listener,
}

impl AudioIn {
    /// `params`: the detector's parameters, `None` for manual turns.
    pub fn new(params: Option<&ServerVadParams>) -> Result<Self, ParamError> {
        let detector = ServerVad::new(params.unwrap_or(&ServerVadParams::default()))?;
        Ok(Self {
            received: 0,
            detector,
            detecting: params.is_some(),
            dsp: None,
            down: false,
            manual: Vec::new(),
            speech_seen: None,
            listener: Listener::new(INPUT_RATE),
        })
    }

    /// The session's barge-in knobs (§6.4, §12): the gate's evidence and
    /// guard, whether input during playback is heard at all, and whether
    /// the gate's turns wait for the word check — which needs an ASR alias
    /// (`asr`): without one the duration rule decides.
    pub fn set_barge_in(&mut self, barge: &Barge, asr: bool) {
        self.listener
            .configure(barge.params, barge.half_duplex, barge.words && asr);
    }

    /// A `session.update` that may have changed `turn_detection`.
    ///
    /// Between the two modes the uncommitted audio moves with the switch: an
    /// open turn becomes the manual buffer, and a manual buffer is dropped
    /// for the detector, which starts listening from here (it cannot judge
    /// audio it never scored, and holding it would be a buffer nobody can
    /// commit).
    pub fn configure(&mut self, params: Option<&ServerVadParams>) -> Result<(), ParamError> {
        match (self.detecting, params) {
            (true, Some(p)) => self.detector.set_params(p)?,
            (true, None) => {
                let open = self.detector.turn_start();
                self.manual = open
                    .map(|start| self.detector.audio(start, self.received))
                    .unwrap_or_default();
                self.detector.reset();
                self.listener.reset();
                self.detecting = false;
            }
            (false, Some(p)) => {
                self.detector.set_params(p)?;
                self.manual = Vec::new();
                self.detector.reset();
                self.listener.reset();
                if let Some(dsp) = &mut self.dsp {
                    dsp.restart(self.received);
                }
                self.detecting = true;
                // A fresh start for a model that failed (module doc).
                self.down = false;
            }
            (false, None) => {}
        }
        Ok(())
    }

    pub fn detecting(&self) -> bool {
        self.detecting
    }

    /// The timeline's end, in milliseconds: where a turn closed now ends.
    pub fn end_ms(&self) -> u64 {
        self.received * 1000 / u64::from(INPUT_RATE)
    }

    /// Append decoded samples that arrived at `arrived`, slice by slice
    /// (module doc), judged against what `listen` says of the output; the
    /// turn events they complete, in order.
    pub async fn append(
        &mut self,
        pcm: &[i16],
        arrived: tokio::time::Instant,
        listen: &Listen,
    ) -> Appended {
        let mut out = Appended::default();
        if self.detecting && !self.down {
            let from = self.received;
            self.listener.stamp(from, from + pcm.len() as u64, arrived);
        }
        let mut slices = pcm.chunks(SLICE).peekable();
        while let Some(slice) = slices.next() {
            if let Err(why) = self.append_slice(slice, arrived, listen, &mut out).await {
                out.down = Some(self.fail(why));
            }
            if slices.peek().is_some() {
                tokio::task::yield_now().await;
            }
        }
        self.release();
        out
    }

    async fn append_slice(
        &mut self,
        pcm: &[i16],
        arrived: tokio::time::Instant,
        listen: &Listen,
        out: &mut Appended,
    ) -> Result<(), String> {
        self.received += pcm.len() as u64;
        if !self.detecting || self.down {
            if !self.detecting {
                self.manual.extend_from_slice(pcm);
            }
            // Only the timeline: manual mode keeps no pre-roll, and a
            // detector that is down keeps nothing (module doc).
            self.detector.push_audio(pcm);
            self.detector.reset();
            return Ok(());
        }
        self.detector.push_audio(pcm);
        let start = self.received - pcm.len() as u64;
        if self.dsp.is_none() {
            self.dsp = Some(load(start).await?);
        }
        let Some(dsp) = self.dsp.as_mut() else {
            return Ok(());
        };
        let mut scratch = std::mem::take(&mut dsp.scratch);
        scratch.clear();
        if let Err(e) = dsp.resampler.push(&pcm16_to_f32(pcm), &mut scratch) {
            dsp.scratch = scratch;
            return Err(e.to_string());
        }
        if self.detector.semantic() {
            dsp.ring.push(&scratch);
        } else {
            dsp.ring.skip(scratch.len());
        }
        for frame in dsp.framer.push(&scratch) {
            let p = match dsp.vad.probability(&level_normalize(&frame)) {
                Ok(p) => p,
                // Non-finite input cannot come from PCM16; anything else is
                // the runtime itself, and a frame it cannot score is
                // reported rather than guessed as silence.
                Err(e) => {
                    dsp.scratch = scratch;
                    return Err(e.to_string());
                }
            };
            let from = dsp.base + dsp.frames * FRAME_24K;
            let to = from + FRAME_24K;
            dsp.frames += 1;
            let (judged, at) =
                self.listener
                    .frame(&mut self.detector, p, (from, to), listen, arrived);
            out.dropped.extend(judged.dropped);
            out.checks.extend(judged.check);
            out.promoted.extend(judged.promoted);
            for j in judged.judged.into_iter().chain(judged.stopped) {
                let (speech_end, end) = match j.event {
                    TurnEvent::SpeechStopped { .. } => {
                        (self.speech_seen.take(), self.detector.take_end())
                    }
                    TurnEvent::SpeechStarted { .. } => (None, None),
                };
                out.events.push(Detected {
                    event: j.event,
                    speech_end,
                    at,
                    barge_in: j.barge_in,
                    end,
                });
            }
            if let Some(r) = self.detector.take_score() {
                out.scores.push(ScoreJob {
                    id: r.id,
                    samples: dsp.ring.span(r.start, r.end),
                    pause_ms: self.detector.pause_start().unwrap_or(r.end) * 1000
                        / u64::from(INPUT_RATE),
                });
            }
            if self.detector.in_speech() && self.detector.silence() == 0 {
                self.speech_seen = Some(Instant::now());
            }
            self.listener.scored(to);
        }
        dsp.scratch = scratch;
        dsp.ring.trim(self.detector.pause_start());
        Ok(())
    }

    /// Give back what a long turn or a large append grew and no longer
    /// holds (module doc): memory follows what is kept, not what once was.
    fn release(&mut self) {
        self.detector.release(SLICE);
        if let Some(dsp) = &mut self.dsp {
            dsp.scratch.shrink_to(SLICE);
            dsp.ring.release();
        }
    }

    fn fail(&mut self, why: String) -> DetectorDown {
        self.down = true;
        // The open turn goes now, not at the next append: until then a
        // commit would still take the audio of a turn the core closes on
        // this (module doc).
        self.detector.reset();
        self.listener.reset();
        self.speech_seen = None;
        DetectorDown {
            why,
            at_ms: self.end_ms(),
        }
    }

    /// Whether word check `id` is the one its turn waits for: a verdict for
    /// any other changes nothing (§6.4).
    pub fn awaits_check(&self, id: u64) -> bool {
        self.detecting && !self.down && self.listener.awaits(id)
    }

    /// The word check `id` decided `verdict` (§6.4): the withheld
    /// `speech_started` on a cut, judged where the gate triggered; a reply
    /// announced as a normal turn, judged now — nothing plays, so nothing is
    /// cut; a turn discarded; the next check due. Nothing when that check's
    /// turn is gone.
    pub fn word_checked(&mut self, id: u64, verdict: Verdict) -> Appended {
        let mut out = Appended::default();
        if !self.detecting || self.down {
            return out;
        }
        let (checked, at) = self.listener.checked(&mut self.detector, id, verdict);
        out.dropped.extend(checked.dropped);
        out.checks.extend(checked.check);
        let now = tokio::time::Instant::now();
        out.events
            .extend(checked.judged.into_iter().map(|j| Detected {
                end: match j.event {
                    TurnEvent::SpeechStopped { .. } => self.detector.take_end(),
                    TurnEvent::SpeechStarted { .. } => None,
                },
                event: j.event,
                speech_end: None,
                at: if j.barge_in { at.unwrap_or(now) } else { now },
                barge_in: j.barge_in,
            }));
        out
    }

    /// Smart Turn's answer to score `id` (§6.3): `Some(p)`, or `None` when
    /// the pause could not be scored. What it decided — `None` when its
    /// pause is over — and the turn's end when it ends the turn now.
    pub fn turn_scored(&mut self, id: u64, p: Option<f32>) -> (Option<Decision>, Appended) {
        let mut out = Appended::default();
        if !self.detecting || self.down {
            return (None, out);
        }
        let (decision, event) = self.detector.scored(id, p);
        if let Some(event) = event {
            out.events.push(Detected {
                event,
                speech_end: self.speech_seen.take(),
                at: tokio::time::Instant::now(),
                barge_in: false,
                end: self.detector.take_end(),
            });
        }
        (decision, out)
    }

    /// The client's `input_audio_buffer.commit`: the uncommitted audio —
    /// manual mode's buffer, or the open turn up to the last sample (with
    /// detection on, audio outside a turn was judged silence and is gone).
    /// `None` when there is nothing to commit.
    pub fn commit(&mut self) -> Option<Vec<i16>> {
        if !self.detecting {
            return (!self.manual.is_empty()).then(|| std::mem::take(&mut self.manual));
        }
        let start = self.detector.turn_start()?;
        let samples = self.detector.audio(start, self.received);
        // Idle from here, and the next turn starts after this commit.
        self.detector.reset();
        self.listener.reset_gate();
        self.speech_seen = None;
        self.release();
        Some(samples)
    }

    /// `input_audio_buffer.clear`: drop the uncommitted audio and every bit
    /// of detector state, and give a failed model another chance.
    pub fn clear(&mut self) {
        self.manual = Vec::new();
        self.speech_seen = None;
        self.detector.reset();
        self.listener.reset();
        if let Some(dsp) = &mut self.dsp {
            dsp.restart(self.received);
        }
        self.down = false;
    }
}

/// Load Silero off the async path (~28 ms, §22): one model per session,
/// whose recurrent state is this stream's.
async fn load(at: u64) -> Result<Dsp, String> {
    let vad = tokio::task::spawn_blocking(|| Vad::from_bytes(SILERO_VAD_ONNX))
        .await
        .map_err(|e| format!("loading Silero VAD panicked or was cancelled: {e}"))?
        .map_err(|e: VadError| e.to_string())?;
    let mut ring = Ring16::default();
    ring.restart(at);
    Ok(Dsp {
        vad,
        resampler: StreamResampler::new().map_err(|e| e.to_string())?,
        framer: Framer::new(),
        base: at,
        frames: 0,
        scratch: Vec::new(),
        ring,
    })
}

#[cfg(test)]
mod tests;
