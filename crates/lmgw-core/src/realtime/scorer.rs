//! Smart Turn for one session (realtime design §6.3, §13): each pause the
//! detector asks about is scored on the blocking pool — never on the
//! session's task — and the answer goes back to the core over a channel,
//! as the ASR calls' do (`transcribe`).
//!
//! - **One model per session**, loaded on the first pause it scores
//!   (~25 ms), with [`INTRA_THREADS`] intra-op threads: `run` takes
//!   `&mut self`, and a pause comes at most every few hundred milliseconds,
//!   so one is enough. A second request while the first still runs (the
//!   voice resumed and paused again within a score's time) waits for it.
//! - **A model that does not load** answers every request with its error,
//!   once loaded or not: the bytes are compiled in, and a second try would
//!   fail the same way. The core warns once and the detector falls back to
//!   the plain silence window (§6.3).
//! - **Every request answers.** A score that panics answers with an error,
//!   or its pause would wait for the maximum wait. A session that ends does
//!   not wait for a score in flight: it runs out (~20 ms) and its answer
//!   finds the channel closed.
//! - **Too little audio is not scored** (fix package B6): a span shorter
//!   than [`MIN_SCORED_MS`] — the ring had less, or the turn was that short
//!   — would reach the model left-padded with zeros to its 8 s, and
//!   near-silence scores as a finished turn. Its pause falls back to the
//!   plain silence window, said at DEBUG.
//! - **A request a newer one overtook is not scored** (fix package B6): it
//!   was queued behind a running score while the voice resumed and paused
//!   again, so its pause is over. It answers without running the model;
//!   the ids only grow within a session (`ServerVad`).
//! - **In process, no alias**: no gate, no hold and no usage row (§11).

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::audio_in::ScoreJob;
use super::turn::mel::SAMPLE_RATE;
use super::turn::semantic::MIN_SCORED_MS;
use super::turn::smart_turn::{SmartTurn, INTRA_THREADS, SMART_TURN_ONNX};
use crate::state::SharedState;

/// A stand-in for Smart Turn that tests install on a gateway
/// (`AppState::set_turn_score_for_tests`): 16 kHz audio in, a probability
/// or an error out.
pub type ScoreHook = Arc<dyn Fn(&[f32]) -> Result<f32, String> + Send + Sync>;

/// A scored pause, for the core.
pub(crate) struct ScoreDone {
    pub id: u64,
    /// Where the pause began on the input timeline (ms), for the log.
    pub pause_ms: u64,
    pub result: Result<f32, Unscored>,
    /// The score's own time on the blocking pool, model load included.
    pub took: Duration,
}

/// Why a pause has no score (module doc).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Unscored {
    /// Its audio is shorter than [`MIN_SCORED_MS`]: that many ms.
    Short(u64),
    /// A newer request overtook it while it waited.
    Overtaken,
    /// The model did not load, or the run failed or panicked.
    Failed(String),
}

pub(crate) type Tx = mpsc::UnboundedSender<ScoreDone>;

enum Model {
    Unloaded,
    Ready(Box<SmartTurn>),
    Failed(String),
}

impl Model {
    fn score(&mut self, audio: &[f32]) -> Result<f32, String> {
        if matches!(self, Self::Unloaded) {
            *self = match SmartTurn::from_bytes(SMART_TURN_ONNX, INTRA_THREADS) {
                Ok(m) => Self::Ready(Box::new(m)),
                Err(e) => Self::Failed(format!("the model did not load: {e}")),
            };
        }
        match self {
            Self::Ready(m) => m.score(audio).map_err(|e| e.to_string()),
            Self::Failed(e) => Err(e.clone()),
            Self::Unloaded => unreachable!("loaded above"),
        }
    }
}

/// The session's scorer (module doc).
pub(crate) struct Scorer {
    state: SharedState,
    tx: Tx,
    model: Arc<Mutex<Model>>,
    /// The newest request's id: an older one still queued is over.
    newest: Arc<AtomicU64>,
}

impl Scorer {
    pub fn new(state: SharedState, tx: Tx) -> Self {
        Self {
            state,
            tx,
            model: Arc::new(Mutex::new(Model::Unloaded)),
            newest: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Score `job` off the session's task; the answer comes on the channel.
    pub fn score(&self, job: ScoreJob) {
        self.newest.fetch_max(job.id, Ordering::Relaxed);
        let ms = job.samples.len() as u64 * 1000 / u64::from(SAMPLE_RATE);
        if ms < u64::from(MIN_SCORED_MS) {
            let _ = self.tx.send(ScoreDone {
                id: job.id,
                pause_ms: job.pause_ms,
                result: Err(Unscored::Short(ms)),
                took: Duration::ZERO,
            });
            return;
        }
        let hook = self.state.turn_score_hook();
        let model = self.model.clone();
        let newest = self.newest.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let t0 = Instant::now();
            // A request made while an earlier score runs waits here — and
            // is checked once the model is free: that wait is when a newer
            // request overtakes it.
            let mut model = model.lock();
            let result = if job.id < newest.load(Ordering::Relaxed) {
                Err(Unscored::Overtaken)
            } else {
                let run = || match &hook {
                    Some(h) => h(&job.samples),
                    None => match &mut model {
                        Ok(m) => m.score(&job.samples),
                        Err(_) => Err("an earlier score panicked".to_string()),
                    },
                };
                std::panic::catch_unwind(AssertUnwindSafe(run))
                    .unwrap_or_else(|_| Err("the score panicked".to_string()))
                    .map_err(Unscored::Failed)
            };
            drop(model);
            let _ = tx.send(ScoreDone {
                id: job.id,
                pause_ms: job.pause_ms,
                result,
                took: t0.elapsed(),
            });
        });
    }
}

#[cfg(test)]
mod tests;
