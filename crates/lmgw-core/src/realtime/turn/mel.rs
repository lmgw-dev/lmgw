//! Whisper's log-mel front end, as Smart Turn v3.2 was trained on it
//! (realtime §6.1, §6.3, §13).
//!
//! The model only ever saw `transformers.WhisperFeatureExtractor(chunk_length=8)`
//! on its numpy path (realtime §22: transformers 5.18, numpy only), so this is
//! a transcription of that path, not of OpenAI's torch `log_mel_spectrogram`.
//! The details that move the numbers:
//! - **8 s window, left-padded.** The turn's last 128 000 samples; a shorter
//!   turn gets zeros in *front*, so the end of speech always sits at the end
//!   of the window (§6.3 "Input").
//! - **`do_normalize` before the STFT**, over the padded waveform: zero mean
//!   and unit (population) variance with `sqrt(var + 1e-7)`. The padding
//!   zeros therefore become a small DC offset, not silence. All-zero input
//!   stays finite (the epsilon), it does not divide by zero.
//! - **STFT:** periodic Hann of 400 (`np.hanning(401)[:-1]`), hop 160,
//!   `center=True` with numpy *reflect* padding of 200 on both sides (no
//!   edge repeat), 801 frames of which the last is dropped, giving 800. The
//!   FFT runs in f64 like `np.fft.rfft`; the bins are rounded to complex64,
//!   because transformers stores them in a `complex64` array before taking
//!   the power `|X|^2` in f64.
//! - **Mel filters:** 80 Slaney-scale triangles over 0–8000 Hz on the 201
//!   linear bins, with Slaney area normalisation `2 / (f[m+2] - f[m])`,
//!   built in f64 the way `audio_utils.mel_filter_bank` builds them. Each
//!   triangle covers a few bins, so they are stored sparse.
//! - **Log and range:** `log10(max(mel, 1e-10))`, cast to f32, then
//!   `max(x, max(x) - 8)` over the whole window and `(x + 4) / 4`.
//!
//! The parity test pins all of this against the Python extractor's output
//! for the committed clips: max |Δ| 1.2e-7, one f32 ulp. The window, the
//! plan and the filters are built once ([`MelFrontEnd::new`]);
//! [`whisper_features`] uses a shared one. A window costs ~1.6 ms (release).

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use std::f64::consts::PI;
use std::sync::{Arc, OnceLock};

/// The front end's sample rate.
pub const SAMPLE_RATE: u32 = 16_000;
/// Smart Turn's input length in seconds (its real input size, §6.1).
pub const WINDOW_SECONDS: u32 = 8;
/// Samples in the window (8 s at 16 kHz).
pub const WINDOW_SAMPLES: usize = (WINDOW_SECONDS * SAMPLE_RATE) as usize;
/// FFT length and analysis frame (25 ms).
pub const N_FFT: usize = 400;
/// Hop between frames (10 ms).
pub const HOP: usize = 160;
/// Mel bins.
pub const N_MELS: usize = 80;
/// Frames in the window: `WINDOW_SAMPLES / HOP` (the STFT's 801st is dropped).
pub const FRAMES: usize = WINDOW_SAMPLES / HOP;
/// Values in one feature tensor, `[N_MELS, FRAMES]` row-major.
pub const FEATURES: usize = N_MELS * FRAMES;

const BINS: usize = N_FFT / 2 + 1;
const MEL_FLOOR: f64 = 1e-10;
const NORM_EPS: f64 = 1e-7;
const DYNAMIC_RANGE: f32 = 8.0;

/// One triangular filter: weights for bins `first..first + weights.len()`.
#[derive(Debug, Clone)]
struct Filter {
    first: usize,
    weights: Vec<f64>,
}

/// Precomputed window, FFT plan and mel filters.
pub struct MelFrontEnd {
    window: Vec<f64>,
    filters: Vec<Filter>,
    fft: Arc<dyn RealToComplex<f64>>,
}

impl std::fmt::Debug for MelFrontEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MelFrontEnd")
            .field("n_fft", &N_FFT)
            .field("n_mels", &self.filters.len())
            .finish()
    }
}

impl Default for MelFrontEnd {
    fn default() -> Self {
        Self::new()
    }
}

impl MelFrontEnd {
    pub fn new() -> Self {
        Self {
            window: periodic_hann(N_FFT),
            filters: slaney_filters(),
            fft: RealFftPlanner::<f64>::new().plan_fft_forward(N_FFT),
        }
    }

    /// Features for the last 8 s of `audio_16k` (left-padded with zeros
    /// when shorter): `[N_MELS, FRAMES]`, mel-major, time fastest, ready
    /// for Smart Turn's `input_features [1, 80, 800]`.
    ///
    /// Non-finite samples propagate into the output (one NaN poisons the
    /// whole window through the normalisation); [`SmartTurn::score`]
    /// refuses them before calling this.
    ///
    /// [`SmartTurn::score`]: super::smart_turn::SmartTurn::score
    pub fn features(&self, audio_16k: &[f32]) -> Vec<f32> {
        let x = normalized_window(audio_16k);
        let mut input = self.fft.make_input_vec();
        let mut spectrum = self.fft.make_output_vec();
        let mut scratch = self.fft.make_scratch_vec();
        let mut power = [0.0f64; BINS];
        let mut out = vec![0.0f32; FEATURES];
        for t in 0..FRAMES {
            for (k, slot) in input.iter_mut().enumerate() {
                *slot = f64::from(x[reflect(t * HOP + k)]) * self.window[k];
            }
            // The buffers come from the plan itself, so the lengths match
            // and no input can make this fail.
            self.fft
                .process_with_scratch(&mut input, &mut spectrum, &mut scratch)
                .expect("buffers sized by the plan");
            for (p, c) in power.iter_mut().zip(&spectrum) {
                *p = complex64_power(*c);
            }
            for (m, f) in self.filters.iter().enumerate() {
                let mel: f64 = f
                    .weights
                    .iter()
                    .zip(&power[f.first..])
                    .map(|(w, p)| w * p)
                    .sum();
                out[m * FRAMES + t] = mel.max(MEL_FLOOR).log10() as f32;
            }
        }
        let max = out.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let floor = max - DYNAMIC_RANGE;
        for v in &mut out {
            *v = (v.max(floor) + 4.0) / 4.0;
        }
        out
    }
}

