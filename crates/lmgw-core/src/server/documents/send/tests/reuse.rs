//! Connection cut review, B1 and nit 1, through a real listener: a
//! keep-alive connection reused after a document answer read whole is
//! never cut for later traffic, and on HTTP/2 a stalled document stream is
//! reset alone, its connection serving other streams on.

use super::*;
use crate::server::conn::PlainListener;
use axum::extract::Request;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A document answer of `len` bytes per request, heard in `seen` (the last
/// one's end), and a slow chat-like stream on `/stream`.
fn app(a: &Answers, len: usize, seen: Arc<Mutex<Option<Ended>>>) -> axum::Router {
    let shares = a.clone();
    let answer = move |req: Request| {
        let (shares, seen) = (shares.clone(), seen.clone());
        async move {
            let share = shares.hold(&who(), 1 << 29).await.unwrap();
            let to = To::of(&req).then(Box::new(move |how| {
                Box::pin(async move {
                    *seen.lock().unwrap() = Some(how);
                })
            }));
            let (b, _) = body(Bytes::from(vec![b'x'; len]), share, to).await;
            axum::response::Response::new(b)
        }
    };
    axum::Router::new()
        .route("/answer", axum::routing::get(answer))
        .route("/stream", axum::routing::get(|| async { stream() }))
}

/// A streamed answer like a chat completion's: a first event of 14 KiB
/// (more than its client's receive buffer), 4 s of nothing (a slow
/// model), then one small event every 10 ms.
fn stream() -> axum::response::Response {
    let events = futures::stream::unfold(0u32, |i| async move {
        if i > 60 {
            return None;
        }
        match i {
            0 => {}
            1 => tokio::time::sleep(Duration::from_secs(4)).await,
            _ => tokio::time::sleep(Duration::from_millis(10)).await,
        }
        let event = if i == 0 {
            format!("data: {}\n\n", "e".repeat(14 << 10))
        } else {
            format!("data: example event {i}\n\n")
        };
        Some((Ok::<_, std::io::Error>(Bytes::from(event)), i + 1))
    });
    axum::response::Response::new(axum::body::Body::from_stream(events))
}

async fn listen(app: axum::Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            PlainListener::new(tcp, None),
            app.into_make_service_with_connect_info::<Conn>(),
        )
        .await;
    });
    (addr, server)
}

/// A task that wakes every 10 ms, so the paused clock moves in steps of
/// 10 ms at most: left alone it leaps to the next timer whenever a socket's
/// wake comes a moment late (here: past the answer's deadline before the
/// next request was even read).
fn ticking() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
}

/// One response's head off `c`, read byte by byte.
async fn head(c: &mut tokio::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        c.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

/// The reviewer's scenario: a small conversion answer read whole, then on
/// the same connection a streamed chat answer across the conversion's
/// deadline, its client a little behind (the first event still in the
/// gateway's send queue at the deadline), then read to its end. It must
/// never be cut (the round before reset it: its rule read any queued byte
/// of the connection).
#[tokio::test(start_paused = true)]
async fn a_stream_on_a_connection_after_an_answer_read_whole_is_never_cut() {
    let a = Answers::new(Some(1 << 30));
    let seen = Arc::new(Mutex::new(None));
    let (addr, server) = listen(app(&a, 524, seen.clone())).await;
    let ticks = ticking();
    // The smallest receive buffer the kernel allows, so most of the first
    // event stays in the gateway's send queue while its client is behind.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(1).unwrap();
    let mut c = socket.connect(addr).await.unwrap();
    c.write_all(b"GET /answer HTTP/1.1\r\nHost: example.invalid\r\n\r\n")
        .await
        .unwrap();
    let h = head(&mut c).await;
    assert!(h.contains("content-length: 524"), "{h}");
    let mut answer = vec![0u8; 524];
    c.read_exact(&mut answer).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), Some(Ended::Whole));
    // The chat request 2 s before the answer's deadline.
    let deadline = answers::write_deadline(524);
    tokio::time::sleep(deadline - Duration::from_secs(2)).await;
    c.write_all(b"GET /stream HTTP/1.1\r\nHost: example.invalid\r\n\r\n")
        .await
        .unwrap();
    let h = head(&mut c).await;
    assert!(h.starts_with("HTTP/1.1 200"), "{h}");
    // Behind past the deadline, then reading on to the stream's end. The
    // deadline judged, the clock runs on in real time: a paused clock
    // leaps to the read's timeout whenever a socket's wake comes late.
    tokio::time::sleep(Duration::from_secs(3)).await;
    tokio::time::resume();
    let mut got = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    let end = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match c.read(&mut buf).await {
                Ok(0) => return Err("closed".to_string()),
                Ok(n) => {
                    got.extend_from_slice(&buf[..n]);
                    if got.ends_with(b"0\r\n\r\n") {
                        return Ok(());
                    }
                }
                Err(e) => return Err(format!("{:?}", e.kind())),
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err("not ended within 60 s".into()));
    let tail = String::from_utf8_lossy(&got[got.len().saturating_sub(200)..]).to_string();
    assert_eq!(
        end,
        Ok(()),
        "{} bytes of the stream read, ending {tail:?}",
        got.len()
    );
    let text = String::from_utf8_lossy(&got);
    assert!(text.contains("example event 60"), "{text}");
    ticks.abort();
    server.abort();
}

