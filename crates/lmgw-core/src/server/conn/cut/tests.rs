use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A connected pair: the server's end and a client whose receive buffer
/// is 4 KiB, so what it does not read stays in the server's send queue.
async fn pair() -> (TcpStream, TcpStream) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client = tokio::spawn(async move {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        socket.connect(addr).await.unwrap()
    });
    let (server, _) = listener.accept().await.unwrap();
    (server, client.await.unwrap())
}

/// An answer of `bytes` written as hyper writes one: begun, its last frame
/// handed over, flushed (which marks its end).
async fn answer(s: &mut Cuttable<TcpStream>, bytes: &[u8]) -> Mark {
    let mark = s.cut().begin();
    s.write_all(bytes).await.unwrap();
    s.cut().last_frame();
    s.flush().await.unwrap();
    mark
}

/// A write that waits for a client that reads nothing, then `end` called
/// on its handle: the write must be woken, fail and reset the socket.
async fn a_waiting_write_ended_by(end: impl FnOnce(&WriteCut, Mark)) {
    let (server, mut client) = pair().await;
    let mut s = Cuttable::new(server);
    let cut = s.cut();
    let mark = cut.begin();
    let writer = tokio::spawn(async move {
        let chunk = vec![7u8; 1 << 20];
        loop {
            if let Err(e) = s.write_all(&chunk).await {
                return (e.kind(), s);
            }
        }
    });
    let mut stalled = false;
    for _ in 0..500 {
        if cut.0.stalled.load(Ordering::SeqCst) {
            stalled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(stalled, "the write waits for a client that reads nothing");
    end(&cut, mark);
    let (kind, s) = tokio::time::timeout(Duration::from_secs(10), writer)
        .await
        .expect("the waiting write was woken")
        .unwrap();
    assert_eq!(kind, std::io::ErrorKind::TimedOut);
    drop(s);
    // The client reads what reached it, then the reset: never an orderly
    // end of the stream.
    let mut buf = vec![0u8; 1 << 16];
    let end = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.read(&mut buf).await {
                Ok(0) => return None,
                Ok(_) => continue,
                Err(e) => return Some(e.kind()),
            }
        }
    })
    .await
    .expect("the socket closed");
    assert_eq!(end, Some(std::io::ErrorKind::ConnectionReset));
}

/// [`WriteCut::cut`] (an answer that missed its deadline mid-body) wakes
/// the waiting write, fails it and resets the socket, its unsent bytes
/// dropped.
#[tokio::test]
async fn a_cut_wakes_a_waiting_write_fails_it_and_resets_the_socket() {
    a_waiting_write_ended_by(|cut, _| cut.cut()).await;
}

/// A tail check of an answer whose rest still waits to be written (no last
/// frame flushed, the write waiting for room) ends the connection the same
/// way (connection cut review, nit 7: named for what it drives).
#[tokio::test]
async fn a_tail_check_ends_an_answer_whose_rest_waits_to_be_written() {
    a_waiting_write_ended_by(|cut, mark| cut.check_tail(mark, u64::MAX, 60)).await;
}

/// A connection whose client read everything and that waits for its next
/// request is left alone when an answer's deadline passes.
#[tokio::test]
async fn a_connection_that_wrote_everything_is_not_cut_when_stalled_is_asked() {
    let (server, mut client) = pair().await;
    let mut s = Cuttable::new(server);
    let cut = s.cut();
    let mark = answer(&mut s, b"example").await;
    let mut got = [0u8; 7];
    client.read_exact(&mut got).await.unwrap();
    assert_eq!(cut.0.end.load(Ordering::SeqCst) - mark.written, 7);
    // Read by its client, nothing waits: the check leaves it alone.
    cut.check_tail(mark, 7, 60);
    client.write_all(b"next").await.unwrap();
    let mut next = [0u8; 4];
    s.read_exact(&mut next).await.unwrap();
    assert_eq!(&next, b"next");
    assert!(!cut.is_cut());
}

/// The bytes the kernel holds for `s` now, waited for until there are
/// some.
#[cfg(target_os = "linux")]
async fn queued_some(s: &Cuttable<TcpStream>) -> u64 {
    for _ in 0..100 {
        if let Some(q) = unsent(s.get_ref()).filter(|q| *q > 0) {
            return q;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("nothing stayed in the send queue");
}

/// The result of a read on a [`Cuttable`] and the stream itself.
#[cfg(target_os = "linux")]
type ReadEnd = (Result<usize, std::io::ErrorKind>, Cuttable<TcpStream>);

/// A read that stays pending on `s` (nothing is sent), handed back with how
/// it ended once the connection is cut or the test drops it.
#[cfg(target_os = "linux")]
fn pending_read(mut s: Cuttable<TcpStream>) -> tokio::task::JoinHandle<ReadEnd> {
    tokio::spawn(async move {
        let mut buf = [0u8; 1];
        let r = s.read(&mut buf).await;
        (r.map_err(|e| e.kind()), s)
    })
}

/// An answer the kernel took whole and its client never read (hyper done,
/// the connection waiting for a next request) is ended at the check: on
/// Linux the kernel's queue is read. A later answer begun since disarms the
/// earlier one's check.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_answer_left_unread_in_the_kernels_queue_is_ended_at_the_check() {
    let (server, mut client) = pair().await;
    let mut s = Cuttable::new(server);
    let cut = s.cut();
    let earlier = cut.begin();
    let mark = answer(&mut s, &vec![7u8; 64 << 10]).await;
    queued_some(&s).await;
    let reader = pending_read(s);
    // The earlier answer's check: a later one began, left alone.
    cut.check_tail(earlier, 64 << 10, 60);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!cut.is_cut(), "a later answer began after the earlier one");
    // The answer itself, unread: ended, and the waiting read fails.
    cut.check_tail(mark, 64 << 10, 60);
    let (r, s) = tokio::time::timeout(Duration::from_secs(10), reader)
        .await
        .expect("the check woke the waiting read")
        .unwrap();
    assert_eq!(r.unwrap_err(), std::io::ErrorKind::TimedOut);
    assert!(cut.is_cut());
    drop(s);
    let mut buf = vec![0u8; 1 << 16];
    let end = loop {
        match client.read(&mut buf).await {
            Ok(0) => break None,
            Ok(_) => continue,
            Err(e) => break Some(e.kind()),
        }
    };
    assert_eq!(end, Some(std::io::ErrorKind::ConnectionReset));
}

