use super::*;
use crate::realtime::calls::audio::fixtures::{error, opus_stream, rms, voice};
use crate::realtime::calls::audio::FRAME_TICKS;

const MS: fn(u64) -> Duration = Duration::from_millis;
/// Where the phone's RTP clock starts: anywhere, extended past the wrap.
const RTP0: u64 = 4_294_000_000;
const SEQ0: u64 = 65_000;
/// The default reorder window (§4.2).
const REORDER: Duration = Duration::from_millis(60);
const F: usize = FRAME_SAMPLES;

/// The phone's stream: frame `k` ends at `t0 + 20 (k + 1)` ms on its clock
/// and the network takes 30 ms, unless a test says otherwise.
struct Phone {
    t0: Instant,
    stream: Vec<Vec<u8>>,
}

impl Phone {
    fn new(frames: usize) -> Self {
        Self {
            t0: Instant::now(),
            stream: opus_stream(&voice(frames)),
        }
    }

    /// When frame `k` arrives on time.
    fn on_time(&self, k: usize) -> Instant {
        self.t0 + MS(20 * (k as u64 + 1) + 30)
    }

    /// Frame `k`, on time, with no packet missing before it.
    fn frame(&self, k: usize) -> InFrame<'_> {
        InFrame {
            rtp: RTP0 + k as u64 * FRAME_TICKS,
            arrived: self.on_time(k),
            contiguous: true,
            seq: SEQ0 + k as u64,
            payload: &self.stream[k],
        }
    }

    /// Frame `k` after lost packets.
    fn after_loss(&self, k: usize) -> InFrame<'_> {
        InFrame {
            contiguous: false,
            ..self.frame(k)
        }
    }
}

fn audio(o: Outcome) -> Decoded {
    match o {
        Outcome::Audio(d) => d,
        other => panic!("not audio: {other:?}"),
    }
}

/// Every frame of `phone` decoded in order by a decoder of its own.
fn reference(phone: &Phone) -> Vec<i16> {
    let mut d = Decode::new(REORDER).unwrap();
    (0..phone.stream.len())
        .flat_map(|k| audio(d.frame(phone.frame(k))).pcm)
        .collect()
}

#[test]
fn frames_in_order_decode_onto_the_timeline_at_their_capture() {
    let phone = Phone::new(50);
    let mut d = Decode::new(REORDER).unwrap();
    let mut all = Vec::new();
    for k in 0..50 {
        let out = audio(d.frame(phone.frame(k)));
        assert_eq!(out.pcm.len(), F, "frame {k}: 20 ms at 24 kHz");
        assert_eq!(out.gap, None);
        // A steady transit: each frame is stamped at its arrival, the last
        // sample's capture plus the least transit.
        assert_eq!(out.captured, phone.on_time(k), "frame {k}");
        all.extend(out.pcm);
    }
    assert!(rms(&all) > 1000.0, "voice came out: {}", rms(&all));
    assert_eq!(d.dropped(), 0);
}

#[test]
fn a_single_loss_is_recovered_from_the_next_packets_fec() {
    let phone = Phone::new(40);
    let truth = reference(&phone);
    let run = |contiguous: bool| {
        let mut d = Decode::new(REORDER).unwrap();
        for k in 0..20 {
            audio(d.frame(phone.frame(k)));
        }
        // Frame 20 never arrives.
        audio(d.frame(InFrame {
            contiguous,
            ..phone.frame(21)
        }))
    };
    let out = run(false);
    assert_eq!(
        out.gap,
        Some(Gap::Filled {
            missing: MS(20),
            concealed: 0,
            fec: F as u64
        })
    );
    assert_eq!(out.pcm.len(), 2 * F, "the lost frame, then frame 21");
    assert_eq!(
        out.captured,
        phone.on_time(21),
        "the timeline stays on RTP time"
    );
    // What FEC gave back is near what was sent, much nearer than
    // concealment: the same gap with no packet lost is concealed whole.
    let lost = &truth[20 * F..21 * F];
    let fec = error(&out.pcm[..F], lost);
    let plc_out = run(true);
    assert_eq!(
        plc_out.gap,
        Some(Gap::Filled {
            missing: MS(20),
            concealed: F as u64,
            fec: 0
        })
    );
    let plc = error(&plc_out.pcm[..F], lost);
    // Measured: 385 against 4310, on a signal of 5310.
    assert!(
        fec < plc / 4.0,
        "FEC error {fec:.0} against concealment's {plc:.0} (signal {:.0})",
        rms(lost)
    );
}

