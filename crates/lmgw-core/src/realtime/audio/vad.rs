//! Silero VAD on ONNX Runtime: a speech probability per 32 ms frame at
//! 16 kHz (realtime §6.1).
//!
//! Why it looks like this:
//! - **Context is mandatory (WP0).** Silero v6.2.3 expects the previous 64
//!   samples prepended to each 512-sample frame (input `[1, 576]`). Without
//!   them the model is dead (p < 0.06 on every speech clip); the earlier
//!   prototype's bare 512 path only worked on an older graph. [`Vad`] carries the context and
//!   resets it to zeros together with the recurrent state.
//! - **One session per stream.** `ort::Session::run` takes `&mut self`, and
//!   the state is per stream anyway. One intra-op thread (realtime §13): a
//!   frame costs ~0.1 ms, so the VAD runs inline on the session's task.
//! - **The op15 16 kHz graph** (`assets/realtime/silero_vad_16k_op15.onnx`,
//!   1.3 MB, bit-identical to the stock file at 16 kHz). It accepts at most
//!   576 samples per call because of an internal `If` (realtime §22), which
//!   is exactly one frame plus context.
//! - **Non-finite input is refused**, not fed: one NaN would poison the
//!   recurrent state for the rest of the session.
//!
//! [`Framer`] re-blocks arbitrary chunks into frames, and
//! [`level_normalize`] is the earlier prototype's boost-only normalizer for
//! quiet microphones (VAD input only; ASR never sees it).

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use std::fmt;

/// Silero's sample rate.
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples per frame (32 ms at 16 kHz).
pub const FRAME: usize = 512;
/// Frame length in milliseconds.
pub const FRAME_MS: u32 = 32;
/// Samples of the previous frame prepended to each call (WP0).
pub const CONTEXT: usize = 64;
const STATE_LEN: usize = 2 * 128;

/// The committed model (realtime §13); in lmgw-core it lives at
/// `crates/lmgw-core/assets/realtime/` next to its MIT licence.
pub static SILERO_VAD_ONNX: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/realtime/silero_vad_16k_op15.onnx"
));

/// VAD errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VadError {
    /// ONNX Runtime refused the model or the run.
    Ort(String),
    /// A frame must be exactly [`FRAME`] samples.
    FrameLength(usize),
    /// The frame holds NaN or an infinity; the state was left untouched.
    NonFinite,
    /// The model answered with an unexpected output; the state was reset.
    BadOutput(String),
}

impl fmt::Display for VadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ort(e) => write!(f, "Silero VAD: {e}"),
            Self::FrameLength(n) => write!(f, "Silero VAD needs {FRAME}-sample frames, got {n}"),
            Self::NonFinite => write!(f, "Silero VAD frame holds a non-finite sample"),
            Self::BadOutput(e) => write!(f, "Silero VAD returned an unexpected output: {e}"),
        }
    }
}

impl std::error::Error for VadError {}

// `ort::Error<SessionBuilder>` is not `Send`, so it is flattened to text.
fn ort_err<E: fmt::Display>(e: E) -> VadError {
    VadError::Ort(e.to_string())
}

/// One Silero stream: model session, recurrent state and 64-sample context.
pub struct Vad {
    session: Session,
    state: Vec<f32>,
    context: [f32; CONTEXT],
}

impl Vad {
    /// Loads the graph from memory with one intra-op thread (realtime §13).
    /// The bytes are copied; pass [`SILERO_VAD_ONNX`] for the committed model.
    pub fn from_bytes(model: &[u8]) -> Result<Self, VadError> {
        let session = Session::builder()
            .map_err(ort_err)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(ort_err)?
            .with_intra_threads(1)
            .map_err(ort_err)?
            .commit_from_memory(model)
            .map_err(ort_err)?;
        Ok(Self {
            session,
            state: vec![0.0; STATE_LEN],
            context: [0.0; CONTEXT],
        })
    }

