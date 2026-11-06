//! The transport seam (chat-plane design, "WP3a: the transport seam").
//!
//! Every request a host-agnostic page makes is plain data, a [`Request`]: a
//! method, a path relative to lmgw, headers and a body. A [`Transport`] sends
//! it and answers a [`Response`]: the status, the headers and the body, whole
//! or as a byte stream. The dashboard's transport is the browser's `fetch`
//! ([`super::fetch::Fetch`]); a host whose page cannot reach lmgw itself (an
//! extension sidebar, whose requests go as frames through its background to
//! Kai's proxy) brings its own. The page is handed the transport by its host
//! in context, as an [`Http`] (`use_http`).

use std::cell::RefCell;
use std::rc::Rc;

use futures::future::LocalBoxFuture;
use futures::stream::{LocalBoxStream, StreamExt};
use serde::Serialize;

/// The request's method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

/// A request's body.
#[derive(Clone, Debug, Default)]
pub enum Body {
    #[default]
    Empty,
    /// JSON text, sent as `application/json`.
    Json(String),
    /// Bytes; their type is the request's `content-type` header.
    Bytes(Vec<u8>),
    /// Bytes the browser holds (a picked file, a dropped one): the
    /// dashboard's fetch sends them as they are, without copying them
    /// through the wasm module's memory; a host that relays frames reads
    /// them as bytes. Their type is the blob's own unless a header says one.
    Blob(web_sys::Blob),
}

impl Body {
    /// `value` as JSON text.
    pub fn json<B: Serialize + ?Sized>(value: &B) -> Result<Body, String> {
        serde_json::to_string(value)
            .map(Body::Json)
            .map_err(|e| e.to_string())
    }
}

/// How the caller reads the answer's body; a host that relays frames buffers
/// or relays by it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Read {
    /// All of it at once (JSON, text).
    #[default]
    Whole,
    /// Chunks as they arrive (an SSE stream: the send stream, the feed). It
    /// stays open as long as lmgw sends.
    Stream,
    /// All of it as a browser blob (a picture, a recording, an export): the
    /// dashboard's fetch never copies it through the wasm module's memory.
    Blob,
}

/// One request to lmgw: plain data.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    /// Relative to lmgw: `/chat/api/threads/7`, query included.
    pub path: String,
    /// Names as written; a transport compares them case-insensitively.
    pub headers: Vec<(String, String)>,
    pub body: Body,
    pub read: Read,
    /// Aborts the request (the Stop button, a page that goes, the feed's
    /// close): the browser's own cancel token, which any host in a page can
    /// listen to. A [`Transport`] must honour it (module doc of
    /// [`Transport`]); a caller that wants to know whether it stopped a
    /// request asks the signal (`aborted()`), never the error's words.
    pub signal: Option<web_sys::AbortSignal>,
}

impl Request {
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Request {
            method,
            path: path.into(),
            headers: Vec::new(),
            body: Body::Empty,
            read: Read::Whole,
            signal: None,
        }
    }

    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::Get, path)
    }

    pub fn post(path: impl Into<String>) -> Self {
        Self::new(Method::Post, path)
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn body(mut self, body: Body) -> Self {
        self.body = body;
        self
    }

    pub fn read(mut self, read: Read) -> Self {
        self.read = read;
        self
    }

    pub fn signal(mut self, signal: Option<&web_sys::AbortSignal>) -> Self {
        self.signal = signal.cloned();
        self
    }

    /// The value of header `name` (any case), the first when repeated.
    pub fn header_value(&self, name: &str) -> Option<&str> {
        header_of(&self.headers, name)
    }
}

/// Chunks of a body as they arrive; an `Err` ends it (a dropped
/// connection, an abort).
///
/// **Dropping it cancels the request**: a reader that stops reading (the
/// feed's dead-link timeout, a page that goes) drops the stream, and the
/// transport must then stop the transfer and release what holds it (the
/// browser's fetch cancels the body's reader; a relaying host sends its
/// cancel frame), not keep it open until lmgw ends it.
pub type ByteStream = LocalBoxStream<'static, Result<Vec<u8>, String>>;

/// A response's body, as the transport has it.
pub enum Payload {
    Whole(Vec<u8>),
    Stream(ByteStream),
    Blob(web_sys::Blob),
}

impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Payload::Whole(b) => write!(f, "Whole({} bytes)", b.len()),
            Payload::Stream(_) => f.write_str("Stream"),
            Payload::Blob(_) => f.write_str("Blob"),
        }
    }
}

/// One answer: status, headers and body. Whichever way the transport holds
/// the body, every reader below takes it.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    /// Names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Payload,
}

impl Response {
    /// A whole answer (a relay's buffered one, a fake's).
    pub fn whole(status: u16, headers: Vec<(String, String)>, body: impl Into<Vec<u8>>) -> Self {
        Response {
            status,
            headers: lower(headers),
            body: Payload::Whole(body.into()),
        }
    }

