//! Smart Turn v3.2 on ONNX Runtime: the probability that a turn is
//! finished, from its last 8 s of 16 kHz audio (realtime §6.3, §13).
//!
//! Why it looks like this:
//! - **The model file** is `smart-turn-v3.2-cpu.onnx` from
//!   `pipecat-ai/smart-turn-v3` (BSD-2-Clause, 8.7 MB, int8; the licence
//!   sits beside it in `assets/realtime/`). Input `input_features
//!   [1, 80, 800]`; the output is named `logits` but the graph already ends
//!   in a sigmoid, so it *is* the probability (WP0).
//! - **Features in Rust** ([`super::mel`]): Whisper's log-mel front end as
//!   the Python extractor computes it, checked against its output.
//! - **The output is int8-quantized.** Every logit lies on a lattice of
//!   0.0394 (measured over 154 cuts), so p moves in steps of ~0.01 near
//!   0.5 and ~8e-4 near 0.98. ORT 1.28 here and 1.30 in the Python
//!   reference differ by exactly one step on `en_complete_short` (0.97771
//!   vs 0.97855) fed the same reference features: the parity floor.
//! - **One session per scorer**, `run` takes `&mut self` (§13). A score
//!   is ~48 ms on one intra-op thread, ~27 on two, ~18 on four (release,
//!   features 1.6 ms of it): the session's scorer takes
//!   [`INTRA_THREADS`] (owner's decision) and runs on `spawn_blocking`,
//!   never on the session's task (§6.3 "Cost"). The threads do not spin
//!   after a run: a score comes once per pause, and spinning would only
//!   burn a core for nothing between pauses. The session shares ONNX
//!   Runtime's process-wide environment with Silero (`ort`'s default,
//!   created on first use).
//! - **The score is a probability, never a gate** (§6.3): this module only
//!   computes it; [`super::semantic`] decides what it is worth.
//! - **Non-finite audio is refused**, like the VAD's: one NaN would turn
//!   the whole normalised window into NaN.

use super::mel::{MelFrontEnd, FEATURES, FRAMES, N_MELS};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use std::fmt;

/// Intra-op threads of a session's scorer: 18 ms a score against 48 on one
/// (realtime §6.3 "Cost", owner's decision).
pub const INTRA_THREADS: usize = 4;

/// The committed model (realtime §13), at `crates/lmgw-core/assets/realtime/`
/// next to its BSD-2-Clause licence (`LICENSE-smart-turn`) and the MIT
/// licence of the Whisper Tiny encoder it is built on (`LICENSE-whisper`).
pub static SMART_TURN_ONNX: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/realtime/smart-turn-v3.2-cpu.onnx"
));

/// Smart Turn errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmartTurnError {
    /// ONNX Runtime refused the model or the run.
    Ort(String),
    /// Features must be exactly [`FEATURES`] values (`[80, 800]`).
    FeatureLength(usize),
    /// The audio or the features hold NaN or an infinity.
    NonFinite,
    /// The model answered with an unexpected output.
    BadOutput(String),
}

impl fmt::Display for SmartTurnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ort(e) => write!(f, "Smart Turn: {e}"),
            Self::FeatureLength(n) => {
                write!(f, "Smart Turn needs {FEATURES} feature values, got {n}")
            }
            Self::NonFinite => write!(f, "Smart Turn input holds a non-finite value"),
            Self::BadOutput(e) => write!(f, "Smart Turn returned an unexpected output: {e}"),
        }
    }
}

impl std::error::Error for SmartTurnError {}

// `ort::Error<SessionBuilder>` is not `Send`, so it is flattened to text.
fn ort_err<E: fmt::Display>(e: E) -> SmartTurnError {
    SmartTurnError::Ort(e.to_string())
}

/// One Smart Turn scorer: the model session plus its feature front end.
pub struct SmartTurn {
    session: Session,
    mel: MelFrontEnd,
}

impl fmt::Debug for SmartTurn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmartTurn").finish_non_exhaustive()
    }
}

impl SmartTurn {
    /// Loads the graph from memory with `intra_threads` intra-op threads
    /// (`0`: ONNX Runtime's default). The bytes are copied; pass
    /// [`SMART_TURN_ONNX`] for the committed model.
    pub fn from_bytes(model: &[u8], intra_threads: usize) -> Result<Self, SmartTurnError> {
        let session = Session::builder()
            .map_err(ort_err)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(ort_err)?
            .with_intra_threads(intra_threads)
            .map_err(ort_err)?
            .with_intra_op_spinning(false)
            .map_err(ort_err)?
            .commit_from_memory(model)
            .map_err(ort_err)?;
        Ok(Self {
            session,
            mel: MelFrontEnd::new(),
        })
    }

    /// Probability that the turn is complete, from its 16 kHz audio — up to
    /// 200 ms into the pause, silence included (§6.3, "variant B"); only the
    /// last 8 s count.
    pub fn score(&mut self, audio_16k_turn: &[f32]) -> Result<f32, SmartTurnError> {
        let window = &audio_16k_turn[audio_16k_turn
            .len()
            .saturating_sub(super::mel::WINDOW_SAMPLES)..];
        if !window.iter().all(|x| x.is_finite()) {
            return Err(SmartTurnError::NonFinite);
        }
        let features = self.mel.features(window);
        self.probability(&features)
    }

    /// Runs the model on ready features, `[80, 800]` row-major (what
    /// [`super::mel::whisper_features`] returns).
    pub fn probability(&mut self, features: &[f32]) -> Result<f32, SmartTurnError> {
        if features.len() != FEATURES {
            return Err(SmartTurnError::FeatureLength(features.len()));
        }
        if !features.iter().all(|x| x.is_finite()) {
            return Err(SmartTurnError::NonFinite);
        }
        let input =
            Tensor::from_array(([1usize, N_MELS, FRAMES], features.to_vec())).map_err(ort_err)?;
        let outputs = self
            .session
            .run(ort::inputs!["input_features" => input])
            .map_err(ort_err)?;
        let (_, p) = outputs
            .get("logits")
            .ok_or_else(|| SmartTurnError::BadOutput("no output `logits`".into()))?
            .try_extract_tensor::<f32>()
            .map_err(ort_err)?;
        match p {
            [p] if p.is_finite() => Ok(*p),
            _ => Err(SmartTurnError::BadOutput(format!("{p:?}"))),
        }
    }
}

#[cfg(test)]
mod tests;