/// Review B1: an answer read whole, then a later response on the same
/// keep-alive connection (a chat stream) of fewer than 16 KiB whose bytes
/// still sit in the kernel's queue at the answer's deadline. The connection
/// must not be cut: those bytes are not the answer's (the round before cut
/// it, as its rule read any queued byte).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_later_response_still_queued_does_not_cut_an_answer_that_was_read() {
    let (server, mut client) = pair().await;
    let mut s = Cuttable::new(server);
    let cut = s.cut();
    let mark = answer(&mut s, b"example answer").await;
    let mut got = [0u8; 14];
    client.read_exact(&mut got).await.unwrap();
    // The later response, not read yet (its client is a little behind).
    s.write_all(&vec![b'e'; 12 << 10]).await.unwrap();
    s.flush().await.unwrap();
    queued_some(&s).await;
    let reader = pending_read(s);
    cut.check_tail(mark, 14, 60);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !cut.is_cut(),
        "a later response's queued bytes cut the connection"
    );
    // The client reads on, to the end of the later response.
    let mut later = vec![0u8; 12 << 10];
    client.read_exact(&mut later).await.unwrap();
    reader.abort();
}

/// A byte read after the answer's mark disarms its check, even with the
/// answer's own bytes still queued: an HTTP/1 client sends its next
/// request only once it has taken the answer.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_byte_read_after_the_answer_disarms_its_check() {
    let (server, mut client) = pair().await;
    let mut s = Cuttable::new(server);
    let cut = s.cut();
    let mark = answer(&mut s, &vec![7u8; 64 << 10]).await;
    queued_some(&s).await;
    client.write_all(b"next").await.unwrap();
    let mut next = [0u8; 4];
    s.read_exact(&mut next).await.unwrap();
    let reader = pending_read(s);
    cut.check_tail(mark, 64 << 10, 60);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!cut.is_cut());
    reader.abort();
}

fn at(answer: u64, written: u64, read: u64) -> Mark {
    Mark {
        answer,
        written,
        read,
    }
}

fn now(answers: u64, read: u64, written: u64, end: u64, waits: bool) -> Now {
    Now {
        answers,
        read,
        written,
        end,
        waits,
    }
}

/// The judgement's rules by counts: only the answer's own bytes, only
/// while the connection did nothing since.
#[test]
fn the_verdict_judges_only_the_answers_own_bytes_while_nothing_happened_since() {
    let m = at(3, 1_000, 500);
    let flushed = now(3, 500, 1_600, 1_600, false);
    // The answer (head and body 600 B) flushed, 200 B of it still queued.
    assert_eq!(
        verdict(m, flushed, Some(200)),
        Verdict::Queued {
            own: 200,
            bytes: 600
        }
    );
    // A TLS queue holds ciphertext: the figure never passes the answer.
    assert_eq!(
        verdict(m, flushed, Some(650)),
        Verdict::Queued {
            own: 600,
            bytes: 600
        }
    );
    // Everything acknowledged, or the queue not read on this system.
    assert_eq!(verdict(m, flushed, Some(0)), Verdict::Left);
    assert_eq!(verdict(m, flushed, None), Verdict::Left);
    // A later response written after the answer: its queued bytes are not
    // the answer's.
    let later = now(3, 500, 1_700, 1_600, false);
    assert_eq!(verdict(m, later, Some(100)), Verdict::Left);
    // A byte read after the mark, or a later answer begun.
    assert_eq!(
        verdict(m, now(3, 501, 1_600, 1_600, false), Some(200)),
        Verdict::Left
    );
    assert_eq!(
        verdict(m, now(4, 500, 1_600, UNKNOWN, true), Some(200)),
        Verdict::Left
    );
    // The last frame not flushed: a waiting write is the answer's own.
    assert_eq!(
        verdict(m, now(3, 500, 1_300, UNKNOWN, true), None),
        Verdict::Waits
    );
    assert_eq!(
        verdict(m, now(3, 500, 1_300, UNKNOWN, false), Some(50)),
        Verdict::Queued {
            own: 50,
            bytes: 300
        }
    );
}

/// Re-review nit: a late tail check of an earlier answer, asked after a
/// later answer began and asked for its own, leaves the later one's in
/// place.
#[test]
fn a_late_check_of_an_earlier_answer_never_replaces_a_later_one_s() {
    let cut = WriteCut::default();
    let earlier = cut.begin();
    let later = cut.begin();
    cut.check_tail(later, 14, 60);
    cut.check_tail(earlier, 64 << 10, 61);
    let tail = cut.0.tail.lock().unwrap().expect("the later check waits");
    assert_eq!((tail.mark, tail.len, tail.limit_s), (later, 14, 60));
    assert!(cut.0.armed.load(Ordering::SeqCst));
    // Alone, the earlier answer's check is not asked for at all.
    let cut = WriteCut::default();
    let earlier = cut.begin();
    let _later = cut.begin();
    cut.check_tail(earlier, 64 << 10, 61);
    assert!(cut.0.tail.lock().unwrap().is_none());
    assert!(!cut.0.armed.load(Ordering::SeqCst));
}