    /// A streamed answer of `chunks`.
    pub fn streamed(status: u16, headers: Vec<(String, String)>, chunks: ByteStream) -> Self {
        Response {
            status,
            headers: lower(headers),
            body: Payload::Stream(chunks),
        }
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The value of header `name` (any case), the first when repeated.
    pub fn header(&self, name: &str) -> Option<&str> {
        header_of(&self.headers, name)
    }

    /// A non-2xx answer read as every helper reads a refusal: the house
    /// `ApiError` (whatever shape the body has), the `401
    /// session_required` hook included.
    pub async fn refusal(self) -> super::Error {
        let status = self.status;
        super::refused(status, &self.text().await.unwrap_or_default())
    }

    /// The body's bytes, all of them.
    pub async fn bytes(self) -> Result<Vec<u8>, String> {
        match self.body {
            Payload::Whole(b) => Ok(b),
            Payload::Stream(mut s) => {
                let mut out = Vec::new();
                while let Some(chunk) = s.next().await {
                    out.extend_from_slice(&chunk?);
                }
                Ok(out)
            }
            Payload::Blob(b) => super::fetch::blob_bytes(&b).await,
        }
    }

    /// The body as text (UTF-8, lossy).
    pub async fn text(self) -> Result<String, String> {
        Ok(String::from_utf8_lossy(&self.bytes().await?).into_owned())
    }

    /// The body as chunks as they arrive; a whole body is one chunk.
    pub fn stream(self) -> ByteStream {
        match self.body {
            Payload::Stream(s) => s,
            Payload::Whole(b) => futures::stream::once(async move { Ok(b) }).boxed_local(),
            Payload::Blob(b) => {
                futures::stream::once(async move { super::fetch::blob_bytes(&b).await })
                    .boxed_local()
            }
        }
    }

    /// The body as a browser blob of the answer's `content-type`.
    pub async fn blob(self) -> Result<web_sys::Blob, String> {
        let kind = self.header("content-type").unwrap_or_default().to_string();
        match self.body {
            Payload::Blob(b) => Ok(b),
            other => {
                let bytes = Response {
                    status: self.status,
                    headers: Vec::new(),
                    body: other,
                }
                .bytes()
                .await?;
                super::fetch::blob_of(&bytes, &kind)
            }
        }
    }
}

fn lower(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v))
        .collect()
}

fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// What sends a page's requests to lmgw.
///
/// The cancel contract, which every transport keeps:
///
/// - **The request's `signal`** ([`Request::signal`]): when it aborts, the
///   transport stops the request at whatever stage it is (no answer yet:
///   `send` resolves `Err`; streaming: the [`ByteStream`] ends with an
///   `Err`). The error's text is the transport's own ("AbortError: …",
///   "cancelled", …): callers tell a stop from a failure by
///   `signal.aborted()`, so a transport need not word it in any way.
/// - **Dropping** the future `send` returned, or the answer's
///   [`ByteStream`], cancels the request too ([`ByteStream`]).
pub trait Transport {
    /// Send `req`. `Err` is a failure to reach lmgw at all (no answer, or
    /// stopped by its signal); a refusal is a [`Response`] with its status.
    fn send(&self, req: Request) -> LocalBoxFuture<'static, Result<Response, String>>;

    /// A URL the browser may load by itself for `path` (an `<img src>`, an
    /// `<audio src>`, a download link), with the same credentials `send`
    /// has. `None` when only `send` reaches lmgw (a host that relays
    /// frames): the page then fetches the bytes and shows them by an object
    /// URL (`super::bytes`).
    fn link(&self, _path: &str) -> Option<String> {
        None
    }
}

thread_local! {
    /// Every transport a host made, by [`Http`]'s index; the first is the
    /// browser's `fetch`. A host makes one or two, so none is ever dropped.
    static TRANSPORTS: RefCell<Vec<Rc<dyn Transport>>> =
        RefCell::new(vec![Rc::new(super::fetch::Fetch)]);
}

/// A handle to a transport: what a host hands the page in context, and what
/// the page's code captures where it starts a request. `Copy`, so a closure
/// takes it along; its JSON helpers are the free functions' (`super::get`,
/// `super::post`, …), refusals and the `401 session_required` hook included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Http(usize);

impl Default for Http {
    fn default() -> Self {
        Http::fetch()
    }
}

impl Http {
    /// The browser's `fetch`, under the base [`super::configure`] set: the
    /// dashboard's, and what a host that hands none gets.
    pub const fn fetch() -> Self {
        Http(0)
    }

    /// Register `transport`; the handle reaches it from now on.
    pub fn new(transport: impl Transport + 'static) -> Self {
        TRANSPORTS.with(|t| {
            let mut t = t.borrow_mut();
            t.push(Rc::new(transport));
            Http(t.len() - 1)
        })
    }

    fn transport(self) -> Rc<dyn Transport> {
        TRANSPORTS.with(|t| t.borrow()[self.0].clone())
    }

