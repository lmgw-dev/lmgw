//! The writer's lanes, purges and pacing (realtime design §4.3, §8.2), on a
//! channel standing in for the socket. The pacing tests run on tokio's paused
//! clock, so "released at 600 ms" is exact.

use futures::StreamExt;
use serde_json::Value;

use super::*;
use crate::realtime::protocol::PartRef;

fn event(n: u32) -> ServerEvent {
    ServerEvent::RateLimitsUpdated {
        rate_limits: vec![Value::from(n)],
    }
}

fn at(item: &str) -> PartRef {
    PartRef {
        response_id: "resp_1".into(),
        item_id: item.into(),
        output_index: 0,
        content_index: 0,
    }
}

/// Queue 100 ms of audio (2400 samples) of `item`, generation `gen`.
fn chunk(ob: &mut Outbox, gen: u64, item: &str) {
    ob.send_audio(gen, at(item), bytes::Bytes::from(vec![0u8; 4800]));
}

fn transcript(item: &str, text: &str) -> ServerEvent {
    ServerEvent::OutputAudioTranscriptDelta {
        at: at(item),
        delta: text.into(),
    }
}

/// The `rate_limits[0]` of every text frame the writer sent, until it
/// ends.
async fn sent(mut rx: futures::channel::mpsc::UnboundedReceiver<Message>) -> Vec<u64> {
    let mut out = Vec::new();
    while let Some(m) = rx.next().await {
        if let Message::Text(t) = m {
            let v: Value = serde_json::from_str(t.as_str()).unwrap();
            out.push(v["rate_limits"][0].as_u64().unwrap());
        }
    }
    out
}

/// What one frame was: its `type`, and its `delta` for a transcript.
fn label(m: &Message) -> String {
    let Message::Text(t) = m else {
        return format!("{m:?}");
    };
    let v: Value = serde_json::from_str(t.as_str()).unwrap();
    match v["type"].as_str().unwrap() {
        "response.output_audio.delta" => "A".into(),
        "response.output_audio_transcript.delta" => format!("T:{}", v["delta"].as_str().unwrap()),
        "rate_limits.updated" => format!("E{}", v["rate_limits"][0]),
        other => other.into(),
    }
}

/// Frames as they arrive, each with the paused clock's milliseconds since
/// `start`; ends at `n` frames.
async fn timed(
    rx: &mut futures::channel::mpsc::UnboundedReceiver<Message>,
    start: Instant,
    n: usize,
) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    while out.len() < n {
        let m = rx.next().await.expect("a frame");
        out.push((start.elapsed().as_millis() as u64, label(&m)));
    }
    out
}

#[tokio::test]
async fn a_purge_drops_its_own_generation_and_nothing_of_another() {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let (out, mut drained, task) = spawn(tx, Arc::new(Ids::new()));
    // Queued in the core's order: response 1's output and marker, then
    // response 2's — and the cancel of response 2 arrives before the writer
    // took any of it. Response 1's output still goes out.
    out.purge(2);
    let mut ob = Outbox::default();
    ob.send_purgeable(1, event(1));
    ob.drained(1);
    ob.send(event(10));
    ob.send_purgeable(2, event(2));
    ob.send_purgeable(3, event(3));
    ob.drained(3);
    ob.flush(&out).await;
    drop(out);
    task.await.unwrap();
    assert_eq!(sent(rx).await, [1, 10, 3]);
    assert_eq!(drained.recv().await, Some(1));
    assert_eq!(drained.recv().await, Some(3));
}

#[tokio::test]
async fn anything_of_a_generation_that_left_is_heard_a_transcript_delta_too() {
    // B3 review 1: what a barge-in owes again hangs on this.
    let (tx, _rx) = futures::channel::mpsc::unbounded();
    let (out, mut drained, _task) = spawn(tx, Arc::new(Ids::new()));
    assert!(!out.heard(5));
    let mut ob = Outbox::default();
    ob.send_purgeable(5, event(5));
    ob.drained(5);
    ob.flush(&out).await;
    assert_eq!(drained.recv().await, Some(5));
    assert!(out.heard(5) && !out.heard(4) && !out.heard(6));
    let sent = out.purge(5);
    assert!(sent.heard && sent.audio.is_empty());
    // A generation nothing of which left was not heard.
    assert!(!out.purge(6).heard);
}