    /// Speech probability in `[0, 1]` for the next [`FRAME`] samples at
    /// 16 kHz, threading state and context forward.
    pub fn probability(&mut self, frame: &[f32]) -> Result<f32, VadError> {
        if frame.len() != FRAME {
            return Err(VadError::FrameLength(frame.len()));
        }
        if !frame.iter().all(|x| x.is_finite()) {
            return Err(VadError::NonFinite);
        }
        let mut input = Vec::with_capacity(CONTEXT + FRAME);
        input.extend_from_slice(&self.context);
        input.extend_from_slice(frame);
        let outputs = self
            .session
            .run(ort::inputs![
                "input" => Tensor::from_array(([1usize, CONTEXT + FRAME], input)).map_err(ort_err)?,
                "state" => Tensor::from_array(([2usize, 1, 128], self.state.clone())).map_err(ort_err)?,
                "sr" => Tensor::from_array(((), vec![i64::from(SAMPLE_RATE)])).map_err(ort_err)?,
            ])
            .map_err(ort_err)?;
        // `get`, not indexing: a graph without these outputs is an error,
        // not a panic.
        let output = |name: &str| {
            outputs
                .get(name)
                .ok_or_else(|| VadError::BadOutput(format!("no output `{name}`")))?
                .try_extract_tensor::<f32>()
                .map_err(ort_err)
        };
        let (_, prob) = output("output")?;
        let (_, state) = output("stateN")?;
        let p = prob.first().copied();
        let state_ok = state.len() == STATE_LEN && state.iter().all(|x| x.is_finite());
        match p {
            Some(p) if p.is_finite() && state_ok => {
                self.state.copy_from_slice(state);
                self.context.copy_from_slice(&frame[FRAME - CONTEXT..]);
                Ok(p)
            }
            _ => {
                let what = format!("output {p:?}, state of {} values", state.len());
                drop(outputs);
                self.reset();
                Err(VadError::BadOutput(what))
            }
        }
    }

    /// Clears the recurrent state and the context together (WP0): a new
    /// stream, `input_audio_buffer.clear`, or after an error.
    pub fn reset(&mut self) {
        self.state.fill(0.0);
        self.context = [0.0; CONTEXT];
    }
}

/// Re-blocks arbitrary chunks into [`FRAME`]-sample frames; the remainder
/// waits for the next push.
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<f32>,
}

impl Framer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `block` and returns every complete frame now available.
    pub fn push(&mut self, block: &[f32]) -> Vec<[f32; FRAME]> {
        self.buf.extend_from_slice(block);
        let (full, rest) = self.buf.as_chunks::<FRAME>();
        let frames = full.to_vec();
        let consumed = self.buf.len() - rest.len();
        self.buf.drain(..consumed);
        frames
    }

    /// Samples waiting for the next frame (always `< FRAME` after a push).
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Drops the partial frame.
    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

