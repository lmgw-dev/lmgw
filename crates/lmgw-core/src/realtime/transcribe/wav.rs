//! A committed turn's upload (voice-audio-input design §3.1): the 24 kHz
//! segment, or — for a turn the chat model hears — the 16 kHz WAV built
//! once at the commit and shared by the ASR call, the word check's second
//! call and the request ([`Wav`]).
//!
//! The WAV is built on the blocking pool the moment the turn commits
//! (`wav_16k`, ~8 ms for a few seconds of speech), so neither reader waits
//! for the other. It lives in memory only, for as long as a reader holds
//! it: the turn's ASR job, and the core's `hearing` until the transcript is
//! in (§4: nothing is stored).

use bytes::Bytes;
use futures::future::{BoxFuture, FutureExt, Shared};

use super::wav_16k;
use crate::error::GatewayError;

/// The turn's 16 kHz mono PCM16 WAV, being built or built (module doc).
/// Cloning shares it; every clone answers the same bytes, or the same
/// error.
#[derive(Clone)]
pub(crate) struct Wav(Shared<BoxFuture<'static, Result<Bytes, GatewayError>>>);

impl Wav {
    /// Start building the WAV of `samples` (the 24 kHz segment) now, on the
    /// blocking pool.
    pub fn build(samples: Vec<i16>) -> Self {
        let job = tokio::task::spawn_blocking(move || wav_16k(&samples));
        let fut = async move {
            job.await
                .map_err(|e| {
                    GatewayError::Internal(format!("building the turn's WAV failed: {e}"))
                })?
                .map(Bytes::from)
        };
        Self(fut.boxed().shared())
    }

    /// The WAV, once built.
    pub async fn get(&self) -> Result<Bytes, GatewayError> {
        self.0.clone().await
    }
}

/// What a turn's ASR call uploads: the segment, made into a WAV by the call
/// (the transcript path), or the WAV already being built (§3.1).
#[derive(Clone)]
pub(crate) enum Upload {
    Samples(Vec<i16>),
    Wav(Wav),
}

impl From<Vec<i16>> for Upload {
    fn from(samples: Vec<i16>) -> Self {
        Self::Samples(samples)
    }
}

impl From<Wav> for Upload {
    fn from(wav: Wav) -> Self {
        Self::Wav(wav)
    }
}

impl Upload {
    /// The WAV the call sends: built now from the segment, or the shared one.
    pub async fn wav(self) -> Result<Bytes, GatewayError> {
        match self {
            Self::Samples(samples) => tokio::task::spawn_blocking(move || wav_16k(&samples))
                .await
                .map_err(|e| {
                    GatewayError::Internal(format!("building the ASR upload failed: {e}"))
                })?
                .map(Bytes::from),
            Self::Wav(wav) => wav.get().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_wav_is_built_once_and_every_clone_reads_it() {
        let samples: Vec<i16> = (0..2400).map(|i| (i % 100) as i16 * 50).collect();
        let wav = Wav::build(samples.clone());
        let other = wav.clone();
        let (a, b) = (wav.get().await.unwrap(), other.get().await.unwrap());
        assert_eq!(a, b);
        // The same bytes the transcript path builds from the segment.
        let built = Upload::from(samples).wav().await.unwrap();
        assert_eq!(a, built);
        assert_eq!(&a[..4], b"RIFF");
        // The shared handle hands out the same buffer, not a copy.
        assert_eq!(a.as_ptr(), b.as_ptr());
    }
}
