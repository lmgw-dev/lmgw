use super::*;
use crate::realtime::calls::audio::FRAME_TICKS;

const MS: fn(u64) -> Duration = Duration::from_millis;

/// Frame `k` of a stream whose first frame starts at RTP time `rtp0`: its
/// end's RTP time, and its arrival when the phone captured its last sample
/// at `c + (k + 1) × 20 ms` and the network took `transit_ms`.
fn frame(rtp0: u64, c: Instant, k: u64, transit_ms: u64) -> (u64, Instant) {
    let end = rtp0 + (k + 1) * FRAME_TICKS;
    (end, c + MS(20 * (k + 1) + transit_ms))
}

#[test]
fn a_frame_is_stamped_on_rtp_time_plus_the_least_transit() {
    let c = Instant::now();
    let rtp0 = 7_000_000;
    let mut s = Stamps::default();
    assert_eq!(s.place(rtp0), None, "nothing before the first frame");
    // The first frame anchors the stream and is stamped at its arrival.
    let (end, at) = frame(rtp0, c, 0, 45);
    s.anchor(rtp0, at);
    assert_eq!(s.received(end, at), at);
    // Jitter: each stamp follows RTP time, never later than its arrival,
    // and from the 30 ms frame on every stamp is capture plus 30 ms.
    let transits = [45, 32, 60, 30, 70, 41, 30, 55];
    for (k, t) in transits.iter().enumerate().skip(1) {
        let (end, at) = frame(rtp0, c, k as u64, *t);
        let stamp = s.received(end, at);
        assert!(stamp <= at, "frame {k} stamped after it arrived");
        if k >= 3 {
            assert_eq!(stamp, c + MS(20 * (k as u64 + 1) + 30), "frame {k}");
        }
    }
    // A concealed run's end is placed by the same floor.
    let end = rtp0 + 10 * FRAME_TICKS;
    assert_eq!(s.place(end), Some(c + MS(200 + 30)));
}

#[test]
fn the_floor_follows_a_slower_path_after_its_horizon() {
    let c = Instant::now();
    let mut s = Stamps::default();
    s.anchor(0, c + MS(50));
    // 2 s at 30 ms, then the path gets slower: 80 ms.
    let mut k = 0;
    while k < 100 {
        let (end, at) = frame(0, c, k, 30);
        s.received(end, at);
        k += 1;
    }
    let (end, at) = frame(0, c, k, 80);
    assert_eq!(
        s.received(end, at),
        c + MS(20 * (k + 1) + 30),
        "within the horizon"
    );
    let last_fast = c + MS(20 * 100 + 30);
    while c + MS(20 * (k + 1) + 80) < last_fast + FLOOR_HORIZON {
        let (end, at) = frame(0, c, k, 80);
        s.received(end, at);
        k += 1;
    }
    // Every 30 ms frame is past the horizon now.
    k += 1;
    let (end, at) = frame(0, c, k, 80);
    assert_eq!(s.received(end, at), at, "the floor is the new path's");
}

#[test]
fn an_anchor_starts_the_floor_over() {
    let c = Instant::now();
    let mut s = Stamps::default();
    s.anchor(0, c + MS(20));
    let (end, at) = frame(0, c, 0, 0);
    s.received(end, at);
    // A discontinuity: the RTP clock starts again elsewhere, 3 s later.
    let later = c + MS(3000);
    s.anchor(90_000_000, later);
    assert!(s.anchored());
    assert_eq!(
        s.received(90_000_000 + FRAME_TICKS, later + MS(20)),
        later + MS(20)
    );
    assert_eq!(
        s.place(90_000_000 + 2 * FRAME_TICKS),
        Some(later + MS(40)),
        "the next frame, on the new anchor"
    );
}
