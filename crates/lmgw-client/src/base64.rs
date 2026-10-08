//! Standard-alphabet base64, as the realtime protocol carries audio:
//! `input_audio_buffer.append` and `response.output_audio.delta`.

/// Standard-alphabet base64 → bytes. Whitespace is tolerated (SSE data lines
/// can wrap); padding ends the payload; anything else is a decode failure.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    fn sextet(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        if c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | sextet(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Bytes → standard-alphabet base64 with padding, the inverse of
/// [`decode`].
pub fn encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.len();
        let v = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        for i in 0..4 {
            if i <= n {
                out.push(ALPHABET[(v >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// PCM16 samples as the little-endian bytes the protocol carries.
pub fn pcm16_le_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Little-endian PCM16 bytes as samples; an odd last byte is dropped.
pub fn pcm16_samples(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_the_shapes_audio_cpp_sends() {
        assert_eq!(decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(decode("QQ==").unwrap(), b"A");
        assert_eq!(decode("QUI=").unwrap(), b"AB");
        assert_eq!(decode("QUJD").unwrap(), b"ABC");
        assert_eq!(decode("QUJD\nREVG").unwrap(), b"ABCDEF");
        assert_eq!(decode("//8=").unwrap(), vec![0xFF, 0xFF]);
        assert!(decode("not*base64").is_none());
    }

    #[test]
    fn base64_encodes_with_padding_and_round_trips() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"A"), "QQ==");
        assert_eq!(encode(b"AB"), "QUI=");
        assert_eq!(encode(b"ABC"), "QUJD");
        assert_eq!(encode(&[0xFF, 0xFF]), "//8=");
        assert_eq!(encode(&[0xFB, 0xEF, 0xBE]), "++++");
        // Every byte value at every offset within a group of three.
        let bytes: Vec<u8> = (0..=255u8).chain((0..=255u8).rev()).collect();
        for len in [0, 1, 2, 3, 4, 5, 511, 512] {
            let b = &bytes[..len];
            assert_eq!(decode(&encode(b)).unwrap(), b, "length {len}");
        }
    }

    #[test]
    fn pcm16_is_little_endian_both_ways() {
        let s = [0i16, 1, -1, i16::MAX, i16::MIN];
        let b = pcm16_le_bytes(&s);
        assert_eq!(&b[..6], &[0, 0, 1, 0, 0xFF, 0xFF]);
        assert_eq!(pcm16_samples(&b), s);
        assert_eq!(pcm16_samples(&[1, 0, 7]), vec![1], "an odd byte is dropped");
        // What `input_audio_buffer.append` carries.
        assert_eq!(
            decode(&encode(&pcm16_le_bytes(&[1, -2, 300]))).unwrap(),
            vec![1, 0, 0xFE, 0xFF, 0x2C, 0x01]
        );
    }
}
