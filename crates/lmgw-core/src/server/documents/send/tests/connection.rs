//! SF2 through a real listener: a client that reads the answer's head and
//! stops reading loses its connection at the write deadline, closed with a
//! reset, not only the answer's share.

use super::*;
use crate::server::conn::PlainListener;
use axum::extract::Request;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 16 MiB: more than the kernel's buffers and hyper's hold, so the body is
/// still being handed over at its deadline.
const LEN: usize = 16 << 20;

#[tokio::test(start_paused = true)]
async fn a_client_that_reads_the_head_and_stops_has_its_connection_closed_at_the_deadline() {
    match stop_reading(LEN).await {
        Some(Ended::Late { sent, len }) => assert!(sent < len, "{sent} of {len}"),
        other => panic!("{other:?}"),
    }
}

/// An answer small enough for the kernel's send queue to take whole: hyper
/// is done with it and the connection waits for a next request, and at the
/// deadline the connection is ended all the same (the kernel's queue is
/// read on Linux).
#[cfg(target_os = "linux")]
#[tokio::test(start_paused = true)]
async fn an_answer_the_kernel_took_whole_and_nobody_read_is_ended_at_the_deadline() {
    assert_eq!(stop_reading(256 << 10).await, Some(Ended::Whole));
}

/// A client with a 4 KiB receive buffer reads the head of an answer of
/// `len` bytes and stops; past the deadline its socket must be reset, the
/// answer's share back. How the answer's end was heard.
async fn stop_reading(len: usize) -> Option<Ended> {
    let a = Answers::new(Some(1 << 30));
    let (to_row, seen) = heard();
    let row = Arc::new(Mutex::new(Some(to_row)));
    let shares = a.clone();
    let app = axum::Router::new().route(
        "/answer",
        axum::routing::get(move |req: Request| {
            let (shares, row) = (shares.clone(), row.clone());
            async move {
                let share = shares.hold(&who(), 1 << 29).await.unwrap();
                let heard = row.lock().unwrap().take().unwrap();
                let to = To {
                    cut: To::of(&req).cut,
                    ..heard
                };
                let (b, _) = body(Bytes::from(vec![b'x'; len]), share, to).await;
                axum::response::Response::new(b)
            }
        }),
    );
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            PlainListener::new(tcp, None),
            app.into_make_service_with_connect_info::<Conn>(),
        )
        .await
    });
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut client = socket.connect(addr).await.unwrap();
    client
        .write_all(b"GET /answer HTTP/1.1\r\nHost: example.invalid\r\n\r\n")
        .await
        .unwrap();
    // The head, then nothing more is read.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        client.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    assert!(head.starts_with(b"HTTP/1.1 200"), "{head:?}");
    tokio::time::sleep(answers::write_deadline(len as u64) + Duration::from_secs(5)).await;
    assert_eq!(a.facts().holding, 0, "the share went back");
    let heard = *seen.lock().unwrap();
    // The client reads what reached it, then the reset: the gateway's side
    // of the socket is gone, its unsent bytes with it.
    let mut buf = vec![0u8; 1 << 16];
    let mut read = 0usize;
    // Bounded: a socket that stays open past the deadline fails here, not
    // at the runner's timeout.
    let end = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match client.read(&mut buf).await {
                Ok(0) => break None,
                Ok(n) => read += n,
                Err(e) => break Some(e.kind()),
            }
        }
    })
    .await
    .expect("the gateway's side of the socket closed after the deadline");
    assert!(read < len, "{read}");
    assert_eq!(
        end,
        Some(std::io::ErrorKind::ConnectionReset),
        "{read} read"
    );
    server.abort();
    heard
}