#[test]
fn a_multi_frame_loss_is_concealed_within_the_wall_clock_bound() {
    let phone = Phone::new(40);
    let mut d = Decode::new(REORDER).unwrap();
    let mut samples = 0;
    for k in 0..20 {
        samples += audio(d.frame(phone.frame(k))).pcm.len();
    }
    // Frames 20 to 23 are lost; 24 comes on time, 100 ms after 19.
    let out = audio(d.frame(phone.after_loss(24)));
    assert_eq!(
        out.gap,
        Some(Gap::Filled {
            missing: MS(80),
            concealed: 3 * F as u64,
            fec: F as u64
        })
    );
    assert_eq!(out.captured, phone.on_time(24));
    samples += out.pcm.len();
    for k in 25..40 {
        let out = audio(d.frame(phone.frame(k)));
        assert_eq!(out.gap, None);
        samples += out.pcm.len();
    }
    assert_eq!(
        samples,
        40 * F,
        "the timeline is RTP time, sample for sample"
    );
}

#[test]
fn a_gap_past_the_wall_clock_bound_conceals_only_the_wall_gap_and_re_anchors() {
    let phone = Phone::new(40);
    let mut d = Decode::new(REORDER).unwrap();
    for k in 0..20 {
        audio(d.frame(phone.frame(k)));
    }
    // 200 ms of RTP time missing, but frame 30 arrives 120 ms after 19:
    // more than the 120 + 60 ms the wall clock allows.
    let arrived = phone.on_time(19) + MS(120);
    let out = audio(d.frame(InFrame {
        arrived,
        ..phone.after_loss(30)
    }));
    let gap = out.gap.unwrap();
    assert_eq!(
        gap,
        Gap::Discontinuity {
            jump: Jump::Forward(MS(200)),
            bound: MS(180),
            wall: MS(100),
            concealed: 100 * 24
        },
        "the wall gap: 120 ms less frame 19's own 20"
    );
    assert_eq!(out.pcm.len(), 100 * 24 + F);
    let line = gap.to_string();
    assert!(
        line.contains("forward by 200 ms") && line.contains("realtime.webrtc_reorder_ms"),
        "{line}"
    );
    // Re-anchored: the frame is stamped at its arrival, and the next one 20
    // ms of RTP time on.
    assert_eq!(out.captured, arrived);
    let next = audio(d.frame(InFrame {
        arrived: arrived + MS(20),
        ..phone.frame(31)
    }));
    assert_eq!((next.gap, next.captured), (None, arrived + MS(20)));
}

#[test]
fn a_sender_clock_reset_is_a_discontinuity_either_way() {
    let phone = Phone::new(40);
    let mut d = Decode::new(REORDER).unwrap();
    for k in 0..10 {
        audio(d.frame(phone.frame(k)));
    }
    // The RTP clock jumps 10 s ahead while frames keep coming every 20 ms:
    // nothing is missing in wall time, so nothing is concealed.
    let ahead = 10 * 48_000;
    let jumped = |k: usize, by: i64| InFrame {
        rtp: (RTP0 as i64 + by + (k as i64) * FRAME_TICKS as i64) as u64,
        ..phone.frame(k)
    };
    let out = audio(d.frame(jumped(10, ahead)));
    assert_eq!(
        out.gap,
        Some(Gap::Discontinuity {
            jump: Jump::Forward(MS(10_000)),
            bound: MS(20) + REORDER,
            wall: Duration::ZERO,
            concealed: 0
        })
    );
    assert_eq!(out.pcm.len(), F);
    assert_eq!(out.captured, phone.on_time(10));
    assert_eq!(audio(d.frame(jumped(11, ahead))).gap, None);
    // Then 5 s back, the sequence numbers running on: not a late packet.
    let back = ahead - 5 * 48_000;
    let out = audio(d.frame(jumped(12, back)));
    assert_eq!(
        out.gap,
        Some(Gap::Discontinuity {
            jump: Jump::Backward(MS(5_000)),
            bound: MS(20) + REORDER,
            wall: Duration::ZERO,
            concealed: 0
        })
    );
    assert_eq!(out.captured, phone.on_time(12));
    assert_eq!(d.dropped(), 0);
}

