//! Sample-rate conversion with rubato 5 (realtime §4.1, §8.2, §13).
//!
//! Two jobs, one engine:
//! - **[`StreamResampler`]**: the session's 24 kHz input to Silero's 16 kHz,
//!   fed whatever chunk sizes `input_audio_buffer.append` brings. The
//!   filter's startup delay is trimmed, so output sample `j` stands for
//!   input sample `1.5 j` (within one output sample) and VAD frames map
//!   straight onto the session's input timeline; what remains is latency,
//!   reported by [`StreamResampler::delay_input_samples`].
//! - **[`resample`]**: whole clips (TTS output at the model's native rate →
//!   the session's 24 kHz). Equal rates pass through.
//!
//! Both use WP0's async sinc (256 taps, BlackmanHarris2, cubic). For the
//! fixed 3:2 stream a synchronous FFT resampler (768 → 512, one Silero frame
//! per block) was measured as the alternative and lost:
//! - against the soxr-VHQ Silero reference it deviates 0.088 at worst,
//!   the sinc 0.018 (neither flips a 0.5 crossing);
//! - its filter delay is 16 ms against the sinc's 5 ms;
//! - its block is `rate / gcd`, which a coprime rate in a hostile WAV
//!   header blows up, so the one-shot path needs the sinc anyway.
//!
//! The worst frame is always an end-of-speech transition and is sensitive
//! to the filter (larger FFT blocks: 0.20 and 0.28, the latter flipping the
//! crossing), so the WP0 sinc is kept as is rather than tuned to the
//! reference.
//!
//! **Not `process_all`.** rubato 5.0.0's `process_all_into_buffer` trims
//! the startup delay with `copy_frames_within(delay, 0, delay)`, moving
//! `delay` frames instead of `output_len - delay`, which corrupts the first
//! block of every clip (a repeated stretch plus a gap). Both paths here run
//! the same block loop, `Blocks`, which trims correctly. It also trims
//! the *measured* delay: the sinc's first output sits at input position
//! `1/ratio - 128`, so the delay is `128 * ratio - 1` output samples, one
//! less than (and unrounded unlike) rubato's `output_delay()`. Rounding
//! leaves the output within half a sample of the input timeline.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::fmt;

/// The session's input rate (realtime §2.2: `audio/pcm` at 24 kHz).
pub const INPUT_RATE: u32 = 24_000;
/// Silero's rate (realtime §6.1).
pub const VAD_RATE: u32 = 16_000;
/// Input samples per streaming block: 10 ms, so block alignment adds at
/// most 10 ms on top of the filter delay.
const STREAM_BLOCK: usize = 240;
/// Output samples per block for whole clips. Blocks are sized by output so
/// a block's scratch memory does not grow with the ratio a header claims
/// (1 Hz → 24 kHz with a fixed 1024-sample input block is 98 MB per block).
const CLIP_BLOCK_OUT: u64 = 1024;
/// WP0's sinc length.
const SINC_LEN: usize = 256;

/// Resampling errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResampleError {
    /// A rate of zero.
    ZeroRate,
    /// The output would exceed the caller's bound (`needed` samples).
    OutputTooLong { needed: u64, max: usize },
    /// rubato refused; carries its message.
    Rubato(String),
}

impl fmt::Display for ResampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroRate => write!(f, "sample rate must not be zero"),
            Self::OutputTooLong { needed, max } => write!(
                f,
                "resampled audio would be {needed} samples, over the limit of {max}"
            ),
            Self::Rubato(e) => write!(f, "resampler: {e}"),
        }
    }
}

impl std::error::Error for ResampleError {}

fn rubato_err<E: fmt::Display>(e: E) -> ResampleError {
    ResampleError::Rubato(e.to_string())
}

/// A delay-trimming block loop around WP0's async sinc (realtime §13):
/// 256 taps, BlackmanHarris2, cubic. Its memory does not depend on the
/// rates, only on the block.
struct Blocks {
    inner: Async<f32>,
    from: u32,
    to: u32,
    pending: Vec<f32>,
    scratch: Vec<f32>,
    /// Output samples of startup delay still to drop.
    to_skip: usize,
    pushed: u64,
    emitted: u64,
}

impl Blocks {
    fn new(from: u32, to: u32, block: usize) -> Result<Self, ResampleError> {
        if from == 0 || to == 0 {
            return Err(ResampleError::ZeroRate);
        }
        let params = SincInterpolationParameters {
            sinc_len: SINC_LEN,
            f_cutoff: None,
            oversampling_factor: 256,
            interpolation: SincInterpolationType::Cubic,
            window: WindowFunction::BlackmanHarris2,
        };
        let ratio = f64::from(to) / f64::from(from);
        let inner = Async::<f32>::new_sinc(ratio, 1.0, &params, block, 1, FixedAsync::Input)
            .map_err(rubato_err)?;
        Ok(Self {
            inner,
            from,
            to,
            pending: Vec::new(),
            scratch: Vec::new(),
            to_skip: trimmed_delay(from, to),
            pushed: 0,
            emitted: 0,
        })
    }