#[tokio::test]
async fn the_drained_marker_is_reported_after_everything_before_it_left() {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let (out, mut drained, _task) = spawn(tx, Arc::new(Ids::new()));
    let mut ob = Outbox::default();
    ob.send(event(1));
    ob.send(event(2));
    ob.drained(7);
    ob.flush(&out).await;
    assert_eq!(drained.recv().await, Some(7));
    // Both events were handed to the socket by then.
    for want in [1, 2] {
        let Some(Message::Text(t)) = rx.next().await else {
            panic!("a text frame");
        };
        let v: Value = serde_json::from_str(t.as_str()).unwrap();
        assert_eq!(v["rate_limits"][0], want);
    }
}

#[test]
fn a_purged_generation_is_forgotten_once_a_later_one_is_taken() {
    let mut p = paced::Purged::default();
    p.add(2);
    assert!(!p.drops(1, true));
    assert!(p.drops(2, true));
    assert!(!p.drops(2, false), "a marker is never dropped");
    assert!(!p.drops(3, true));
    assert!(p.is_empty());
}

#[tokio::test]
async fn audio_waits_as_pcm_and_leaves_as_base64() {
    use crate::realtime::audio::pcm::{decode_pcm16, pcm16_to_le_bytes};
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let (out, _drained, _task) = spawn(tx, Arc::new(Ids::new()));
    let pcm: Vec<i16> = (0..2400).map(|i| (i * 7 - 8000) as i16).collect();
    let mut ob = Outbox::default();
    ob.pace(1, Duration::from_secs(1));
    ob.send_audio(1, at("item_a"), bytes::Bytes::from(pcm16_to_le_bytes(&pcm)));
    ob.flush(&out).await;
    let Some(Message::Text(t)) = rx.next().await else {
        panic!("a text frame");
    };
    let v: Value = serde_json::from_str(t.as_str()).unwrap();
    assert_eq!(v["type"], "response.output_audio.delta");
    assert_eq!(v["item_id"], "item_a");
    assert_eq!(decode_pcm16(v["delta"].as_str().unwrap()).unwrap(), pcm);
}

#[tokio::test(start_paused = true)]
async fn the_lead_goes_at_once_then_audio_leaves_as_it_plays() {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let (out, mut drained, _task) = spawn(tx, Arc::new(Ids::new()));
    let start = Instant::now();
    let mut ob = Outbox::default();
    ob.pace(1, Duration::from_millis(500));
    // Clause 1 (1 s) with its transcript, clause 2 (0.5 s) with its own.
    ob.send_purgeable(1, transcript("item_a", "Hello,"));
    for _ in 0..10 {
        chunk(&mut ob, 1, "item_a");
    }
    ob.send_purgeable(1, transcript("item_a", " world."));
    for _ in 0..5 {
        chunk(&mut ob, 1, "item_a");
    }
    ob.drained(1);
    ob.flush(&out).await;
    let frames = timed(&mut rx, start, 17).await;
    let at = |i: usize| frames[i].0;
    assert_eq!(frames[0], (0, "T:Hello,".into()));
    // The first 500 ms at once, then one chunk per 100 ms.
    for i in 1..=5 {
        assert_eq!(frames[i], (0, "A".into()), "{frames:?}");
    }
    for (i, want) in (6..=10).zip([100, 200, 300, 400, 500]) {
        assert_eq!(frames[i], (want, "A".into()), "{frames:?}");
    }
    // The second clause's transcript travels with its first chunk, not
    // right after the first clause's last.
    assert_eq!(frames[11], (600, "T: world.".into()), "{frames:?}");
    assert_eq!(frames[12], (600, "A".into()));
    assert_eq!(at(16), 1000);
    // Drained when the client has played it all — the paced send's end plus
    // the lead it holds then — not when the audio was queued, nor when the
    // last chunk left (owner's decision Q1).
    assert_eq!(drained.recv().await, Some(1));
    assert_eq!(start.elapsed().as_millis(), 1500);
}

