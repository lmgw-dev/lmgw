//! What the audio tests share: a voice-like signal and its Opus stream.

use opusic_c::{Application, Bitrate, Channels, Encoder, InbandFec, SampleRate, Signal};

use super::{FRAME_SAMPLES, PCM_RATE};

/// `frames` 20 ms frames of a voiced, vowel-like signal at 24 kHz: eight
/// harmonics of a pitch that glides between 80 and 200 Hz, under a syllable
/// envelope. The pitch moves frame to frame, so concealment, which carries
/// the last pitch on, differs from what was sent.
pub(crate) fn voice(frames: usize) -> Vec<i16> {
    let n = frames * FRAME_SAMPLES;
    let rate = f64::from(PCM_RATE);
    let mut phase = 0.0f64;
    (0..n)
        .map(|i| {
            let t = i as f64 / rate;
            let f0 = 140.0 + 60.0 * (std::f64::consts::TAU * 1.3 * t).sin();
            phase += std::f64::consts::TAU * f0 / rate;
            let env = 0.55 + 0.45 * (std::f64::consts::TAU * 3.0 * t).sin();
            let s: f64 = (1..=8).map(|h| (phase * h as f64).sin() / h as f64).sum();
            (s * env * 6000.0) as i16
        })
        .collect()
}

/// `pcm` encoded frame by frame as a phone's libwebrtc would: VoIP, 32
/// kbps, in-band FEC on with 20 % expected loss, so every packet carries the
/// frame before it.
pub(crate) fn opus_stream(pcm: &[i16]) -> Vec<Vec<u8>> {
    let mut enc = Encoder::new(Channels::Mono, SampleRate::Hz24000, Application::Voip).unwrap();
    enc.set_bitrate(Bitrate::Value(32_000)).unwrap();
    enc.set_inband_fec(InbandFec::Mode1).unwrap();
    enc.set_packet_loss(20).unwrap();
    enc.set_signal(Signal::Voice).unwrap();
    pcm.chunks(FRAME_SAMPLES)
        .map(|f| {
            let input: Vec<u16> = f.iter().map(|&s| s as u16).collect();
            let mut out = vec![0u8; 1275];
            let n = enc.encode_to_slice(&input, &mut out).unwrap();
            out.truncate(n);
            out
        })
        .collect()
}

/// The root mean square of `pcm`.
pub(crate) fn rms(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / pcm.len() as f64).sqrt()
}

/// The root mean square of `a − b`.
pub(crate) fn error(a: &[i16], b: &[i16]) -> f64 {
    assert_eq!(a.len(), b.len());
    let d: Vec<i16> = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| (i32::from(x) - i32::from(y)).clamp(-32768, 32767) as i16)
        .collect();
    rms(&d)
}