/// The true case beside it: a small answer whose client never reads it is
/// ended at its deadline, its socket reset. A 256 KiB answer, so that its
/// bytes stay in the gateway's queue (a small one would sit acknowledged in
/// the client's own buffer, where no server can see it unread).
#[cfg(target_os = "linux")]
#[tokio::test(start_paused = true)]
async fn an_answer_whose_client_never_reads_it_is_cut_at_its_deadline() {
    let a = Answers::new(Some(1 << 30));
    let seen = Arc::new(Mutex::new(None));
    let len = 256 << 10;
    let (addr, server) = listen(app(&a, len, seen.clone())).await;
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut c = socket.connect(addr).await.unwrap();
    c.write_all(b"GET /answer HTTP/1.1\r\nHost: example.invalid\r\n\r\n")
        .await
        .unwrap();
    let ticks = ticking();
    tokio::time::sleep(answers::write_deadline(len as u64) + Duration::from_secs(5)).await;
    tokio::time::resume();
    ticks.abort();
    assert_eq!(*seen.lock().unwrap(), Some(Ended::Whole));
    let mut buf = vec![0u8; 1 << 16];
    let end = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match c.read(&mut buf).await {
                Ok(0) => return None,
                Ok(_) => continue,
                Err(e) => return Some(e.kind()),
            }
        }
    })
    .await
    .expect("the gateway's side closed after the deadline");
    assert_eq!(end, Some(std::io::ErrorKind::ConnectionReset));
    server.abort();
}

/// Nit 1: over HTTP/2 (h2c on the plain listener) a document stream whose
/// client does not read it is reset alone at its deadline; a second stream
/// on the same connection is answered after it.
#[tokio::test(start_paused = true)]
async fn on_http2_a_stalled_document_stream_is_reset_and_the_connection_serves_on() {
    use http_body_util::Empty;
    let a = Answers::new(Some(1 << 30));
    let seen = Arc::new(Mutex::new(None));
    let len = 16 << 20;
    let (addr, server) = listen(app(&a, len, seen.clone())).await;
    let ticks = ticking();
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut send, conn) =
        hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .initial_stream_window_size(65_535)
            .handshake::<_, Empty<Bytes>>(hyper_util::rt::TokioIo::new(tcp))
            .await
            .unwrap();
    let driver = tokio::spawn(conn);
    let get = |path: &str| {
        axum::http::Request::builder()
            .uri(format!("http://{addr}{path}"))
            .body(Empty::<Bytes>::new())
            .unwrap()
    };
    let stalled = send.send_request(get("/answer")).await.unwrap();
    assert_eq!(stalled.version(), axum::http::Version::HTTP_2);
    assert_eq!(stalled.status(), 200);
    tokio::time::sleep(answers::write_deadline(len as u64) + Duration::from_secs(5)).await;
    match *seen.lock().unwrap() {
        Some(Ended::Late { sent, len: l }) => assert!(sent < l, "{sent} of {l}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(a.facts().holding, 0, "the share went back");
    tokio::time::resume();
    // The stalled stream ends in an error once what reached it is read.
    let mut body = stalled.into_body();
    let mut read = 0usize;
    let failed = loop {
        match body.frame().await {
            Some(Ok(f)) => read += f.into_data().map_or(0, |d| d.len()),
            Some(Err(_)) => break true,
            None => break false,
        }
    };
    assert!(failed, "the stream was reset ({read} of {len} read)");
    assert!(read < len, "{read}");
    // A second stream on the same connection is answered whole.
    *seen.lock().unwrap() = None;
    let next = send.send_request(get("/stream")).await.unwrap();
    assert_eq!(next.status(), 200);
    let got = next.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8_lossy(&got).contains("example event 60"),
        "{} bytes",
        got.len()
    );
    assert!(!driver.is_finished(), "the connection is still up");
    ticks.abort();
    server.abort();
}