    /// Send `req` as it is.
    pub async fn send(self, req: Request) -> super::Result<Response> {
        let fut = self.transport().send(req);
        fut.await.map_err(super::Error::Transport)
    }

    /// Send `req` and keep a 2xx answer; a refusal is read as every helper
    /// reads it (the house `ApiError`, the `401 session_required` hook).
    pub async fn ok(self, req: Request) -> super::Result<Response> {
        admit(self.send(req).await?).await
    }

    /// The transport's own link for `path` (`Transport::link`).
    pub fn link(self, path: &str) -> Option<String> {
        self.transport().link(path)
    }

    pub async fn get<T: serde::de::DeserializeOwned>(
        self,
        path: impl Into<String>,
    ) -> super::Result<T> {
        json_of(self.ok(Request::get(path)).await?).await
    }

    /// GET a plain-text body (an attachment's text).
    pub async fn get_text(self, path: impl Into<String>) -> super::Result<String> {
        self.ok(Request::get(path))
            .await?
            .text()
            .await
            .map_err(bad_body)
    }

    pub async fn post<T: serde::de::DeserializeOwned, B: Serialize + ?Sized>(
        self,
        path: impl Into<String>,
        body: &B,
    ) -> super::Result<T> {
        let body = Body::json(body).map_err(super::Error::Transport)?;
        json_of(self.ok(Request::post(path).body(body)).await?).await
    }

    /// POST whose success is a bare `204`.
    pub async fn post_no_content<B: Serialize + ?Sized>(
        self,
        path: impl Into<String>,
        body: &B,
    ) -> super::Result<()> {
        let body = Body::json(body).map_err(super::Error::Transport)?;
        self.ok(Request::post(path).body(body)).await.map(|_| ())
    }

    /// POST `body` as it is (a file's bytes) and read the JSON answer.
    pub async fn post_body<T: serde::de::DeserializeOwned>(
        self,
        path: impl Into<String>,
        body: Body,
    ) -> super::Result<T> {
        json_of(self.ok(Request::post(path).body(body)).await?).await
    }

    /// POST JSON and take the answer as bytes with its content type (audio).
    pub async fn post_bytes<B: Serialize + ?Sized>(
        self,
        path: impl Into<String>,
        body: &B,
    ) -> super::Result<(Vec<u8>, String)> {
        let body = Body::json(body).map_err(super::Error::Transport)?;
        let resp = self.ok(Request::post(path).body(body)).await?;
        let kind = resp.header("content-type").unwrap_or_default().to_string();
        Ok((resp.bytes().await.map_err(bad_body)?, kind))
    }

    /// GET `path`'s bytes as a browser blob, with the answer's headers (a
    /// download's name is in `content-disposition`).
    pub async fn blob(
        self,
        path: impl Into<String>,
    ) -> super::Result<(web_sys::Blob, Vec<(String, String)>)> {
        let resp = self.ok(Request::get(path).read(Read::Blob)).await?;
        let headers = resp.headers.clone();
        Ok((resp.blob().await.map_err(bad_body)?, headers))
    }
}

fn bad_body(e: String) -> super::Error {
    super::Error::Transport(format!("bad response body: {e}"))
}

/// `resp` when it is a 2xx, else its refusal as every helper reads it (the
/// house `ApiError`, the `401 session_required` hook).
pub(super) async fn admit(resp: Response) -> super::Result<Response> {
    if resp.ok() {
        Ok(resp)
    } else {
        Err(resp.refusal().await)
    }
}

pub(super) async fn json_of<T: serde::de::DeserializeOwned>(resp: Response) -> super::Result<T> {
    let bytes = resp.bytes().await.map_err(bad_body)?;
    serde_json::from_slice(&bytes).map_err(|e| bad_body(e.to_string()))
}

/// The transport the host handed the page (`provide_context(http)`), else
/// the browser's `fetch`. Call where the page is set up, never after an
/// `.await`: a task has no reactive owner to read context from.
///
/// Called without an owner it can only answer the browser's `fetch`, which
/// in a host that relays frames is a request behind the host's back. A debug
/// build says so loudly: a console error, and the count on
/// `<html data-use-http-orphans>`, which the relay drive checks
/// (`scripts/csp-check.sh`).
pub fn use_http() -> Http {
    #[cfg(debug_assertions)]
    if leptos::prelude::Owner::current().is_none() {
        orphan_call();
    }
    leptos::prelude::use_context::<Http>().unwrap_or_default()
}

/// A debug build's alarm for [`use_http`] called without a reactive owner.
#[cfg(debug_assertions)]
fn orphan_call() {
    leptos::logging::error!(
        "use_http() called without a reactive owner: it answers the browser's fetch whatever \
         the host handed the page; read it where the page is set up and carry the handle"
    );
    #[cfg(target_arch = "wasm32")]
    if let Some(html) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    {
        let n = html
            .get_attribute("data-use-http-orphans")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let _ = html.set_attribute("data-use-http-orphans", &(n + 1).to_string());
    }
}
