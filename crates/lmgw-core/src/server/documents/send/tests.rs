use super::*;
use crate::extract::convert::budget::answers::Answers;
use http_body_util::BodyExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn who() -> Arc<str> {
    Arc::from("key:1")
}

/// A [`To`] whose end is kept in `seen`.
fn heard() -> (To, Arc<Mutex<Option<Ended>>>) {
    let seen = Arc::new(Mutex::new(None));
    let s = seen.clone();
    let to = To::default().then(Box::new(move |how| {
        Box::pin(async move {
            *s.lock().unwrap() = Some(how);
        })
    }));
    (to, seen)
}

/// Whether `a` holds no share any more, waiting as [`settled`] does: a
/// streamed body's share goes on its answer thread after its last chunk,
/// which the reader can have before that thread drops it (and the end is
/// told before the last chunk, `feed::whole_first`).
async fn released(a: &Answers) -> bool {
    for _ in 0..200 {
        if a.facts().holding == 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

async fn settled(seen: &Mutex<Option<Ended>>) -> Option<Ended> {
    for _ in 0..200 {
        if let Some(e) = *seen.lock().unwrap() {
            return Some(e);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

#[tokio::test]
async fn the_body_comes_whole_and_its_share_goes_back_once_it_is_read() {
    let a = Answers::new(Some(1 << 30));
    let share = a.hold(&who(), 1 << 29).await.unwrap();
    let data: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
    let (to, seen) = heard();
    let (b, deadline) = body(Bytes::from(data.clone()), share, to).await;
    assert_eq!(
        deadline.to_str().unwrap(),
        "60 (ANSWER_WRITE_GRACE_S + 1 s per ANSWER_MIN_RATE_BYTES_PER_S)"
    );
    assert_eq!(hyper::body::Body::size_hint(&b).exact(), Some(200_000));
    // Settled to the body's size at once.
    assert_eq!(a.facts().held_bytes, 200_000);
    let got = b.collect().await.unwrap().to_bytes();
    assert_eq!(got.as_ref(), &data[..]);
    assert_eq!(settled(&seen).await, Some(Ended::Whole));
    assert_eq!(a.facts().holding, 0);
}

#[tokio::test(start_paused = true)]
async fn a_client_that_does_not_read_loses_the_answer_at_its_deadline() {
    let a = Answers::new(Some(1 << 30));
    let share = a.hold(&who(), 1 << 29).await.unwrap();
    let cut = WriteCut::default();
    let (to, seen) = heard();
    let to = To {
        cut: Some(cut.clone()),
        ..to
    };
    let (mut b, _) = body(Bytes::from(vec![7u8; 4 * CHUNK]), share, to).await;
    // One chunk read, then nothing.
    let first = b.frame().await.unwrap().unwrap();
    assert_eq!(first.into_data().unwrap().len(), CHUNK);
    assert_eq!(a.facts().holding, 1);
    tokio::time::sleep(answers::write_deadline(4 * CHUNK as u64) + Duration::from_secs(1)).await;
    assert_eq!(a.facts().holding, 0, "the share went back at the deadline");
    assert!(cut.is_cut(), "and the connection is ended (SF2)");
    // The row hears the deadline, not a whole answer (N-b): what was
    // handed over is the chunk read and the one in the channel; the third
    // waits, so the last was never reached.
    assert_eq!(
        *seen.lock().unwrap(),
        Some(Ended::Late {
            sent: 2 * CHUNK as u64,
            len: 4 * CHUNK as u64
        })
    );
    // What the channel still held comes, then the body ends in an error.
    let mut ended = false;
    while let Some(f) = b.frame().await {
        if f.is_err() {
            ended = true;
            break;
        }
    }
    assert!(ended);
}

/// Connection cut review, nit 2: a deadline missed while the last chunk
/// waits for room is heard as `Late` (a 499 `write_deadline` row), never as
/// a whole answer told before it.
#[tokio::test(start_paused = true)]
async fn a_deadline_missed_during_the_last_chunk_is_heard_as_late() {
    let a = Answers::new(Some(1 << 30));
    let share = a.hold(&who(), 1 << 29).await.unwrap();
    let (to, seen) = heard();
    // Two chunks, none read: the first fills the channel, the last waits.
    let (b, _) = body(Bytes::from(vec![7u8; 2 * CHUNK]), share, to).await;
    tokio::time::sleep(answers::write_deadline(2 * CHUNK as u64) + Duration::from_secs(1)).await;
    assert_eq!(
        *seen.lock().unwrap(),
        Some(Ended::Late {
            sent: CHUNK as u64,
            len: 2 * CHUNK as u64
        })
    );
    assert_eq!(a.facts().holding, 0);
    drop(b);
}

/// A client that goes away midway is heard as such (N-b).
#[tokio::test]
async fn a_client_that_leaves_midway_is_heard_as_gone() {
    let a = Answers::new(Some(1 << 30));
    let share = a.hold(&who(), 1 << 29).await.unwrap();
    let (to, seen) = heard();
    let (mut b, _) = body(Bytes::from(vec![7u8; 4 * CHUNK]), share, to).await;
    b.frame().await.unwrap().unwrap();
    drop(b);
    match settled(&seen).await {
        Some(Ended::Left { sent, len }) => {
            assert!(sent < len, "{sent} of {len}");
            assert_eq!(len, 4 * CHUNK as u64);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(a.facts().holding, 0);
}

/// SF1: the body is counted before anything is made: made whole (and
/// only as large as it is) when it is at most half the room, else not made
/// at all.
#[test]
fn the_body_is_counted_first_and_made_whole_only_within_half_the_room() {
    let value = serde_json::json!({"content": "x".repeat(10_000)});
    let len = serde_json::to_vec(&value).unwrap().len() as u64;
    match made(&value, 2 * len).unwrap() {
        Made::Whole(v) => {
            assert_eq!(v.len() as u64, len);
            assert_eq!(v.capacity() as u64, len, "one allocation of its size");
        }
        Made::Streamed(..) => panic!("half the room holds it"),
    }
    match made(&value, 2 * len - 1).unwrap() {
        Made::Streamed(_, n) => assert_eq!(n, len),
        Made::Whole(_) => panic!("more than half the room"),
    }
}

/// Records the thread it is written on.
struct OnThread(Arc<Mutex<Option<String>>>, String);

impl serde::Serialize for OnThread {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let name = std::thread::current().name().map(String::from);
        if let Some(n) = name {
            *self.0.lock().unwrap() = Some(n);
        }
        s.serialize_str(&self.1)
    }
}

/// A body larger than half its share is written as it is read: the same
/// bytes, its length announced, the deadline in its header, on a thread of
/// its own, not the runtime's blocking pool (N-f).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_too_large_beside_its_answer_is_written_as_it_is_read() {
    let a = Answers::new(Some(1 << 30));
    let text: String = (0..400_000u32)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    let thread = Arc::new(Mutex::new(None));
    let value = OnThread(thread.clone(), text.clone());
    let want = serde_json::to_vec(&text).unwrap();
    *thread.lock().unwrap() = None;
    let share = a.hold(&who(), 300_000).await.unwrap();
    let (to, seen) = heard();
    let r = json(StatusCode::OK, value, share, to).await;
    assert_eq!(
        r.headers()[WRITE_DEADLINE].to_str().unwrap(),
        "60 (ANSWER_WRITE_GRACE_S + 1 s per ANSWER_MIN_RATE_BYTES_PER_S)"
    );
    let b = r.into_body();
    assert_eq!(
        hyper::body::Body::size_hint(&b).exact(),
        Some(want.len() as u64)
    );
    assert_eq!(
        a.facts().held_bytes,
        want.len() as u64,
        "settled to the body"
    );
    let got = b.collect().await.unwrap().to_bytes();
    assert_eq!(got.as_ref(), &want[..]);
    assert_eq!(settled(&seen).await, Some(Ended::Whole));
    // The answer's thread gives the share back once its write returns
    // (at 32 test threads beside a CPU hog, 2026-11-20, often after the
    // reader had the last byte).
    assert!(released(&a).await, "the share was given back");
    // The count ran on the blocking pool; the write on the answer's thread.
    assert_eq!(thread.lock().unwrap().as_deref(), Some(feed::THREAD));
}

mod connection;
mod reuse;
