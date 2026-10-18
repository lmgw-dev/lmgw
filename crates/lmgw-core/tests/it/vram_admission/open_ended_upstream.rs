//! A container that can stream `/v1/completions` without end

use super::*;

use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::{Body, Bytes};
use axum::extract::{Request as HttpRequest, State};
use axum::http::{header, Method};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};

/// One frame of the stream a legacy completion sends.
const FRAME: &str = "data: {\"choices\":[{\"text\":\"x\",\"index\":0}]}\n\n";

/// What a test holds on to: the switch that turns the stream on.
pub(super) struct OpenEnded {
    streaming: Arc<AtomicBool>,
}

impl OpenEnded {
    /// From now on, `POST /v1/completions` is answered with a stream of
    /// frames that never ends by itself, whatever the buffers between the
    /// container and the client hold. Only the relay can end it: by closing
    /// the connection when the client has gone, the one thing the test is
    /// about.
    pub(super) fn stream_without_end(&self) {
        self.streaming.store(true, Ordering::SeqCst);
    }
}

/// Make the chat model's container one that [`OpenEnded::stream_without_end`]
/// can turn into an endless stream, in front of the fixture's first mock. Call
/// it before anything starts the model: the registry is pointed at the new
/// container, which answers `/slots` for the model that runs on it and hands
/// every other route to `f.first`, so what a test mounts there still answers.
///
/// A wiremock answer is a whole body, so a test that wants a stream still
/// open when its client leaves can only make the body longer than every
/// buffer between the two — a guess about hyper's read size, the relay's
/// channel and the kernel's sockets. This container sends frames for as long
/// as anyone reads them: the relay writes to the client until a write fails,
/// and a stream that merely went quiet would never be written to again, so
/// the hang-up would go unnoticed.
pub(super) async fn open_ended_upstream(f: &Fixture) -> OpenEnded {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let streaming = Arc::new(AtomicBool::new(false));
    let front = Arc::new(Front {
        world: f.world.clone(),
        port,
        behind: f.first.address().to_string(),
        client: reqwest::Client::new(),
        streaming: streaming.clone(),
    });
    let app = Router::new().fallback(answer).with_state(front);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // The fixture's allocator hands out its mocks' ports; this one hands out
    // the new container's, once.
    let ports = Mutex::new(Some(port));
    f.state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        f.podman.clone(),
        reqwest::Client::new(),
        Arc::new(move || {
            ports.lock().unwrap().take().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    "the open-ended container serves one start — an unexpected extra start",
                )
            })
        }),
    )));
    OpenEnded { streaming }
}

struct Front {
    world: Arc<Mutex<World>>,
    port: u16,
    /// The fixture's first mock, which answers whatever this container does
    /// not.
    behind: String,
    client: reqwest::Client,
    streaming: Arc<AtomicBool>,
}

async fn answer(State(front): State<Arc<Front>>, req: HttpRequest) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path();
    if path == "/slots" {
        return Json(slots(&front.world, front.port)).into_response();
    }
    if path == "/v1/completions"
        && parts.method == Method::POST
        && front.streaming.load(Ordering::SeqCst)
    {
        // Many frames to a chunk, so that filling the buffers is not a
        // million tiny writes; the chunk size decides nothing.
        let chunk = Bytes::from(FRAME.repeat(256));
        let frames = futures::stream::repeat(Ok::<_, std::convert::Infallible>(chunk));
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(frames))
            .unwrap();
    }

    let url = format!(
        "http://{}{}",
        front.behind,
        parts.uri.path_and_query().map_or("", |p| p.as_str())
    );
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let mut forwarded = front.client.request(parts.method, url).body(bytes);
    if let Some(content_type) = parts.headers.get(header::CONTENT_TYPE) {
        forwarded = forwarded.header(header::CONTENT_TYPE, content_type);
    }
    let answer = forwarded.send().await.expect("the first mock answers");
    let mut out = Response::builder().status(answer.status());
    if let Some(content_type) = answer.headers().get(header::CONTENT_TYPE) {
        out = out.header(header::CONTENT_TYPE, content_type);
    }
    out.body(Body::from(answer.bytes().await.unwrap())).unwrap()
}
