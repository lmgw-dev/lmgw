//! The paced queue pulled by a frame clock (calls design §5.1, §5.3, §5.4):
//! the writer on a sink that pulls, ticked by hand on tokio's paused clock.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::sync::mpsc;

use super::*;
use crate::realtime::calls::audio::clock::{FrameClock, Slot};
use crate::realtime::calls::audio::line::ntp_ms;
use crate::realtime::calls::audio::{FRAME, FRAME_SAMPLES, FRAME_TICKS};
use crate::realtime::link::{Close, Sink};
use crate::realtime::protocol::PartRef;
use crate::realtime::writer::{self, Drained, Outbox, WriterHandle};

const MS: fn(u64) -> Duration = Duration::from_millis;
const RTP0: u64 = 7_000;

/// A sink whose audio its own clock pulls: the test is the clock.
struct PullSink {
    sent: mpsc::UnboundedSender<String>,
    queue: Arc<Mutex<Option<Pulled>>>,
}

impl Sink for PullSink {
    async fn event(&mut self, text: String) -> bool {
        self.sent.send(text).is_ok()
    }
    async fn ping(&mut self, _round_trip: &RoundTrip) -> bool {
        true
    }
    async fn close(&mut self, _close: Close) {}
    async fn end(&mut self) {}
    fn pull(&mut self, queue: Pulled) -> bool {
        *self.queue.lock().unwrap() = Some(queue);
        true
    }
}

struct Rig {
    out: WriterHandle,
    drained: Drained,
    sent: mpsc::UnboundedReceiver<String>,
    queue: Pulled,
    clock: FrameClock,
    t0: Instant,
    _task: tokio::task::JoinHandle<()>,
}

impl Rig {
    fn new(interval: Option<Duration>) -> Self {
        let (tx, sent) = mpsc::unbounded_channel();
        let slot = Arc::new(Mutex::new(None));
        let sink = PullSink {
            sent: tx,
            queue: slot.clone(),
        };
        let (out, drained, task) = writer::spawn(sink, Arc::new(Ids::new()));
        let queue = slot
            .lock()
            .unwrap()
            .take()
            .expect("the writer handed the queue over");
        let t0 = Instant::now();
        queue.begin(t0, RTP0, interval);
        Self {
            out,
            drained,
            sent,
            queue,
            clock: FrameClock::new(t0, RTP0),
            t0,
            _task: task,
        }
    }

