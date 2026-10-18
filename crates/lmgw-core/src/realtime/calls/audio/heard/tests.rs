use std::time::{SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

use super::*;
use crate::realtime::calls::audio::clock::FrameClock;
use crate::realtime::calls::audio::frames::{OutFrame, Span};
use crate::realtime::calls::audio::line::ntp_ms;

const MS: fn(u64) -> Duration = Duration::from_millis;

fn span(item: &str, at: usize, from: u64, to: u64) -> Span {
    Span {
        item_id: item.into(),
        at,
        from,
        to,
    }
}

/// Two frames of silence, then response r1 over five — item i1, a clause
/// change into i2 inside the second, an underrun's padding in the fourth —
/// then three of silence. RTP 0 is the stream's start, and lmgw's sender
/// report put it at `report`.
fn call() -> (Record, SentLine, SystemTime) {
    let t0 = Instant::now();
    let mut clock = FrameClock::new(t0, 0);
    let mut record = Record::new(t0, 0);
    let r1 = |spans: Vec<Span>, padded: usize| OutFrame {
        pcm: vec![0; 480],
        response_id: Some("r1".into()),
        spans,
        padded,
        starved: None,
        started: None,
    };
    let silence = OutFrame {
        pcm: vec![0; 480],
        response_id: None,
        spans: Vec::new(),
        padded: 0,
        starved: None,
        started: None,
    };
    let frames = [
        silence.clone(),
        silence.clone(),
        r1(vec![span("i1", 0, 0, 480)], 0),
        r1(vec![span("i1", 0, 480, 800), span("i2", 320, 0, 160)], 0),
        r1(vec![span("i2", 0, 160, 640)], 0),
        r1(Vec::new(), 480),
        r1(vec![span("i2", 0, 640, 1120)], 0),
        silence.clone(),
        silence.clone(),
        silence,
    ];
    for (k, f) in frames.iter().enumerate() {
        let slot = clock.take(t0 + MS(20 * k as u64)).unwrap();
        record.sent(slot, f, slot.due);
    }
    let report = UNIX_EPOCH + Duration::from_secs(1_760_000_000);
    (record, SentLine::new(report, 0, 0), report)
}

fn heard(pairs: &[(&str, u64)]) -> Heard {
    pairs.iter().map(|(i, n)| (i.to_string(), *n)).collect()
}

#[test]
fn a_stop_with_a_position_cuts_each_item_at_the_sample_playing_there() {
    let (record, line, report) = call();
    let at = |ms: f64| at_position(&record, Some(&line), ntp_ms(report) + ms);
    // 16.67 ms into r1's second frame (60 ms in): 400 samples of it played,
    // i1's 320 and 80 of i2.
    let (h, clamped) = at(40.0 + 20.0 + 400.0 / 24.0).unwrap();
    assert_eq!((h, clamped), (heard(&[("i1", 800), ("i2", 80)]), None));
    // Inside the padding: the padding counts as nothing.
    let (h, _) = at(100.0 + 7.0).unwrap();
    assert_eq!(h, heard(&[("i1", 800), ("i2", 640)]));
    // Before r1's first frame: nothing of it was heard.
    let (h, _) = at(30.0).unwrap();
    assert_eq!(h, heard(&[("i1", 0), ("i2", 0)]));
    // Past the last frame sent: clamped to what was sent, and said.
    let (h, clamped) = at(500.0).unwrap();
    assert_eq!(h, heard(&[("i1", 800), ("i2", 1120)]));
    let clamped = clamped.unwrap();
    assert_eq!((clamped.rtp, clamped.end), (500 * 48, 10 * 960));
    assert!(
        clamped.to_string().contains("heard is clamped"),
        "{clamped}"
    );
    // No sender report to map through: heard is what was sent, the
    // caller's.
    assert_eq!(
        at_position(&record, None, ntp_ms(report)),
        Err(Unmapped::NoLine)
    );
}

#[test]
fn a_barge_in_hears_what_left_by_the_allowance_before_the_cut() {
    let (record, _, _) = call();
    // r1's frames left at 40 to 120 ms (its first is slot 2): the last one's
    // send end is 140 ms.
    let t0 = record.frames()[0].sent - MS(40);
    let end = t0 + MS(140);
    // Cut as the last frame's send ends: r1 ended with 480 samples of i2 in
    // that frame, and 10 ms less than all of it is 240 samples into it.
    assert_eq!(
        on_barge_in(&record, MS(10), end),
        heard(&[("i1", 800), ("i2", 640 + 240)])
    );
    // 30 ms back lands in the padding before it.
    assert_eq!(
        on_barge_in(&record, MS(30), end),
        heard(&[("i1", 800), ("i2", 640)])
    );
    // Cut while the last frame still leaves (5 ms after it began): the
    // allowance comes off what had left by then.
    assert_eq!(
        on_barge_in(&record, MS(10), t0 + MS(125)),
        heard(&[("i1", 800), ("i2", 640)])
    );
    // Confirmed after the last frame left: 25 ms after its end, 30 ms back
    // is 15 ms into it, more than the end less the allowance.
    assert_eq!(
        on_barge_in(&record, MS(30), end + MS(25)),
        heard(&[("i1", 800), ("i2", 640 + 360)])
    );
    // Confirmed once all of it played: what was sent, no more.
    assert_eq!(on_barge_in(&record, MS(30), end + MS(500)), sent(&record));
    // Before the answer's first frame left: nothing of it.
    assert_eq!(
        on_barge_in(&record, MS(30), t0 + MS(50)),
        heard(&[("i1", 0), ("i2", 0)])
    );
    // Without the allowance, what left by the cut.
    assert_eq!(on_barge_in(&record, Duration::ZERO, end), sent(&record));
    assert_eq!(sent(&record), heard(&[("i1", 800), ("i2", 1120)]));
}

/// A tick 40 ms late takes three slots at once: the phone plays them one
/// after another, so what left by a time counts them in turn, not all at
/// the instant they left together.
#[test]
fn a_late_ticks_frames_count_one_after_another() {
    let t0 = Instant::now();
    let mut clock = FrameClock::new(t0, 0);
    let mut record = Record::new(t0, 0);
    for (k, at) in [0, 60, 60, 60].into_iter().enumerate() {
        let k = k as u64;
        let slot = clock.take(t0 + MS(20 * k)).unwrap();
        let frame = OutFrame {
            pcm: vec![0; 480],
            response_id: Some("r1".into()),
            spans: vec![span("i1", 0, 480 * k, 480 * (k + 1))],
            padded: 0,
            starved: None,
            started: None,
        };
        record.sent(slot, &frame, t0 + MS(at));
    }
    // Frame 0 plays 0 to 20 ms, then nothing until the late three, from 60,
    // 80 and 100 ms. Cut at 110 ms, 20 ms back: 10 ms into the third.
    assert_eq!(
        on_barge_in(&record, MS(20), t0 + MS(110)),
        heard(&[("i1", 960 + 240)])
    );
    // In the gap: everything before the late frames.
    assert_eq!(
        on_barge_in(&record, MS(20), t0 + MS(60)),
        heard(&[("i1", 480)])
    );
    assert_eq!(record.rtp_at(t0 + MS(500)), Some(4 * 960));
    assert_eq!(Record::new(t0, 0).rtp_at(t0), None, "no frame");
}