#[test]
fn a_packet_behind_the_timeline_is_dropped_and_counted() {
    // What str0m would pass on with no hold-back, within the wall-clock
    // bound: the reader drops it too.
    let phone = Phone::new(20);
    let mut d = Decode::new(REORDER).unwrap();
    for k in 0..10 {
        audio(d.frame(phone.frame(k)));
    }
    let out = audio(d.frame(phone.after_loss(11)));
    assert!(matches!(out.gap, Some(Gap::Filled { .. })), "{:?}", out.gap);
    // Frame 10 turns up after its time was filled.
    let late = InFrame {
        arrived: phone.on_time(11) + MS(5),
        ..phone.frame(10)
    };
    let Outcome::Late(l) = d.frame(late) else {
        panic!("not dropped")
    };
    assert_eq!(
        l,
        Late {
            behind: MS(40),
            dropped: 1
        }
    );
    assert!(l.to_string().contains("1 so far"), "{l}");
    assert_eq!(d.dropped(), 1);
    // A duplicate is late too, and the timeline goes on.
    assert!(matches!(d.frame(phone.frame(11)), Outcome::Late(_)));
    let out = audio(d.frame(phone.frame(12)));
    assert_eq!((out.gap, out.pcm.len()), (None, F));
}

#[test]
fn a_payload_libopus_cannot_read_is_a_lost_frame() {
    let phone = Phone::new(10);
    let mut d = Decode::new(REORDER).unwrap();
    for k in 0..4 {
        audio(d.frame(phone.frame(k)));
    }
    let empty = InFrame {
        payload: &[],
        ..phone.frame(4)
    };
    assert!(matches!(d.frame(empty), Outcome::Invalid(_)));
    // The next frame finds its time missing: no packet was, so it is
    // concealed, not taken from FEC.
    let out = audio(d.frame(phone.frame(5)));
    assert_eq!(
        out.gap,
        Some(Gap::Filled {
            missing: MS(20),
            concealed: F as u64,
            fec: 0
        })
    );
}

/// A stream that restarts — a replaced track, its RTP time and its sequence
/// numbers both behind — is a discontinuity, not a run of late packets: the
/// input goes on from it rather than going deaf.
#[test]
fn a_restart_behind_in_rtp_time_and_sequence_is_heard() {
    let phone = Phone::new(20);
    let mut d = Decode::new(REORDER).unwrap();
    for k in 0..10 {
        audio(d.frame(phone.frame(k)));
    }
    // The new stream starts 3 s back on the RTP clock and at sequence 1, on
    // time: 3 s behind is far past the 20 + 60 ms the wall clock allows.
    let restarted = |k: usize| InFrame {
        rtp: RTP0 - 3 * 48_000 + (k as u64 - 10) * FRAME_TICKS,
        seq: 1 + k as u64 - 10,
        ..phone.frame(k)
    };
    let out = audio(d.frame(restarted(10)));
    assert!(
        matches!(
            out.gap,
            Some(Gap::Discontinuity {
                jump: Jump::Backward(_),
                ..
            })
        ),
        "{:?}",
        out.gap
    );
    for k in 11..20 {
        let out = audio(d.frame(restarted(k)));
        assert_eq!((out.gap, out.pcm.len()), (None, F), "frame {k}");
    }
    assert_eq!(d.dropped(), 0);
}

/// A long gap is concealed sample-exact, but libopus is asked only until
/// its concealment has faded to silence: the rest is zeros.
#[test]
fn a_long_gap_is_concealed_until_it_fades_then_filled_with_zeros() {
    let phone = Phone::new(20);
    let mut d = Decode::new(REORDER).unwrap();
    for k in 0..10 {
        audio(d.frame(phone.frame(k)));
    }
    // Nothing arrives for 60 s, then the stream goes on 60 s on (the
    // phone's own pause, its RTP clock running): all of it concealed.
    let gap = 60 * 48_000;
    let out = audio(d.frame(InFrame {
        rtp: RTP0 + 10 * FRAME_TICKS + gap,
        arrived: phone.on_time(9) + MS(60_000 + 20),
        ..phone.frame(10)
    }));
    assert_eq!(
        out.gap,
        Some(Gap::Filled {
            missing: MS(60_000),
            concealed: 60 * 24_000,
            fec: 0
        })
    );
    assert_eq!(out.pcm.len(), 60 * 24_000 + F, "sample for sample");
    assert!(
        d.plc_steps < 500,
        "{} concealment steps for 60 s (3000 frames)",
        d.plc_steps
    );
    assert!(out.pcm[30 * 24_000..60 * 24_000].iter().all(|&s| s == 0));
}