    /// Hand `ob` to the writer and let it take everything off the lane.
    async fn flush(&mut self, mut ob: Outbox) {
        ob.flush(&self.out).await;
        while self.out.data.capacity() < writer::WINDOW {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
    }

    /// The next slot's tick on a bound call, the clock moved to its due
    /// time.
    async fn tick(&mut self) -> Tick {
        self.try_tick().await.expect("the queue is pulled")
    }

    /// [`Self::tick`], `None` once the queue stopped.
    async fn try_tick(&mut self) -> Option<Tick> {
        let due = self.clock.next_due();
        tokio::time::sleep_until(due).await;
        let slot = self.clock.take(Instant::now()).expect("a slot is due");
        let mut events = Vec::new();
        let t = self
            .queue
            .tick(slot, Instant::now(), true, |text| events.push(text))?;
        Some(Tick {
            pcm: t.pcm,
            starved: t.starved,
            notes: t.notes,
            allowance: t.allowance,
            events,
        })
    }

    /// A tick `late` after the next slot's due time, as the frame clock
    /// takes it (`calls::audio::out`): the slots the phone would discard
    /// skipped, the rest ticked in order. How many were skipped, and the
    /// ticks.
    async fn late_tick(&mut self, late: Duration) -> (usize, Vec<Tick>) {
        tokio::time::sleep_until(self.clock.next_due() + late).await;
        let now = Instant::now();
        let slots: Vec<Slot> = std::iter::from_fn(|| self.clock.take(now)).collect();
        let (stale, _) = self.queue.skip_stale(&slots, now).expect("pulled");
        let mut ticks = Vec::new();
        for slot in slots.into_iter().skip(stale) {
            let mut events = Vec::new();
            let t = self
                .queue
                .tick(slot, now, true, |text| events.push(text))
                .expect("pulled");
            ticks.push(Tick {
                pcm: t.pcm,
                starved: t.starved,
                notes: t.notes,
                allowance: t.allowance,
                events,
            });
        }
        (stale, ticks)
    }

    /// What the writer itself sent, by `type` (`delta` for a transcript).
    fn sent(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(t) = self.sent.try_recv() {
            out.push(label(&t));
        }
        out
    }
}

fn label(text: &str) -> String {
    let v: Value = serde_json::from_str(text).unwrap();
    match v["type"].as_str().unwrap() {
        "response.output_audio_transcript.delta" => format!("T:{}", v["delta"].as_str().unwrap()),
        other => other.into(),
    }
}

/// A tick, with the events it handed the link.
struct Tick {
    pcm: Vec<i16>,
    starved: Option<OutputAudioStarved>,
    notes: Vec<Note>,
    allowance: Option<Allowance>,
    events: Vec<String>,
}

fn labels(t: &Tick) -> Vec<String> {
    t.events.iter().map(String::as_str).map(label).collect()
}

fn at(item: &str) -> PartRef {
    PartRef {
        response_id: "resp_1".into(),
        item_id: item.into(),
        output_index: 0,
        content_index: 0,
    }
}

/// `samples` of `value`, as the core queues audio (PCM16-LE).
fn audio(ob: &mut Outbox, gen: u64, item: &str, samples: usize, value: i16) {
    let pcm: Vec<u8> = std::iter::repeat_n(value.to_le_bytes(), samples)
        .flatten()
        .collect();
    ob.send_audio(gen, at(item), bytes::Bytes::from(pcm));
}

fn delta(gen: u64, ob: &mut Outbox, item: &str, text: &str) {
    ob.send_purgeable(
        gen,
        ServerEvent::OutputAudioTranscriptDelta {
            at: at(item),
            delta: text.into(),
        },
    );
}

#[tokio::test(start_paused = true)]
async fn each_tick_takes_twenty_ms_with_the_deltas_ahead_of_it_and_pads_a_starved_response() {
    let mut r = Rig::new(None);
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    delta(1, &mut ob, "i1", "Hello");
    audio(&mut ob, 1, "i1", 720, 100);
    r.flush(ob).await;
    // Nothing leaves by time: the transcript waits for the tick that takes
    // the audio behind it.
    assert!(r.sent().is_empty());
    let t = r.tick().await;
    // The response's first frame: `started` ahead of the deltas (§6.2).
    assert_eq!(labels(&t), ["output_audio_buffer.started", "T:Hello"]);
    assert_eq!(t.pcm, vec![100; FRAME_SAMPLES]);
    assert_eq!(t.starved, None);
    // The rest of the chunk, then padding for a response still speaking:
    // starved, said once.
    let t = r.tick().await;
    assert_eq!(&t.pcm[..240], &[100; 240]);
    assert_eq!(&t.pcm[240..], &[0; 240]);
    assert_eq!(t.starved, None, "audio in the frame: not starved yet");
    let t = r.tick().await;
    assert_eq!(t.pcm, vec![0; FRAME_SAMPLES]);
    assert_eq!(
        t.starved,
        Some(OutputAudioStarved {
            response_id: "resp_1".into(),
            starved: true
        })
    );
    assert_eq!(r.tick().await.starved, None, "once");
    // Its audio again: starved false with it.
    let mut ob = Outbox::default();
    delta(1, &mut ob, "i1", "again");
    audio(&mut ob, 1, "i1", 480, 7);
    r.flush(ob).await;
    let t = r.tick().await;
    assert_eq!(t.pcm, vec![7; FRAME_SAMPLES]);
    // The delta, then on a bound call `starved: false` after it.
    assert_eq!(labels(&t), ["T:again", "lmgw.output_audio.starved"]);
    assert_eq!(t.starved.map(|s| s.starved), Some(false));
    // The writer released none of it: the clock did.
    assert!(r.sent().is_empty());
    // What was sent, exact to the frame: 720 + 480 samples of i1.
    let s = r.out.purge(1);
    assert_eq!(s.audio.get("i1"), Some(&1200));
    assert!(s.heard);
}

#[tokio::test(start_paused = true)]
async fn the_window_ends_an_allowance_after_the_last_frame_and_drained_half_a_round_trip_later() {
    let mut r = Rig::new(None);
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    audio(&mut ob, 1, "i1", 3 * FRAME_SAMPLES, 1);
    ob.drained(1);
    r.flush(ob).await;
    for _ in 0..3 {
        r.tick().await;
    }
    // Frames 0..2 left at 0, 20 and 40 ms: the last one's send end is 60 ms.
    // No receiver report yet: the allowance is one frame, no round trip.
    let p = r.out.playback().unwrap();
    assert_eq!(p.first, Some(r.t0));
    assert_eq!(p.end(), Some(r.t0 + MS(60) + FRAME));
    assert_eq!(p.drained_at(), Some(r.t0 + MS(80)));
    assert!(!p.waiting);
    // A round trip of 80 ms and a jitter of 480 ticks (10 ms): the formula's
    // allowance is one frame plus 2 × J, drained half the round trip later.
    r.queue.reported(MS(80));
    r.queue.jitter(480);
    let t = r.tick().await;
    assert_eq!(
        t.pcm,
        vec![0; FRAME_SAMPLES],
        "silence: it no longer speaks"
    );
    let a = t.allowance.expect("the allowance changed");
    assert_eq!(a.value, MS(40));
    let p = r.out.playback().unwrap();
    assert_eq!(p.end(), Some(r.t0 + MS(60 + 40)));
    assert_eq!(p.drained_at(), Some(r.t0 + MS(60 + 40 + 40)));
    // The drained acknowledgement comes at drained, not before.
    let gen = r.drained.recv().await.unwrap();
    assert_eq!(gen, 1);
    assert_eq!(Instant::now(), r.t0 + MS(140));
}

#[tokio::test(start_paused = true)]
async fn the_drained_marker_waits_while_the_clock_holds_the_last_chunk() {
    let mut r = Rig::new(None);
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    // 50 ms: the clock cuts it over three ticks.
    audio(&mut ob, 1, "i1", 1200, 3);
    ob.drained(1);
    r.flush(ob).await;
    r.tick().await;
    assert!(r.out.playback().unwrap().waiting, "held by the clock");
    // However late the next tick is, the marker is not due while the clock
    // holds audio of its response.
    tokio::time::sleep(MS(500)).await;
    assert!(r.drained.try_recv().is_err());
    let t = r.tick().await;
    assert_eq!(t.pcm, vec![3; FRAME_SAMPLES]);
    let t = r.tick().await;
    assert_eq!(&t.pcm[..240], &[3; 240]);
    assert_eq!(
        &t.pcm[240..],
        &[0; 240],
        "not padding: the marker is reached"
    );
    assert_eq!(r.drained.recv().await, Some(1));
}

#[tokio::test(start_paused = true)]
async fn a_purge_answers_what_was_heard_as_its_cut_counts_it() {
    // Ten frames of i1 sent (200 ms), then the cut.
    async fn spoken() -> Rig {
        let mut r = Rig::new(None);
        let mut ob = Outbox::default();
        ob.pace(1, Duration::ZERO);
        audio(&mut ob, 1, "i1", 10 * FRAME_SAMPLES, 5);
        audio(&mut ob, 1, "i1", 10 * FRAME_SAMPLES, 6);
        r.flush(ob).await;
        for _ in 0..10 {
            r.tick().await;
        }
        r
    }
    let sent = 10 * FRAME_SAMPLES as u64;
    // A cancel: what was sent, exact to the frame (the paced queue would have
    // counted the whole chunk taken).
    let r = spoken().await;
    assert_eq!(r.out.purge_cut(1, Cut::Sent).audio.get("i1"), Some(&sent));
    // A bound call's barge-in: what left by the allowance before the cut,
    // the allowance one frame before the first receiver report. Cut as the
    // last frame's send ends (200 ms): all of it less one frame.
    let r = spoken().await;
    tokio::time::sleep_until(r.t0 + MS(200)).await;
    let heard = r.out.purge_cut(1, Cut::BargeIn).audio;
    assert_eq!(heard.get("i1"), Some(&(sent - FRAME_SAMPLES as u64)));
    // Cut as the last tick took its frame (180 ms): what had left by then,
    // nine frames, less the allowance.
    let r = spoken().await;
    let heard = r.out.purge_cut(1, Cut::BargeIn).audio;
    assert_eq!(heard.get("i1"), Some(&(sent - 2 * FRAME_SAMPLES as u64)));
    // A stop at a position: the sample playing there. lmgw's report said NTP
    // `ntp` is RTP0 + 960 × 3 (frame 3's start); the phone played 30 ms on
    // from it: frame 4, half through.
    let r = spoken().await;
    let report = UNIX_EPOCH + Duration::from_secs(1_760_000_000);
    r.queue.line(SentLine::new(
        report,
        RTP0 + 3 * FRAME_TICKS,
        RTP0 + 9 * FRAME_TICKS,
    ));
    let at = ntp_ms(report) + 30.0;
    let heard = r.out.purge_cut(1, Cut::At(at)).audio;
    assert_eq!(heard.get("i1"), Some(&(4 * FRAME_SAMPLES as u64 + 240)));
    // Past what was sent: clamped to it. With no line: what was sent.
    let r = spoken().await;
    r.queue.line(SentLine::new(report, RTP0, RTP0));
    let heard = r.out.purge_cut(1, Cut::At(ntp_ms(report) + 10_000.0)).audio;
    assert_eq!(heard.get("i1"), Some(&sent));
    let r = spoken().await;
    let heard = r.out.purge_cut(1, Cut::At(ntp_ms(SystemTime::now()))).audio;
    assert_eq!(heard.get("i1"), Some(&sent));
    // And the clock holds nothing of it any more: silence follows.
    let mut r = spoken().await;
    r.out.purge(1);
    let t = r.tick().await;
    assert_eq!(t.pcm, vec![0; FRAME_SAMPLES]);
    assert_eq!(t.starved, None, "a cut response is not starved");
}

#[tokio::test(start_paused = true)]
async fn a_playout_report_measures_the_delay_drained_follows() {
    let mut r = Rig::new(Some(Duration::from_secs(1)));
    let report = UNIX_EPOCH + Duration::from_secs(1_760_000_000);
    // lmgw's sender report: NTP `report` is RTP0, the clock's start.
    r.queue.line(SentLine::new(report, RTP0, RTP0));
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    audio(&mut ob, 1, "i1", 100 * FRAME_SAMPLES, 9);
    r.flush(ob).await;
    for _ in 0..21 {
        r.tick().await;
    }
    // At 400 ms the phone reports it plays the sample sent at 100 ms: the
    // playout delay is 300 ms (no round trip yet).
    assert_eq!(Instant::now(), r.t0 + MS(400));
    let notes = r
        .out
        .playout(Instant::now(), Some(ntp_ms(report) + 100.0), Some(250.0));
    assert!(notes.is_empty(), "{notes:?}");
    let t = r.tick().await;
    let a = t.allowance.expect("the report changed the allowance");
    assert_eq!((a.value, a.delay), (MS(300), Some(MS(300))));
    let p = r.out.playback().unwrap();
    // The last frame left at 420 ms: its send end plus d.
    assert_eq!(p.drained_at(), Some(r.t0 + MS(440 + 300)));
    // A position not sent yet is ignored, with its note.
    let notes = r
        .out
        .playout(Instant::now(), Some(ntp_ms(report) + 60_000.0), None);
    assert!(matches!(notes.as_slice(), [Note::Ignored(_)]), "{notes:?}");
    // No report for a whole interval of playback: the formula again.
    let mut fell = Vec::new();
    for _ in 0..50 {
        fell.extend(r.tick().await.notes);
    }
    assert!(
        matches!(fell.as_slice(), [Note::FellBack { .. }]),
        "{fell:?}"
    );
    let p = r.out.playback().unwrap();
    let send_end = r.t0 + MS(20 * 71 + 20);
    assert_eq!(
        p.end(),
        Some(send_end + FRAME),
        "one frame, no receiver report"
    );
}

#[tokio::test(start_paused = true)]
async fn a_close_stops_the_clock_before_the_last_events_go() {
    let mut r = Rig::new(None);
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    audio(&mut ob, 1, "i1", FRAME_SAMPLES, 4);
    delta(1, &mut ob, "i1", "unsaid");
    audio(&mut ob, 1, "i1", FRAME_SAMPLES, 4);
    r.flush(ob).await;
    assert_eq!(r.tick().await.pcm, vec![4; FRAME_SAMPLES]);
    // The session's last event, then the close: the writer stops the clock
    // before it sends what the lane holds.
    let mut ob = Outbox::default();
    ob.send(closed());
    ob.flush(&r.out).await;
    r.out.close(Close::GoingAway);
    let _ = (&mut r._task).await;
    assert_eq!(r.sent(), ["lmgw.session.closed"]);
    // No tick after it: the delta and the audio behind it never leave.
    assert!(r.try_tick().await.is_none());
    // What was sent still counts for the heard the session's end settles.
    assert_eq!(
        r.out.purge(1).audio.get("i1"),
        Some(&(FRAME_SAMPLES as u64))
    );
}

/// A bound call's `lmgw.session.closed`.
fn closed() -> ServerEvent {
    ServerEvent::LmgwSessionClosed(lmgw_api_types::realtime::SessionClosed {
        kind: lmgw_api_types::realtime::ClosedKind::Closed,
        by: None,
        message: "bye".into(),
    })
}

/// The session queues `lmgw.session.closed`, flushes and only then closes
/// (`say_closed`, `say_closed_now`): the writer takes the event off the lane
/// before any close reaches it, and the clock stops right there, so no tick
/// in between sends a delta or `starved` behind it.
#[tokio::test(start_paused = true)]
async fn lmgw_session_closed_stops_the_clock_where_it_goes() {
    let mut r = Rig::new(None);
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    audio(&mut ob, 1, "i1", FRAME_SAMPLES, 4);
    delta(1, &mut ob, "i1", "unsaid");
    audio(&mut ob, 1, "i1", FRAME_SAMPLES, 4);
    r.flush(ob).await;
    assert_eq!(r.tick().await.pcm, vec![4; FRAME_SAMPLES]);
    let mut ob = Outbox::default();
    ob.send(closed());
    r.flush(ob).await;
    assert_eq!(r.sent(), ["lmgw.session.closed"]);
    // The next tick finds the clock stopped: the delta behind the event, and
    // the padding's `starved`, never leave.
    assert!(r.try_tick().await.is_none());
    assert_eq!(
        r.out.purge(1).audio.get("i1"),
        Some(&(FRAME_SAMPLES as u64)),
        "what was sent still counts"
    );
}

/// A barge-in confirmed after the answer's last frame left: the phone
/// still plays its tail, and heard is what left by the allowance before the
/// cut — more than the end less the allowance, and never more than was
/// sent (§5.4).
#[tokio::test(start_paused = true)]
async fn a_barge_in_confirmed_after_the_last_frame_left_hears_up_to_the_cut_less_the_allowance() {
    async fn ended() -> Rig {
        let mut r = Rig::new(None);
        let mut ob = Outbox::default();
        ob.pace(1, Duration::ZERO);
        audio(&mut ob, 1, "i1", 10 * FRAME_SAMPLES, 5);
        ob.drained(1);
        r.flush(ob).await;
        for _ in 0..10 {
            r.tick().await;
        }
        r
    }
    let sent = 10 * FRAME_SAMPLES as u64;
    // The last frame left at 180 ms and ended at 200; the cut at 210 ms,
    // the allowance one frame: what left by 190 ms, half of the last frame.
    let r = ended().await;
    tokio::time::sleep_until(r.t0 + MS(210)).await;
    let heard = r.out.purge_cut(1, Cut::BargeIn).audio;
    assert_eq!(heard.get("i1"), Some(&(sent - FRAME_SAMPLES as u64 / 2)));
    // At 230 ms its tail had played: all of it, clamped to what was sent.
    let r = ended().await;
    tokio::time::sleep_until(r.t0 + MS(230)).await;
    let heard = r.out.purge_cut(1, Cut::BargeIn).audio;
    assert_eq!(heard.get("i1"), Some(&sent));
}

/// A tick that runs late skips the slots due longer ago than the playout
/// allowance — the phone would discard them — and sends the rest: none of
/// the answer is lost (its audio leaves in the frames sent), heard and the
/// window count only what was sent, and a position in a skipped slot maps
/// to no send time (§5.5).
#[tokio::test(start_paused = true)]
async fn a_late_tick_skips_the_slots_the_phone_would_discard() {
    let mut r = Rig::new(None);
    let report = UNIX_EPOCH + Duration::from_secs(1_760_000_000);
    r.queue.line(SentLine::new(report, RTP0, RTP0));
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    for v in 1..=10 {
        audio(&mut ob, 1, "i1", FRAME_SAMPLES, v);
    }
    r.flush(ob).await;
    assert_eq!(r.tick().await.pcm, vec![1; FRAME_SAMPLES]);
    // The next tick comes 100 ms late, at 120 ms: slots 1 to 6 are due, 100
    // to 0 ms ago. The allowance is one frame (no receiver report yet):
    // slots 1 to 4 are older, and skipped; 5 (20 ms ago) and 6 are sent.
    let (stale, ticks) = r.late_tick(MS(100)).await;
    assert_eq!(stale, 4);
    let pcm: Vec<i16> = ticks.iter().map(|t| t.pcm[0]).collect();
    assert_eq!(pcm, [2, 3], "the audio goes on in the frames sent");
    // Heard counts what was sent: three frames.
    let p = r.out.playback().unwrap();
    assert_eq!(p.end(), Some(r.t0 + MS(120) + FRAME + FRAME));
    // A position in a skipped slot (slot 2, 40 to 60 ms) was never sent.
    let notes = r
        .out
        .playout(Instant::now(), Some(ntp_ms(report) + 50.0), None);
    assert!(
        matches!(notes.as_slice(), [Note::Ignored(why)] if why.contains("skipped")),
        "{notes:?}"
    );
    assert_eq!(
        r.out.purge(1).audio.get("i1"),
        Some(&(3 * FRAME_SAMPLES as u64))
    );
}

/// A clause with no audio after the response's last chunk: its delta waits
/// ahead of the drained marker, for the writer to release at once. Its
/// marker is queued, so a tick that comes before the writer took the delta
/// (here the second slot of one late tick) finds the response no longer
/// speaking: silence, no `starved: true`. The tick that took the chunk
/// woke the writer for the delta.
#[tokio::test(start_paused = true)]
async fn a_delta_ahead_of_the_drained_marker_is_no_starved_response() {
    let mut r = Rig::new(None);
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    audio(&mut ob, 1, "i1", FRAME_SAMPLES, 6);
    delta(1, &mut ob, "i1", " tail");
    ob.drained(1);
    r.flush(ob).await;
    let (stale, ticks) = r.late_tick(MS(20)).await;
    assert_eq!((stale, ticks.len()), (0, 2));
    assert_eq!(ticks[0].pcm, vec![6; FRAME_SAMPLES]);
    assert_eq!(ticks[1].pcm, vec![0; FRAME_SAMPLES]);
    assert_eq!(ticks[1].starved, None, "the marker is queued: not starved");
    assert_eq!(labels(&ticks[0]), ["output_audio_buffer.started"]);
    assert!(ticks[1].events.is_empty());
    tokio::task::yield_now().await;
    assert_eq!(r.sent(), ["T: tail"], "released at once, woken by the tick");
}

/// `response.done` for `resp_1`, as the core queues it.
fn done(ob: &mut Outbox) {
    use crate::realtime::protocol::{
        Inf, MaxOutputTokens, ResponseObject, ResponseStatus, ServerEvent,
    };
    ob.send(ServerEvent::ResponseDone {
        response: Box::new(ResponseObject {
            id: "resp_1".into(),
            object: "realtime.response".into(),
            status: ResponseStatus::Completed,
            status_details: None,
            output: Vec::new(),
            conversation_id: None,
            output_modalities: Vec::new(),
            max_output_tokens: MaxOutputTokens::Inf(Inf::Inf),
            audio: None,
            usage: None,
            metadata: None,
        }),
    });
}

/// The call's output buffer (calls design §6.2): `started` goes with the
/// response's first frame, from the tick; `stopped` follows the
/// `response.done` of a response that played out, `cleared` that of one a
/// purge cut, each once, from the writer; a response that never sent a
/// frame gets none of them.
#[tokio::test(start_paused = true)]
async fn the_output_buffer_starts_with_the_first_frame_and_stops_or_clears_after_the_done() {
    // Played out.
    let mut r = Rig::new(None);
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    audio(&mut ob, 1, "i1", 2 * FRAME_SAMPLES, 1);
    r.flush(ob).await;
    assert!(!r.out.speaking("resp_1"), "no frame yet");
    let t = r.tick().await;
    assert_eq!(labels(&t), ["output_audio_buffer.started"]);
    let v: Value = serde_json::from_str(&t.events[0]).unwrap();
    assert_eq!(v["response_id"], "resp_1");
    assert!(v["event_id"].as_str().unwrap().starts_with("event_"));
    assert!(labels(&r.tick().await).is_empty(), "once");
    assert!(r.out.speaking("resp_1"));
    let mut ob = Outbox::default();
    done(&mut ob);
    done(&mut ob);
    r.flush(ob).await;
    assert_eq!(
        r.sent(),
        [
            "response.done",
            "output_audio_buffer.stopped",
            "response.done"
        ],
        "right after the done, once"
    );
    assert!(!r.out.speaking("resp_1"));

    // Cut.
    let mut r = Rig::new(None);
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    audio(&mut ob, 1, "i1", 4 * FRAME_SAMPLES, 1);
    r.flush(ob).await;
    r.tick().await;
    r.out.purge(1);
    let mut ob = Outbox::default();
    done(&mut ob);
    r.flush(ob).await;
    assert_eq!(r.sent(), ["response.done", "output_audio_buffer.cleared"]);

    // Cut before its first frame: nothing.
    let mut r = Rig::new(None);
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    audio(&mut ob, 1, "i1", FRAME_SAMPLES, 1);
    r.flush(ob).await;
    r.out.purge(1);
    assert!(labels(&r.tick().await).is_empty());
    let mut ob = Outbox::default();
    done(&mut ob);
    r.flush(ob).await;
    assert_eq!(r.sent(), ["response.done"]);
}

/// Once the writer released a response's drained marker its answer is
/// over: an allowance that grows afterwards (a receiver report's jitter)
/// does not move its window's end, which the barge-in gate judges by.
#[tokio::test(start_paused = true)]
async fn a_finished_responses_window_stays_where_its_marker_left_it() {
    let mut r = Rig::new(None);
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    audio(&mut ob, 1, "i1", 2 * FRAME_SAMPLES, 1);
    ob.drained(1);
    r.flush(ob).await;
    r.tick().await;
    r.tick().await;
    // Send end 40 ms, the allowance one frame: drained at 60 ms.
    let t = r.tick().await;
    assert!(t.events.is_empty());
    assert_eq!(r.drained.recv().await, Some(1));
    let end = r.out.playback().unwrap().end();
    assert_eq!(end, Some(r.t0 + MS(60)));
    // A report of 100 ms of jitter: the formula's allowance is 220 ms now.
    r.queue.jitter(4_800);
    let t = r.tick().await;
    assert_eq!(t.allowance.map(|a| a.value), Some(MS(220)));
    assert_eq!(r.out.playback().unwrap().end(), end, "not reopened");
}