#[tokio::test(start_paused = true)]
async fn the_lanes_keep_moving_while_audio_waits() {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let (out, _drained, _task) = spawn(tx, Arc::new(Ids::new()));
    let start = Instant::now();
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    for _ in 0..20 {
        chunk(&mut ob, 1, "item_a");
    }
    ob.flush(&out).await;
    assert_eq!(timed(&mut rx, start, 1).await, [(0, "A".into())]);
    // Two seconds of audio wait; a ping and another event do not.
    tokio::time::sleep(Duration::from_millis(50)).await;
    out.ping();
    ob.send(event(9));
    ob.flush(&out).await;
    let next = timed(&mut rx, start, 2).await;
    assert_eq!(next[0].0, 50);
    assert!(next[0].1.starts_with("Ping"), "{next:?}");
    assert_eq!(next[1], (50, "E9".into()));
    // And the window was never taken: 64 more events flush at once.
    for n in 0..64 {
        ob.send(event(n));
    }
    ob.flush(&out).await;
    assert_eq!(start.elapsed().as_millis(), 50);
}

#[tokio::test(start_paused = true)]
async fn a_purge_says_what_left_and_stops_the_rest() {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let (out, mut drained, _task) = spawn(tx, Arc::new(Ids::new()));
    let start = Instant::now();
    let mut ob = Outbox::default();
    ob.pace(4, Duration::from_millis(300));
    for _ in 0..5 {
        chunk(&mut ob, 4, "item_a");
    }
    for _ in 0..5 {
        chunk(&mut ob, 4, "item_b");
    }
    ob.drained(4);
    ob.flush(&out).await;
    // 0 ms: three chunks; 100, 200, 300, 400: one each — item_a's five,
    // and two of item_b's by 450 ms.
    let frames = timed(&mut rx, start, 7).await;
    assert_eq!(frames.last().unwrap().0, 400);
    tokio::time::sleep_until(start + Duration::from_millis(450)).await;
    let left = out.purge(4);
    assert_eq!(left.audio.get("item_a"), Some(&(5 * 2400)));
    assert_eq!(left.audio.get("item_b"), Some(&(2 * 2400)));
    assert!(left.heard);
    // Nothing more of it leaves; the marker still comes, and other output
    // goes on.
    assert_eq!(drained.recv().await, Some(4));
    ob.send(event(5));
    ob.flush(&out).await;
    assert_eq!(timed(&mut rx, start, 1).await, [(450, "E5".into())]);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(rx.try_recv().is_err(), "nothing else was sent");
    // A purge of a generation with nothing queued is empty.
    let none = out.purge(5);
    assert!(none.audio.is_empty() && !none.heard);
}

#[tokio::test(start_paused = true)]
async fn a_stalled_socket_re_bases_the_pacing_instead_of_bursting() {
    // A socket that takes one frame at a time, as fast as the test reads.
    let (tx, mut rx) = futures::channel::mpsc::channel(0);
    let (out, _drained, _task) = spawn(tx, Arc::new(Ids::new()));
    let start = Instant::now();
    let mut ob = Outbox::default();
    ob.pace(1, Duration::from_millis(300));
    for _ in 0..40 {
        chunk(&mut ob, 1, "item_a");
    }
    ob.flush(&out).await;
    // The lead at once, then real time.
    let first = timed_bounded(&mut rx, start, 5).await;
    assert_eq!(
        first.iter().map(|f| f.0).collect::<Vec<_>>(),
        [0, 0, 0, 100, 200]
    );
    // The client stops reading for two seconds: the socket stalls.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = timed_bounded(&mut rx, start, 8).await;
    let at: Vec<u64> = after.iter().map(|f| f.0).collect();
    // What was in the socket's buffer, then a fresh lead from the moment
    // the stall ended — not the twenty chunks that fell due during it in
    // one burst — then one chunk every 100 ms again.
    let burst = at.iter().filter(|&&t| t == 2200).count();
    assert!(burst <= 4, "{at:?}");
    let tail = &at[burst..];
    assert_eq!(
        *tail,
        [2300, 2400, 2500, 2600, 2700, 2800, 2900][..tail.len()]
    );
}