/// [`MelFrontEnd::features`] on a process-wide front end built on first use.
pub fn whisper_features(audio_16k: &[f32]) -> Vec<f32> {
    static FRONT_END: OnceLock<MelFrontEnd> = OnceLock::new();
    FRONT_END.get_or_init(MelFrontEnd::new).features(audio_16k)
}

/// The last [`WINDOW_SAMPLES`] samples, zeros in front if shorter, then
/// `(x - mean) / sqrt(var + 1e-7)` over all of it (transformers'
/// `zero_mean_unit_var_norm`; the mask covers the whole padded window).
/// The moments are taken in f64; numpy's float32 pairwise sums differ by
/// ~1e-7 relative, far below the features' tolerance.
fn normalized_window(audio: &[f32]) -> Vec<f32> {
    let tail = &audio[audio.len().saturating_sub(WINDOW_SAMPLES)..];
    let mut x = vec![0.0f32; WINDOW_SAMPLES - tail.len()];
    x.extend_from_slice(tail);
    let n = WINDOW_SAMPLES as f64;
    let mean = x.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    let var = x
        .iter()
        .map(|&v| (f64::from(v) - mean).powi(2))
        .sum::<f64>()
        / n;
    let (mean, std) = (mean as f32, (var + NORM_EPS).sqrt() as f32);
    for v in &mut x {
        *v = (*v - mean) / std;
    }
    x
}

/// Index into the window for position `i` of the reflect-padded signal
/// (`np.pad(x, 200, mode="reflect")`: `[c b a b c d c b]` for `[a b c d]`).
fn reflect(i: usize) -> usize {
    const PAD: usize = N_FFT / 2;
    const LAST: usize = WINDOW_SAMPLES - 1;
    if i < PAD {
        PAD - i
    } else if i - PAD <= LAST {
        i - PAD
    } else {
        2 * LAST - (i - PAD)
    }
}

/// `|X|^2` of a bin after the round trip through numpy's complex64 array.
fn complex64_power(c: Complex<f64>) -> f64 {
    let (re, im) = (f64::from(c.re as f32), f64::from(c.im as f32));
    re * re + im * im
}

/// `window_function(n, "hann", periodic=True)`: `np.hanning(n + 1)[:-1]`,
/// computed as numpy does (`0.5 + 0.5 * cos(pi * k / n)` for
/// `k = -n, -n + 2, ..`).
fn periodic_hann(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let k = 2.0 * i as f64 - n as f64;
            0.5 + 0.5 * (PI * k / n as f64).cos()
        })
        .collect()
}

/// `hertz_to_mel(f, "slaney")`: linear below 1 kHz, log above.
fn hz_to_mel(f: f64) -> f64 {
    if f >= 1000.0 {
        15.0 + (f / 1000.0).ln() * (27.0 / 6.4f64.ln())
    } else {
        3.0 * f / 200.0
    }
}

/// `mel_to_hertz(m, "slaney")`.
fn mel_to_hz(m: f64) -> f64 {
    if m >= 15.0 {
        1000.0 * ((6.4f64.ln() / 27.0) * (m - 15.0)).exp()
    } else {
        200.0 * m / 3.0
    }
}

/// `mel_filter_bank(201, 80, 0, 8000, 16000, norm="slaney",
/// mel_scale="slaney")`, op for op, kept sparse.
fn slaney_filters() -> Vec<Filter> {
    let max_mel = hz_to_mel(f64::from(SAMPLE_RATE / 2));
    // np.linspace: i * step + start, the last point exactly `stop`.
    let step = max_mel / (N_MELS + 1) as f64;
    let edges: Vec<f64> = (0..N_MELS + 2)
        .map(|i| {
            let m = if i == N_MELS + 1 {
                max_mel
            } else {
                i as f64 * step
            };
            mel_to_hz(m)
        })
        .collect();
    // np.linspace(0, 8000, 201): exactly 40 Hz apart.
    let bin_hz = f64::from(SAMPLE_RATE / 2) / (BINS - 1) as f64;
    (0..N_MELS)
        .map(|m| {
            let enorm = 2.0 / (edges[m + 2] - edges[m]);
            let weights: Vec<f64> = (0..BINS)
                .map(|b| {
                    let f = b as f64 * bin_hz;
                    let down = -(edges[m] - f) / (edges[m + 1] - edges[m]);
                    let up = (edges[m + 2] - f) / (edges[m + 2] - edges[m + 1]);
                    down.min(up).max(0.0) * enorm
                })
                .collect();
            let first = weights.iter().position(|&w| w > 0.0).unwrap_or(0);
            let last = weights.iter().rposition(|&w| w > 0.0).unwrap_or(first);
            Filter {
                first,
                weights: weights[first..=last].to_vec(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
