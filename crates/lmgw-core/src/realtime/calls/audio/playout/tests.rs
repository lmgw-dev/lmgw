use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::realtime::calls::audio::clock::FrameClock;
use crate::realtime::calls::audio::frames::OutFrame;
use crate::realtime::calls::audio::line::ntp_ms;

const MS: fn(u64) -> Duration = Duration::from_millis;

/// A call whose output started at `t0`, 50 frames of response r1 sent on
/// time, and the line of lmgw's sender report that fixed RTP 0 at
/// `report`'s NTP time.
struct Call {
    t0: Instant,
    record: Record,
    line: SentLine,
    report: SystemTime,
}

impl Call {
    fn new() -> Self {
        let t0 = Instant::now();
        let mut clock = FrameClock::new(t0, 0);
        let mut record = Record::new(t0, 0);
        let frame = OutFrame {
            pcm: vec![0; 480],
            response_id: Some("r1".into()),
            spans: Vec::new(),
            padded: 0,
            starved: None,
            started: None,
        };
        for k in 0..50 {
            let slot = clock.take(t0 + MS(20 * k)).unwrap();
            record.sent(slot, &frame, slot.due);
        }
        let report = UNIX_EPOCH + Duration::from_secs(1_760_000_000);
        Self {
            t0,
            record,
            line: SentLine::new(report, 0, 0),
            report,
        }
    }

    /// `playout_ntp_ms` of the sample sent `ms` into the stream.
    fn position(&self, ms: u64) -> f64 {
        ntp_ms(self.report) + ms as f64
    }
}

#[test]
fn the_allowance_is_one_frame_then_the_formula_then_the_reports() {
    let call = Call::new();
    let mut p = Playout::new(Some(Duration::from_secs(20)));
    let send_end = call.t0 + MS(1000);
    // Before the first receiver report: one frame, and no round trip.
    let a = p.allowance(None);
    assert_eq!((a.value, a.source), (MS(20), Source::Frame));
    assert!(a.to_string().contains("round trip not measured yet"), "{a}");
    assert_eq!(p.drained(send_end, None), send_end + MS(20));
    // A receiver report: J = 480 ticks (10 ms), round trip 80 ms.
    p.jitter(480);
    let rtt = Some(MS(80));
    let a = p.allowance(rtt);
    assert_eq!((a.value, a.source), (MS(40), Source::Formula));
    assert!(
        a.to_string()
            .contains("2 × the phone's jitter (480 ticks, 10 ms)"),
        "{a}"
    );
    assert_eq!(p.window_end(send_end, rtt), send_end + MS(40));
    assert_eq!(
        p.drained(send_end, rtt),
        send_end + MS(80),
        "plus half the round trip"
    );
    // A playout report: the sample sent 300 ms in plays as the report
    // leaves, 40 ms (half the round trip) before it arrives 490 ms in.
    let notes = p.report(
        call.t0 + MS(490),
        Some(call.position(300)),
        Some(61.0),
        Some(&call.line),
        &call.record,
        rtt,
    );
    assert!(notes.is_empty(), "{notes:?}");
    let a = p.allowance(rtt);
    assert_eq!(
        (a.value, a.source, a.delay),
        (MS(110), Source::Report, Some(MS(150)))
    );
    assert!(a.to_string().contains("jitter buffer 61 ms"), "{a}");
    // The formula for the same stretch, beside it (§5.3, §13.2).
    assert!(
        a.to_string()
            .contains("the formula would give 40 ms (the phone's jitter 480 ticks)"),
        "{a}"
    );
    assert_eq!(p.window_end(send_end, rtt), send_end + MS(110));
    assert_eq!(
        p.drained(send_end, rtt),
        send_end + MS(150),
        "send end plus d"
    );
    // A report with only the jitter buffer keeps the measured delay.
    p.report(call.t0 + MS(900), None, Some(70.0), None, &call.record, rtt);
    let a = p.allowance(rtt);
    assert_eq!((a.delay, a.jitter_buffer_ms), (Some(MS(150)), Some(70.0)));
}

