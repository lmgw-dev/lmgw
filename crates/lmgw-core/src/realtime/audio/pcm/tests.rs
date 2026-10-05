use super::*;

/// Builds a WAV with an arbitrary fmt body, extra chunks and data size.
fn wav(fmt_body: &[u8], extra: &[(&[u8; 4], &[u8])], data: &[u8], data_size: u32) -> Vec<u8> {
    let mut b = b"RIFF\0\0\0\0WAVE".to_vec();
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&(fmt_body.len() as u32).to_le_bytes());
    b.extend_from_slice(fmt_body);
    for (id, body) in extra {
        b.extend_from_slice(*id);
        b.extend_from_slice(&(body.len() as u32).to_le_bytes());
        b.extend_from_slice(body);
        if body.len() % 2 == 1 {
            b.push(0);
        }
    }
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_size.to_le_bytes());
    b.extend_from_slice(data);
    b
}

fn fmt_body(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&tag.to_le_bytes());
    f.extend_from_slice(&channels.to_le_bytes());
    f.extend_from_slice(&rate.to_le_bytes());
    let align = u32::from(channels) * u32::from(bits) / 8;
    f.extend_from_slice(&rate.wrapping_mul(align).to_le_bytes());
    f.extend_from_slice(&(align as u16).to_le_bytes());
    f.extend_from_slice(&bits.to_le_bytes());
    f
}

fn le16(s: &[i16]) -> Vec<u8> {
    s.iter().flat_map(|x| x.to_le_bytes()).collect()
}

#[test]
fn base64_round_trip_and_errors() {
    let s = [0i16, 1, -1, i16::MAX, i16::MIN, 12345];
    let b = encode_pcm16(&s);
    assert_eq!(decode_pcm16(&b).unwrap(), s);
    // Unpadded input is accepted.
    assert_eq!(decode_pcm16(b.trim_end_matches('=')).unwrap(), s);
    assert_eq!(decode_pcm16("").unwrap(), Vec::<i16>::new());
    assert!(matches!(decode_pcm16("!!!!"), Err(PcmError::Base64(_))));
    // Three bytes decode fine as base64 but are not PCM16.
    assert_eq!(decode_pcm16("AAAA"), Err(PcmError::OddByteCount(3)));
}

#[test]
fn float_conversion_round_trips_and_saturates() {
    let s = [0i16, 1, -1, i16::MAX, i16::MIN];
    assert_eq!(f32_to_pcm16(&pcm16_to_f32(&s)), s);
    assert_eq!(
        f32_to_pcm16(&[2.0, -2.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY]),
        [i16::MAX, i16::MIN, 0, i16::MAX, i16::MIN]
    );
}

#[test]
fn writes_and_reads_back_pcm16() {
    let s: Vec<i16> = (0..1000).map(|i| (i * 31 - 15000) as i16).collect();
    let b = write_wav_pcm16_mono(&s, 16_000).unwrap();
    assert_eq!(b.len(), 44 + 2000);
    let w = parse_wav(&b).unwrap();
    assert_eq!((w.rate, w.channels), (16_000, 1));
    assert_eq!(f32_to_pcm16(&w.samples), s);
    assert_eq!(write_wav_pcm16_mono(&s, 0), Err(WavError::ZeroRate));
}

#[test]
fn stereo_downmix_float_and_unknown_chunks() {
    let data = le16(&[1000, 3000, -2000, 2000, 7]); // two frames + a partial one
    let b = wav(
        &fmt_body(1, 2, 44_100, 16),
        &[(b"LIST", &b"abc"[..]), (b"fact", &b"1234"[..])],
        &data,
        data.len() as u32,
    );
    let w = parse_wav(&b).unwrap();
    assert_eq!((w.rate, w.channels), (44_100, 2));
    assert_eq!(f32_to_pcm16(&w.samples), [2000, 0]);

    let fdata: Vec<u8> = [0.5f32, f32::NAN, -0.25]
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .collect();
    let w = parse_wav(&wav(&fmt_body(3, 1, 24_000, 32), &[], &fdata, 12)).unwrap();
    assert_eq!(w.samples, [0.5, 0.0, -0.25]);
}

#[test]
fn extensible_float_is_decoded() {
    let mut f = fmt_body(WAVE_FORMAT_EXTENSIBLE, 1, 48_000, 32);
    f.extend_from_slice(&22u16.to_le_bytes()); // cbSize
    f.extend_from_slice(&32u16.to_le_bytes()); // valid bits
    f.extend_from_slice(&4u32.to_le_bytes()); // channel mask
    f.extend_from_slice(&[3, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113]);
    let w = parse_wav(&wav(&f, &[], &1.0f32.to_le_bytes(), 4)).unwrap();
    assert_eq!((w.rate, w.samples.as_slice()), (48_000, &[1.0f32][..]));
}

#[test]
fn streaming_data_sizes_use_the_remaining_bytes() {
    let data = le16(&[100, 200, 300]);
    for size in [0, u32::MAX, 1_000_000] {
        let w = parse_wav(&wav(&fmt_body(1, 1, 24_000, 16), &[], &data, size)).unwrap();
        assert_eq!(f32_to_pcm16(&w.samples), [100, 200, 300], "size {size}");
    }
}

#[test]
fn hostile_headers_are_errors_not_panics() {
    let data = le16(&[1, 2]);
    let ok = fmt_body(1, 1, 24_000, 16);
    assert_eq!(parse_wav(b"RIFF"), Err(WavError::NotWave));
    assert_eq!(parse_wav(b"RIFF\0\0\0\0AVI "), Err(WavError::NotWave));
    assert_eq!(
        parse_wav(&wav(&fmt_body(1, 0, 24_000, 16), &[], &data, 4)),
        Err(WavError::ZeroChannels)
    );
    assert_eq!(
        parse_wav(&wav(&fmt_body(1, 1, 0, 16), &[], &data, 4)),
        Err(WavError::ZeroRate)
    );
    assert_eq!(
        parse_wav(&wav(&fmt_body(1, 1, 24_000, 24), &[], &data, 4)),
        Err(WavError::Unsupported {
            format_tag: 1,
            bits: 24
        })
    );
    assert_eq!(
        parse_wav(&wav(&ok[..10], &[], &data, 4)),
        Err(WavError::FmtTooShort(10))
    );
    // A chunk claiming 4 GiB is refused, not allocated.
    let mut b = b"RIFF\0\0\0\0WAVE".to_vec();
    b.extend_from_slice(b"junk\xfe\xff\xff\xff");
    assert!(matches!(parse_wav(&b), Err(WavError::Truncated { .. })));
    // No fmt, no data.
    let mut b = b"RIFF\0\0\0\0WAVE".to_vec();
    b.extend_from_slice(b"data\x02\0\0\0\x01\0");
    assert_eq!(parse_wav(&b), Err(WavError::MissingFmt));
    let mut b = b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0".to_vec();
    b.extend_from_slice(&ok);
    assert_eq!(parse_wav(&b), Err(WavError::MissingData));
    // 65535 channels with two bytes of audio: zero frames, no panic.
    let w = parse_wav(&wav(&fmt_body(1, u16::MAX, 8_000, 16), &[], &data, 4)).unwrap();
    assert!(w.samples.is_empty());
}

#[test]
fn reads_the_committed_fixture() {
    let b = std::fs::read(crate::realtime::test_fixtures::path(
        "en_two_sentences_pause.wav",
    ))
    .unwrap();
    let w = parse_wav(&b).unwrap();
    assert_eq!((w.rate, w.channels, w.samples.len()), (24_000, 1, 87_584));
}