/// [`timed`] for a bounded socket.
async fn timed_bounded(
    rx: &mut futures::channel::mpsc::Receiver<Message>,
    start: Instant,
    n: usize,
) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    while out.len() < n {
        let m = rx.next().await.expect("a frame");
        out.push((start.elapsed().as_millis() as u64, label(&m)));
    }
    out
}

#[tokio::test(start_paused = true)]
async fn a_schedule_ends_with_its_response() {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let (out, mut drained, _task) = spawn(tx, Arc::new(Ids::new()));
    // A speaking response that said nothing: its marker is due at once,
    // and its schedule goes with it (review m8).
    let mut ob = Outbox::default();
    ob.pace(1, Duration::from_millis(500));
    ob.drained(1);
    ob.flush(&out).await;
    assert_eq!(drained.recv().await, Some(1));
    assert_eq!(out.paced.pacers(), 0);
    // One that spoke: when its marker leaves behind the audio.
    ob.pace(2, Duration::ZERO);
    chunk(&mut ob, 2, "item_a");
    chunk(&mut ob, 2, "item_a");
    ob.drained(2);
    ob.flush(&out).await;
    let start = Instant::now();
    assert_eq!(timed(&mut rx, start, 1).await, [(0, "A".into())]);
    assert_eq!(out.paced.pacers(), 1, "the second chunk is still due");
    assert_eq!(drained.recv().await, Some(2));
    assert_eq!(out.paced.pacers(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_close_sends_what_was_said_and_drops_what_is_paced() {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let (out, _drained, task) = spawn(tx, Arc::new(Ids::new()));
    let start = Instant::now();
    let mut ob = Outbox::default();
    // Five seconds of audio, paced; an event behind it.
    ob.pace(1, Duration::ZERO);
    for _ in 0..50 {
        chunk(&mut ob, 1, "item_a");
    }
    ob.send(event(9));
    ob.flush(&out).await;
    out.close(1000, "bye".into());
    task.await.unwrap();
    let mut frames = Vec::new();
    while let Ok(m) = rx.try_recv() {
        frames.push(label(&m));
    }
    // The event goes out, and the close follows at once — not after the
    // paced audio (review m8).
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0], "E9");
    assert!(frames[1].starts_with("Close"), "{frames:?}");
    assert_eq!(start.elapsed(), Duration::ZERO);

    // The same when the session simply ends.
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let (out, _drained, task) = spawn(tx, Arc::new(Ids::new()));
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    for _ in 0..50 {
        chunk(&mut ob, 1, "item_a");
    }
    ob.flush(&out).await;
    drop(out);
    task.await.unwrap();
    let mut audio = 0;
    while let Ok(m) = rx.try_recv() {
        audio += usize::from(label(&m) == "A");
    }
    assert!(audio <= 1, "{audio} chunks pushed out at the end");
    assert_eq!(start.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn the_ack_waits_for_the_window_end_and_the_window_outlives_it() {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let (out, mut drained, _task) = spawn(tx, Arc::new(Ids::new()));
    let start = Instant::now();
    assert_eq!(out.playback(), None, "nothing has paced yet");
    // Lead 0: each chunk leaves just in time, and the last one still plays
    // for its own length after it left.
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    for _ in 0..3 {
        chunk(&mut ob, 1, "item_a");
    }
    ob.drained(1);
    ob.flush(&out).await;
    let at: Vec<u64> = timed(&mut rx, start, 3).await.iter().map(|f| f.0).collect();
    assert_eq!(at, [0, 100, 200]);
    assert_eq!(drained.recv().await, Some(1));
    assert_eq!(start.elapsed().as_millis(), 300);
    // The record is still there after the ack: input captured inside the
    // window may be judged after it (§6.4).
    let p = out.playback().unwrap();
    assert_eq!(p.gen, 1);
    assert_eq!(p.first, Some(start));
    assert_eq!(p.end(), Some(start + Duration::from_millis(300)));
    assert_eq!(p.ended, None);
}

#[tokio::test(start_paused = true)]
async fn the_record_says_while_audio_of_it_still_waits() {
    // B3 review 8: with a lead of 0 the end of what left is always "now"
    // while the rest of the answer waits — the window must not read as over.
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let (out, _drained, _task) = spawn(tx, Arc::new(Ids::new()));
    let start = Instant::now();
    let mut ob = Outbox::default();
    ob.pace(1, Duration::ZERO);
    for _ in 0..3 {
        chunk(&mut ob, 1, "item_a");
    }
    ob.drained(1);
    ob.flush(&out).await;
    timed(&mut rx, start, 1).await;
    let p = out.playback().unwrap();
    assert_eq!(p.end(), Some(start + Duration::from_millis(100)));
    assert!(p.waiting, "two chunks still queued");
    timed(&mut rx, start, 2).await;
    assert!(!out.playback().unwrap().waiting, "all of it left");
    // A purge ends the window: nothing waits for a cut answer.
    let mut ob = Outbox::default();
    ob.pace(2, Duration::ZERO);
    for _ in 0..3 {
        chunk(&mut ob, 2, "item_b");
    }
    ob.flush(&out).await;
    timed(&mut rx, start, 1).await;
    assert!(out.playback().unwrap().waiting);
    out.purge(2);
    assert!(!out.playback().unwrap().waiting);
}

#[tokio::test(start_paused = true)]
async fn a_marker_behind_audio_that_already_left_waits_for_the_playback() {
    // Package B review 11: the audio left before the marker reached the
    // writer — the queue is empty, and the client still plays.
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let (out, mut drained, _task) = spawn(tx, Arc::new(Ids::new()));
    let start = Instant::now();
    let mut ob = Outbox::default();
    ob.pace(1, Duration::from_millis(500));
    for _ in 0..3 {
        chunk(&mut ob, 1, "item_a");
    }
    ob.flush(&out).await;
    // The whole 300 ms answer is inside the lead: it leaves at once.
    assert_eq!(timed(&mut rx, start, 3).await[2].0, 0);
    tokio::time::sleep(Duration::from_millis(50)).await;
    ob.drained(1);
    ob.flush(&out).await;
    assert_eq!(drained.recv().await, Some(1));
    // At its audio's end — not at once, and not a whole lead after the
    // send: the client never held more than the answer.
    assert_eq!(start.elapsed().as_millis(), 300);
    // A text response's marker (nothing paced) is still due at once, and
    // the spoken window stays the one recorded.
    ob.drained(2);
    ob.flush(&out).await;
    assert_eq!(drained.recv().await, Some(2));
    assert_eq!(start.elapsed().as_millis(), 300);
    assert_eq!(out.playback().map(|p| p.gen), Some(1));
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_response_s_marker_never_holds_back_the_next_one() {
    // The trap of package B review 11: a purge keeps the marker and drops
    // the schedule; with the ack due at the window's end, the stale marker
    // must be due at once — it is queued ahead of the next response's audio.
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let (out, mut drained, _task) = spawn(tx, Arc::new(Ids::new()));
    let start = Instant::now();
    let mut ob = Outbox::default();
    ob.pace(1, Duration::from_millis(200));
    for _ in 0..20 {
        chunk(&mut ob, 1, "item_a");
    }
    ob.drained(1);
    ob.flush(&out).await;
    timed(&mut rx, start, 3).await;
    tokio::time::sleep_until(start + Duration::from_millis(150)).await;
    let left = out.purge(1);
    assert_eq!(left.audio.get("item_a"), Some(&(3 * 2400)));
    let p = out.playback().unwrap();
    assert_eq!(p.end(), Some(start + Duration::from_millis(150)), "cut");
    // The next response paces right behind the stale marker.
    ob.pace(2, Duration::from_millis(200));
    chunk(&mut ob, 2, "item_b");
    ob.drained(2);
    ob.flush(&out).await;
    assert_eq!(drained.recv().await, Some(1));
    assert_eq!(start.elapsed().as_millis(), 150, "the stale ack at once");
    assert_eq!(timed(&mut rx, start, 1).await, [(150, "A".into())]);
    assert_eq!(out.playback().map(|p| p.gen), Some(2));
    // Generation 2 plays its 100 ms and is acknowledged at its end.
    assert_eq!(drained.recv().await, Some(2));
    assert_eq!(start.elapsed().as_millis(), 250);

    // A generation purged before its lead reached the writer starts ended,
    // and its marker is due at once too.
    out.purge(3);
    ob.pace(3, Duration::from_millis(200));
    chunk(&mut ob, 3, "item_c");
    ob.drained(3);
    ob.flush(&out).await;
    assert_eq!(drained.recv().await, Some(3));
    assert_eq!(start.elapsed().as_millis(), 250);
    let p = out.playback().unwrap();
    assert_eq!(
        (p.gen, p.first, p.end()),
        (3, None, Some(start + Duration::from_millis(250)))
    );
    assert!(rx.try_recv().is_err(), "nothing of it was sent");
}

/// A socket whose peer closed at `peer_closed`: every frame after it is
/// refused, as tungstenite refuses a write after a received close; the
/// close itself is counted (it flushes the queued close reply).
#[derive(Clone, Default)]
struct PeerSink {
    peer_closed: Arc<std::sync::atomic::AtomicBool>,
    frames: Arc<std::sync::Mutex<Vec<String>>>,
    closes: Arc<std::sync::atomic::AtomicUsize>,
}

impl futures::Sink<Message> for PeerSink {
    type Error = &'static str;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn start_send(self: std::pin::Pin<&mut Self>, m: Message) -> Result<(), Self::Error> {
        if self.peer_closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("send after closing");
        }
        self.frames.lock().unwrap().push(label(&m));
        Ok(())
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.closes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::task::Poll::Ready(Ok(()))
    }
}

/// WP11 binding review M1: after the peer's close a send fails, and the
/// writer goes quiet instead of ending — it keeps the socket, sends
/// nothing, never holds a sender up, keeps what left on record for the
/// heard cut, and closes the socket only once every handle is gone.
#[tokio::test(start_paused = true)]
async fn a_peer_s_close_quiets_the_writer_until_the_session_lets_go() {
    use std::sync::atomic::Ordering;
    let sink = PeerSink::default();
    let (out, _drained, task) = spawn(sink.clone(), Arc::new(Ids::new()));
    let mut ob = Outbox::default();
    // Two chunks within the lead leave; the rest waits to be paced.
    ob.pace(1, Duration::from_millis(200));
    for _ in 0..30 {
        chunk(&mut ob, 1, "item_a");
    }
    ob.flush(&out).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let left = sink.frames.lock().unwrap().len();
    assert!(left >= 2, "the lead left: {left}");

    // The client closes; the next frame is refused.
    sink.peer_closed.store(true, Ordering::SeqCst);
    for n in 0..200 {
        ob.send(event(n));
    }
    // More than the window: a writer that stopped reading would hold this.
    tokio::time::timeout(Duration::from_secs(5), ob.flush(&out))
        .await
        .expect("no sender waits on a quiet writer");
    out.ping();
    out.close(1000, "bye".into());
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(
        !task.is_finished(),
        "the socket is kept while a handle lives"
    );
    assert_eq!(sink.closes.load(Ordering::SeqCst), 0);
    assert_eq!(sink.frames.lock().unwrap().len(), left, "nothing more sent");
    // What left is still on record: the heard cut reads it.
    let sent = out.purge(1);
    assert!(
        sent.heard && sent.audio.get("item_a").is_some_and(|&n| n >= 4800),
        "the audio that left stays recorded: {:?}",
        sent.audio
    );

    drop(out);
    task.await.unwrap();
    assert_eq!(
        sink.closes.load(Ordering::SeqCst),
        1,
        "closed once, at the end"
    );
}