/// The earlier prototype's boost-only level normalizer (realtime §6.1): lifts a frame's
/// peak towards 0.4 with at most 32x gain, never attenuates, and leaves
/// frames whose peak is under 0.008 (silence) alone. VAD input only.
pub fn level_normalize(frame: &[f32]) -> Vec<f32> {
    const FLOOR: f32 = 0.008;
    const TARGET: f32 = 0.4;
    const MAX_GAIN: f32 = 32.0;
    // `f32::max` ignores NaN, so a NaN sample cannot become the peak.
    let peak = frame.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    if peak < FLOOR {
        return frame.to_vec();
    }
    let gain = (TARGET / peak).clamp(1.0, MAX_GAIN);
    frame.iter().map(|x| x * gain).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realtime::audio::resample::tests::stream;
    use crate::realtime::audio::resample::StreamResampler;
    use crate::realtime::test_fixtures::{f32le, silero_csv, wav_24k};

    fn probs(vad: &mut Vad, audio: &[f32]) -> Vec<f32> {
        audio
            .as_chunks::<FRAME>()
            .0
            .iter()
            .map(|f| vad.probability(f).unwrap())
            .collect()
    }

    /// Exact inputs (the soxr-resampled `audio16k` dumps): the Rust path
    /// must reproduce the Python reference to ~1e-3 on every frame.
    #[test]
    fn matches_reference_on_exact_inputs() {
        let mut vad = Vad::from_bytes(SILERO_VAD_ONNX).unwrap();
        for name in ["en_complete_short", "en_midsentence_pause"] {
            vad.reset();
            let got = probs(
                &mut vad,
                &f32le(&format!("smartturn_{name}.audio16k_f32le.bin")),
            );
            let want = silero_csv(&format!("silero_smartturn_{name}_cutaudio.csv"));
            assert_eq!(got.len(), want.len(), "{name}: frame count");
            let max = got
                .iter()
                .zip(&want)
                .map(|(g, w)| (g - w).abs())
                .fold(0.0f32, f32::max);
            assert!(max <= 1e-3, "{name}: max |dp| {max}");
        }
    }

    /// Frames where the 0.5 decision changes.
    fn edges(p: &[f32]) -> Vec<usize> {
        (1..p.len())
            .filter(|&i| (p[i] >= 0.5) != (p[i - 1] >= 0.5))
            .collect()
    }

    /// The full-file CSVs start from a soxr-VHQ resample; ours is rubato's
    /// sinc. Every frame agrees to the README's ~0.02 (the transition
    /// frames, reference in 0.1..0.9, are the resampler-sensitive ones, see
    /// `resample`), and every 0.5 crossing lands on the same frame.
    #[test]
    fn matches_reference_after_own_resampling() {
        for name in ["en_two_sentences_pause", "noise_only"] {
            let x = wav_24k(&format!("{name}.wav"));
            let y = stream(&mut StreamResampler::new().unwrap(), &x);
            let got = probs(&mut Vad::from_bytes(SILERO_VAD_ONNX).unwrap(), &y);
            let want = silero_csv(&format!("silero_{name}.csv"));
            assert_eq!(got.len(), want.len(), "{name}: frame count");
            let (mut settled, mut transition) = (0.0f32, 0.0f32);
            for (g, w) in got.iter().zip(&want) {
                let d = (g - w).abs();
                if (0.1..=0.9).contains(w) {
                    transition = transition.max(d);
                } else {
                    settled = settled.max(d);
                }
            }
            eprintln!("{name}: max |dp| settled {settled:.4}, transition {transition:.4}");
            assert!(
                settled.max(transition) <= 0.02,
                "{name}: {settled} / {transition}"
            );
            assert_eq!(edges(&got), edges(&want), "{name}: 0.5 crossings");
        }
    }

    #[test]
    fn reset_restores_a_fresh_stream() {
        let audio = f32le("smartturn_en_complete_short.audio16k_f32le.bin");
        let mut vad = Vad::from_bytes(SILERO_VAD_ONNX).unwrap();
        let first = probs(&mut vad, &audio[..FRAME * 20]);
        vad.reset();
        assert_eq!(probs(&mut vad, &audio[..FRAME * 20]), first);
    }

    #[test]
    fn rejects_bad_frames_without_touching_state() {
        let mut vad = Vad::from_bytes(SILERO_VAD_ONNX).unwrap();
        assert_eq!(
            vad.probability(&[0.0; 100]),
            Err(VadError::FrameLength(100))
        );
        let mut f = [0.0f32; FRAME];
        f[7] = f32::NAN;
        assert_eq!(vad.probability(&f), Err(VadError::NonFinite));
        let p = vad.probability(&[0.0; FRAME]).unwrap();
        assert!(
            (0.0..0.1).contains(&p),
            "silence after rejected frames: {p}"
        );
        assert!(matches!(
            Vad::from_bytes(b"not a model"),
            Err(VadError::Ort(_))
        ));
    }

    #[test]
    fn framer_carries_the_remainder() {
        let mut fr = Framer::new();
        assert!(fr.push(&[1.0; 300]).is_empty());
        let frames = fr.push(&[2.0; 1000]);
        assert_eq!(frames.len(), 2);
        assert_eq!((frames[0][299], frames[0][300]), (1.0, 2.0));
        assert_eq!(fr.pending(), 1300 - 2 * FRAME);
        fr.reset();
        assert_eq!(fr.pending(), 0);
    }

    #[test]
    fn level_normalize_only_boosts() {
        assert_eq!(level_normalize(&[0.001, -0.007]), [0.001, -0.007]);
        let quiet = level_normalize(&[0.01, -0.005]);
        assert!(
            (quiet[0] - 0.32).abs() < 1e-6,
            "gain capped at 32: {quiet:?}"
        );
        assert_eq!(level_normalize(&[0.9, -0.5]), [0.9, -0.5]);
        let mid = level_normalize(&[0.1, -0.05, f32::NAN]);
        assert!((mid[0] - 0.4).abs() < 1e-6 && (mid[1] + 0.2).abs() < 1e-6);
    }
}