#[test]
fn reports_that_stop_for_an_interval_of_playback_fall_back_to_the_formula() {
    let call = Call::new();
    let interval = Duration::from_secs(1);
    let mut p = Playout::new(Some(interval));
    p.jitter(240);
    let rtt = Some(MS(60));
    let arrived = call.t0 + MS(400);
    p.report(
        arrived,
        Some(call.position(300)),
        None,
        Some(&call.line),
        &call.record,
        rtt,
    );
    assert_eq!(p.allowance(rtt).source, Source::Report);
    // 49 frames of playback with no report: still the report's.
    for k in 1..50 {
        assert_eq!(p.played(arrived + MS(20 * k)), None);
    }
    assert_eq!(p.allowance(rtt).source, Source::Report);
    // The 50th: a whole second of playback.
    let note = p.played(arrived + MS(1000)).unwrap();
    assert_eq!(note, Note::FellBack { age: MS(1000) });
    assert!(
        note.to_string().contains("realtime.ping_interval_s"),
        "{note}"
    );
    assert_eq!(p.played(arrived + MS(1020)), None, "said once");
    let a = p.allowance(rtt);
    assert_eq!((a.value, a.source), (MS(30), Source::Formula));
    // A report again takes over again.
    let notes = p.report(
        arrived + MS(1100),
        Some(call.position(900)),
        None,
        Some(&call.line),
        &call.record,
        rtt,
    );
    assert_eq!(notes, vec![Note::Again]);
    assert_eq!(p.allowance(rtt).source, Source::Report);
    // With the interval at 0, the last report holds however long it plays.
    let mut held = Playout::new(None);
    held.report(
        arrived,
        Some(call.position(300)),
        None,
        Some(&call.line),
        &call.record,
        rtt,
    );
    for k in 1..1000 {
        assert_eq!(held.played(arrived + MS(20 * k)), None);
    }
    assert_eq!(held.allowance(rtt).source, Source::Report);
}

#[test]
fn a_position_that_maps_to_no_frame_sent_is_ignored_with_its_reason() {
    let call = Call::new();
    let mut p = Playout::new(Some(Duration::from_secs(20)));
    let rtt = Some(MS(80));
    // Past the last frame sent (the stream is 1 s long).
    let notes = p.report(
        call.t0 + MS(1500),
        Some(call.position(1200)),
        None,
        Some(&call.line),
        &call.record,
        rtt,
    );
    assert!(
        matches!(&notes[..], [Note::Ignored(why)] if why.contains("past the last frame sent")),
        "{notes:?}"
    );
    // Without a sender report of lmgw's to map through.
    let notes = p.report(
        call.t0,
        Some(call.position(10)),
        None,
        None,
        &call.record,
        rtt,
    );
    assert!(matches!(&notes[..], [Note::Ignored(_)]), "{notes:?}");
    // A position the phone could not have played yet.
    let notes = p.report(
        call.t0 + MS(100),
        Some(call.position(500)),
        None,
        Some(&call.line),
        &call.record,
        rtt,
    );
    assert!(matches!(&notes[..], [Note::Ignored(why)] if why.contains("after the report left")));
    assert_eq!(p.allowance(rtt).source, Source::Frame, "nothing measured");
}

/// What the phone says is bounded by `realtime.ping_interval_s`, as the
/// round trip it stands in for is: a measured delay, or a jitter whose
/// allowance, of the interval or more is ignored with its note, and the
/// last value kept. At 0 nothing is bounded.
#[test]
fn a_delay_or_a_jitter_of_the_interval_or_more_is_ignored() {
    let call = Call::new();
    let mut p = Playout::new(Some(Duration::from_secs(1)));
    assert_eq!(p.jitter(240), None);
    // J of 24 000 ticks: an allowance of 20 ms + 1 s.
    let n = p.jitter(24_000).expect("ignored");
    assert_eq!(
        n,
        Note::JitterIgnored {
            jitter: 24_000,
            allowance: MS(1020),
            bound: Duration::from_secs(1),
        }
    );
    assert!(n.to_string().contains("realtime.ping_interval_s"), "{n}");
    assert_eq!(p.allowance(None).jitter, Some(240), "the last J kept");
    // J just under the bound: 20 ms + 2 × 489.6 ms.
    assert_eq!(p.jitter(23_500), None);
    // A report naming the sample sent 100 ms in, arriving 1.1 s in: a delay
    // of a whole second, ignored.
    let notes = p.report(
        call.t0 + MS(1100),
        Some(call.position(100)),
        None,
        Some(&call.line),
        &call.record,
        None,
    );
    assert!(
        matches!(&notes[..], [Note::Ignored(why)] if why.contains("playout delay of 1000 ms")),
        "{notes:?}"
    );
    assert_eq!(p.allowance(None).source, Source::Formula);
    // 999 ms is a delay.
    let notes = p.report(
        call.t0 + MS(999),
        Some(call.position(0)),
        None,
        Some(&call.line),
        &call.record,
        None,
    );
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(p.allowance(None).delay, Some(MS(999)));
    // At 0 the phone's numbers stand.
    let mut p = Playout::new(None);
    assert_eq!(p.jitter(24_000), None);
    assert_eq!(p.allowance(None).value, MS(1020));
}