    fn push(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<(), ResampleError> {
        self.pending.extend_from_slice(input);
        self.pushed += input.len() as u64;
        self.drain(out)
    }

    fn drain(&mut self, out: &mut Vec<f32>) -> Result<(), ResampleError> {
        let mut start = 0;
        loop {
            let need = self.inner.input_frames_next();
            if self.pending.len() - start < need {
                break;
            }
            self.scratch.resize(self.inner.output_frames_next(), 0.0);
            let n_out = self.scratch.len();
            let input = InterleavedSlice::new(&self.pending[start..start + need], 1, need)
                .map_err(rubato_err)?;
            let mut output =
                InterleavedSlice::new_mut(&mut self.scratch, 1, n_out).map_err(rubato_err)?;
            let (used, made) = self
                .inner
                .process_into_buffer(&input, &mut output, None)
                .map_err(rubato_err)?;
            start += used;
            let skip = self.to_skip.min(made);
            self.to_skip -= skip;
            out.extend_from_slice(&self.scratch[skip..made]);
            self.emitted += (made - skip) as u64;
        }
        self.pending.drain(..start);
        Ok(())
    }

    /// Exact output length for everything pushed: `ceil(pushed * to / from)`.
    fn expected(&self) -> u64 {
        let n = (u128::from(self.pushed) * u128::from(self.to)).div_ceil(u128::from(self.from));
        u64::try_from(n).unwrap_or(u64::MAX)
    }

    fn flush(&mut self, out: &mut Vec<f32>) -> Result<(), ResampleError> {
        let expected = self.expected();
        let before = self.emitted;
        let start_len = out.len();
        while self.emitted < expected {
            let zeros = vec![0.0; self.inner.input_frames_next()];
            self.pending.extend_from_slice(&zeros);
            self.drain(out)?;
        }
        out.truncate(start_len + expected.saturating_sub(before) as usize);
        self.reset();
        Ok(())
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.pending.clear();
        self.to_skip = trimmed_delay(self.from, self.to);
        self.pushed = 0;
        self.emitted = 0;
    }
}

/// Output samples to drop at the start: `round(SINC_LEN/2 * to/from - 1)`,
/// computed exactly in integers (see the module docs).
fn trimmed_delay(from: u32, to: u32) -> usize {
    let (from, to) = (u64::from(from), u64::from(to));
    ((SINC_LEN as u64 * to + from) / (2 * from)).saturating_sub(1) as usize
}

/// Streaming 24 kHz → 16 kHz for the detector, aligned to the input timeline.
pub struct StreamResampler(Blocks);

impl StreamResampler {
    pub fn new() -> Result<Self, ResampleError> {
        Blocks::new(INPUT_RATE, VAD_RATE, STREAM_BLOCK).map(Self)
    }

    /// Feeds any number of input samples and appends every output sample
    /// now complete to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<(), ResampleError> {
        self.0.push(input, out)
    }

    /// Ends the stream: pads with silence until the output covers every
    /// pushed sample, appends exactly that tail, and resets.
    pub fn flush(&mut self, out: &mut Vec<f32>) -> Result<(), ResampleError> {
        self.0.flush(out)
    }

    /// Forgets all input; the next sample starts a new aligned stream.
    pub fn reset(&mut self) {
        self.0.reset();
    }

    /// The filter's delay in input samples (126 ≈ 5.3 ms). It is trimmed
    /// from the output's indices and shows up only as latency: output for
    /// input sample `n` exists once `n + delay` samples, rounded up to a
    /// whole [`block_input_samples`](Self::block_input_samples), arrived.
    pub fn delay_input_samples(&self) -> u64 {
        let out = trimmed_delay(INPUT_RATE, VAD_RATE) as u64;
        (out * u64::from(INPUT_RATE)).div_ceil(u64::from(VAD_RATE))
    }

    /// Input samples consumed per processing block (240 = 10 ms).
    pub fn block_input_samples(&self) -> usize {
        self.0.inner.input_frames_next()
    }

    /// The input-timeline sample that output sample `out_index` stands for.
    pub fn output_to_input(out_index: u64) -> u64 {
        out_index * u64::from(INPUT_RATE) / u64::from(VAD_RATE)
    }

    /// Output samples emitted since the start or the last reset.
    pub fn emitted(&self) -> u64 {
        self.0.emitted
    }
}

/// Resamples a whole mono clip from `from` Hz to `to` Hz (TTS output to the
/// session rate, realtime §8.2). Equal rates pass through. The result has
/// exactly `ceil(len * to / from)` samples.
///
/// `max_output_len` is the caller's real bound on the result: a WAV header
/// claiming 1 Hz would otherwise turn a small body into a huge allocation.
/// Exceeding it is an error, never a truncation.
pub fn resample(
    samples: &[f32],
    from: u32,
    to: u32,
    max_output_len: usize,
) -> Result<Vec<f32>, ResampleError> {
    if from == 0 || to == 0 {
        return Err(ResampleError::ZeroRate);
    }
    let needed = (samples.len() as u128 * u128::from(to)).div_ceil(u128::from(from));
    if needed > max_output_len as u128 {
        return Err(ResampleError::OutputTooLong {
            needed: u64::try_from(needed).unwrap_or(u64::MAX),
            max: max_output_len,
        });
    }
    if from == to || samples.is_empty() {
        return Ok(samples.to_vec());
    }
    // Input block for ~CLIP_BLOCK_OUT output samples, never beyond the clip.
    let block = (CLIP_BLOCK_OUT * u64::from(from))
        .div_ceil(u64::from(to))
        .clamp(1, samples.len() as u64) as usize;
    let mut blocks = Blocks::new(from, to, block)?;
    let mut out = Vec::with_capacity(needed as usize);
    blocks.push(samples, &mut out)?;
    blocks.flush(&mut out)?;
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests;
