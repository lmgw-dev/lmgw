//! Raw PCM wrapped as a WAV file: the Audio lab's declared-format streams and
//! the Chat's dictation capture both hand the browser one.

/// Wrap raw PCM in the canonical 44-byte RIFF/WAVE header.
///
/// A headless PCM stream carries no format at all, so `rate` / `channels` /
/// `bits` are what the user declared in the form — a declaration, not a
/// measurement. Getting them wrong yields audio at the wrong pitch, never an
/// error, which is why the form says so out loud.
pub fn pcm_to_wav(pcm: &[u8], rate: u32, channels: u16, bits: u16) -> Vec<u8> {
    let bytes_per_sample = (bits / 8) as u32;
    let block_align = (channels as u32).wrapping_mul(bytes_per_sample);
    let data_len = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&data_len.wrapping_add(36).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
                                                 // 3 = IEEE float (f32le), 1 = integer PCM.
    out.extend_from_slice(&if bits == 32 { 3u16 } else { 1u16 }.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&rate.wrapping_mul(block_align).to_le_bytes()); // byte rate
    out.extend_from_slice(&(block_align as u16).to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_byte_exact_for_s16le() {
        let pcm = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let wav = pcm_to_wav(&pcm, 24000, 1, 16);
        assert_eq!(wav.len(), 44 + pcm.len());
        assert_eq!(
            &wav[..44],
            &[
                b'R', b'I', b'F', b'F', // "RIFF"
                44, 0, 0, 0, // 36 + 8 data bytes
                b'W', b'A', b'V', b'E', //
                b'f', b'm', b't', b' ', //
                16, 0, 0, 0, // fmt chunk size
                1, 0, // audioFormat = 1 (integer PCM)
                1, 0, // channels
                0xC0, 0x5D, 0x00, 0x00, // 24000 Hz
                0x80, 0xBB, 0x00, 0x00, // byteRate = 24000 * 2
                2, 0, // blockAlign = channels * 2
                16, 0, // bits per sample
                b'd', b'a', b't', b'a', //
                8, 0, 0, 0, // data length
            ]
        );
        assert_eq!(&wav[44..], &pcm[..]);
    }

    #[test]
    fn wav_header_marks_32_bit_as_ieee_float_and_scales_stereo() {
        let wav = pcm_to_wav(&[0; 16], 48000, 2, 32);
        assert_eq!(&wav[20..22], &[3, 0]); // audioFormat = 3 (f32le)
        assert_eq!(&wav[22..24], &[2, 0]); // channels
        assert_eq!(&wav[24..28], &48000u32.to_le_bytes()); // sample rate
        assert_eq!(&wav[28..32], &(48000u32 * 8).to_le_bytes()); // byte rate
        assert_eq!(&wav[32..34], &[8, 0]); // blockAlign = 2 * 4
        assert_eq!(&wav[34..36], &[32, 0]); // bits
    }
}
