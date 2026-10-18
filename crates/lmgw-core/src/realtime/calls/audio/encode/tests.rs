use super::*;
use crate::realtime::calls::audio::fixtures::voice;

const F: usize = FRAME_SAMPLES;

fn sent(e: Encoded) -> (Vec<u8>, bool) {
    match e {
        Encoded::Send { payload, talkspurt } => (payload, talkspurt),
        Encoded::Skip => panic!("skipped"),
    }
}

#[test]
fn the_settings_take_effect_in_libopus() {
    let b = bitrate(32, None);
    assert_eq!(
        b,
        OutputBitrate {
            bps: 32_000,
            capped_by: None,
            ignored: None
        }
    );
    let mut e = Encoder::new(b, false).unwrap();
    assert_eq!(
        e.settings(),
        (Bitrate::Value(32_000), false, InbandFec::Mode1, 0)
    );
    // The phone reports 12 % loss: FEC expects it.
    e.loss(0.12).unwrap();
    assert_eq!(e.settings().3, 12);
    e.loss(3.0).unwrap();
    assert_eq!(e.settings().3, 100, "a fraction, at most all");
    let mut e = Encoder::new(bitrate(32, None), true).unwrap();
    assert!(e.settings().1, "DTX on usedtx=1");
    // The phone's maxaveragebitrate caps the setting, and says so.
    let capped = bitrate(32, Some(20_000));
    assert_eq!(
        capped,
        OutputBitrate {
            bps: 20_000,
            capped_by: Some(20_000),
            ignored: None
        }
    );
    assert!(
        capped.to_string().contains("maxaveragebitrate 20000"),
        "{capped}"
    );
    assert_eq!(bitrate(32, Some(64_000)).capped_by, None);
    // Outside RFC 7587's 6000 to 510000 the phone's value caps nothing (0
    // would make no encoder, and the call silent): the setting's own, the
    // value named as ignored. The range's ends cap as any value does.
    for v in [0, 1, 5_999, 510_001] {
        let b = bitrate(32, Some(v));
        assert_eq!(
            b,
            OutputBitrate {
                bps: 32_000,
                capped_by: None,
                ignored: Some(v)
            }
        );
        assert!(
            b.to_string()
                .contains(&format!("maxaveragebitrate {v} is ignored")),
            "{b}"
        );
        assert!(Encoder::new(b, false).is_ok());
    }
    assert_eq!(bitrate(32, Some(6_000)).bps, 6_000);
    assert_eq!(bitrate(600, Some(510_000)).capped_by, Some(510_000));
    let mut e = Encoder::new(capped, false).unwrap();
    assert_eq!(e.settings().0, Bitrate::Value(20_000));
    assert!(e.encode(&[0; 100]).is_err(), "a frame is 20 ms");
}

#[test]
fn the_bitrate_sets_the_size_of_what_is_sent() {
    let pcm = voice(100);
    let mean = |kbps: u32| {
        let mut e = Encoder::new(bitrate(kbps, None), false).unwrap();
        let total: usize = pcm
            .chunks(F)
            .map(|f| sent(e.encode(f).unwrap()).0.len())
            .sum();
        total as f64 / 100.0
    };
    let (low, high) = (mean(16), mean(64));
    // 16 kbps is 40 bytes a frame, 64 kbps 160; VBR stays near them.
    assert!(
        (25.0..60.0).contains(&low),
        "16 kbps: {low:.0} bytes a frame"
    );
    assert!(
        (110.0..200.0).contains(&high),
        "64 kbps: {high:.0} bytes a frame"
    );
}

#[test]
fn dtx_skips_silence_marks_the_talkspurt_and_keeps_comfort_noise() {
    let silence = vec![0i16; F];
    let speech = voice(10);
    let mut e = Encoder::new(bitrate(32, None), true).unwrap();
    let out: Vec<Encoded> = (0..100).map(|_| e.encode(&silence).unwrap()).collect();
    let skipped = out.iter().filter(|o| matches!(o, Encoded::Skip)).count();
    assert!(skipped >= 60, "{skipped} of 100 silent frames skipped");
    // libopus breaks a DTX run with a comfort-noise update every 400 ms,
    // and the first frame after a pause carries the marker bit.
    let first_skip = out.iter().position(|o| matches!(o, Encoded::Skip)).unwrap();
    let updates: Vec<usize> = (first_skip..100)
        .filter(|&k| matches!(out[k], Encoded::Send { .. }))
        .collect();
    assert!(updates.len() >= 2, "comfort noise at {updates:?}");
    for &k in &updates {
        assert!(
            matches!(
                out[k],
                Encoded::Send {
                    talkspurt: true,
                    ..
                }
            ),
            "frame {k} follows a pause"
        );
    }
    // Speech after silence: sent, the first with the marker, the next not.
    let first = e.encode(&speech[..F]).unwrap();
    let first = if first == Encoded::Skip {
        e.encode(&speech[F..2 * F]).unwrap()
    } else {
        first
    };
    assert!(matches!(
        first,
        Encoded::Send {
            talkspurt: true,
            ..
        }
    ));
    let (_, mark) = sent(e.encode(&speech[2 * F..3 * F]).unwrap());
    assert!(!mark);
    // Without usedtx every frame goes out, no marker, well under a frame of
    // speech at the bitrate (80 bytes at 32 kbps): libopus 1.6 makes 32
    // bytes of a frame of digital silence (§5.2's note of 2026-10-15).
    let mut e = Encoder::new(bitrate(32, None), false).unwrap();
    let sizes: Vec<usize> = (0..100)
        .map(|k| {
            let (payload, mark) = sent(e.encode(&silence).unwrap());
            assert!(!mark, "frame {k}");
            payload.len()
        })
        .collect();
    assert!(sizes.iter().all(|&n| n <= 40), "{sizes:?}");
}
