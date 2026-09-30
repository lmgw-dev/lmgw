//! llama-server's native HTTP API, as the benchmark drives it (benchmark
//! design §4, §5): `/health`, `/props`, `/slots`, `/tokenize`, `/detokenize`,
//! `/apply-template`, `/v1/chat/completions` and streamed `/completion`, all
//! at the server's root.
//!
//! **No overall timeout.** A 131k-token prefill takes minutes on a large
//! model, and any bound picked here would be a guess that cuts a real
//! measurement short. Requests end when the server answers or when the run
//! is canceled: every call is raced against the job's [`Cancel`], and a
//! canceled call's future is *dropped*, which closes the connection — for a
//! streamed completion that stops the generation on the server as well. The
//! `reqwest::Client` passed in may carry a connect timeout; that one is
//! fine, since a refused connection is not a measurement.
//!
//! **Connections** (benchmark design §13 decision 62). llama-server closes
//! the connection after every streamed response, although the response's
//! headers say `Keep-Alive: timeout=5, max=100`: its chunked content
//! provider returns `false` after the final chunk, which cpp-httplib reads
//! as a canceled write and answers by closing the socket. hyper believes
//! the header and pools the connection whenever it has read the final
//! chunk, so a request sent right after a stream — the measured request
//! after a slot reset — could go out on a socket the server had already
//! closed, and fail with "connection closed before message completed"
//! without ever reaching a slot. So every streamed request says
//! `Connection: close`, and hyper never pools what the server is about to
//! close. A request that still loses its connection before any response
//! arrives is sent once more, logged, and listed in `results.retried`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::StreamExt;
use lmgw_api_types::bench::RetriedRequest;
use serde_json::{json, Value};

use super::corpus;
use super::stream::{parse_chunk, StreamRecord};
use crate::agent::Cancel;
use crate::sse::SseDecoder;

/// Why an engine step did not produce its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchError {
    /// The run was canceled; whatever was in flight has been dropped.
    Canceled,
    /// Anything else, with the message the owner sees.
    Failed(String),
}

impl std::fmt::Display for BenchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BenchError::Canceled => f.write_str("canceled"),
            BenchError::Failed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for BenchError {}

impl From<String> for BenchError {
    fn from(m: String) -> Self {
        BenchError::Failed(m)
    }
}

/// `e`'s message followed by every cause under it (`source()`), joined with
/// ": ". reqwest's own message names the URL and nothing else — "error
/// sending request for url (…)" — while the why (connection closed before
/// message completed, connection reset by peer, …) is further down. A cause
/// whose text is already shown is not repeated.
pub fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        let m = c.to_string();
        if !m.is_empty() && !out.contains(&m) {
            out.push_str(": ");
            out.push_str(&m);
        }
        cause = c.source();
    }
    out
}

/// A failed request as the owner reads it: what was asked, then the whole
/// cause chain.
fn failed(what: &str, e: &(dyn std::error::Error + 'static)) -> BenchError {
    BenchError::Failed(format!("{what}: {}", error_chain(e)))
}

/// Whether a request that got no response failed on its connection — the
/// kind a fresh connection cures, so it is worth sending once more: hyper's
/// "connection closed before message completed", a connection closed or
/// canceled under the request, a reset, an abort, a broken pipe. A refused
/// or timed-out connect is not: nothing is listening, and a second attempt
/// would only say so again.
pub fn resendable(e: &reqwest::Error) -> bool {
    if e.is_connect() || e.is_timeout() {
        return false;
    }
    let mut cause = std::error::Error::source(e);
    while let Some(c) = cause {
        if let Some(h) = c.downcast_ref::<hyper::Error>() {
            if h.is_incomplete_message() || h.is_closed() || h.is_canceled() {
                return true;
            }
        }
        if let Some(io) = c.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            if matches!(
                io.kind(),
                ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof
            ) {
                return true;
            }
        }
        cause = c.source();
    }
    false
}

/// Times a request is sent at most: once, and once more after a broken
/// connection ([`resendable`]). The error of a request that fails both
/// times names both attempts.
const SENDS: u32 = 2;

/// The resends so far, and the stage they belong to.
#[derive(Default)]
struct Resends {
    stage: String,
    list: Vec<RetriedRequest>,
}

/// A non-streamed answer, whatever its status: a probe judges a 500 rather
/// than failing on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpAnswer {
    pub status: u16,
    pub body: String,
    /// Request sent → body read.
    pub ms: u64,
}

impl HttpAnswer {
    pub fn ok(&self) -> bool {
        self.status == 200
    }

    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }
}

/// One bench server.
#[derive(Clone)]
pub struct LlamaClient {
    http: reqwest::Client,
    root: String,
    cancel: Cancel,
    resends: Arc<Mutex<Resends>>,
}

impl LlamaClient {
    /// `base_url` is the server's root (`http://127.0.0.1:PORT`), with or
    /// without a trailing slash.
    pub fn new(http: reqwest::Client, base_url: &str, cancel: Cancel) -> Self {
        Self {
            http,
            root: base_url.trim_end_matches('/').to_string(),
            cancel,
            resends: Arc::default(),
        }
    }

    /// The step the requests from now on belong to, for a resend's record.
    pub fn set_stage(&self, stage: &str) {
        let mut r = self.resends.lock().unwrap_or_else(|p| p.into_inner());
        stage.clone_into(&mut r.stage);
    }

    /// The requests sent a second time since the last call, oldest first.
    pub fn take_retried(&self) -> Vec<RetriedRequest> {
        let mut r = self.resends.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut r.list)
    }

    /// Send the request `build` makes, calling `sending` just before each
    /// attempt (a timer's start). A failure before any response arrived that
    /// a fresh connection can cure ([`resendable`]) sends it again, up to
    /// [`SENDS`] times in all; each resend is logged and kept for
    /// `results.retried`, never silent.
    async fn send(
        &self,
        what: &str,
        build: impl Fn() -> reqwest::RequestBuilder,
        mut sending: impl FnMut(),
    ) -> Result<reqwest::Response, BenchError> {
        let mut earlier: Vec<String> = Vec::new();
        for attempt in 1..=SENDS {
            sending();
            let e = match build().send().await {
                Ok(resp) => return Ok(resp),
                Err(e) => e,
            };
            let why = error_chain(&e);
            if attempt == SENDS || !resendable(&e) || self.cancel.is_raised() {
                let mut m = format!("{what}: {why}");
                if !earlier.is_empty() {
                    m.push_str(&format!(
                        " (attempt {attempt} of {SENDS}; before it: {})",
                        earlier.join("; ")
                    ));
                }
                return Err(BenchError::Failed(m));
            }
            let stage = {
                let mut r = self.resends.lock().unwrap_or_else(|p| p.into_inner());
                let stage = r.stage.clone();
                r.list.push(RetriedRequest {
                    stage: stage.clone(),
                    request: what.to_string(),
                    error: why.clone(),
                });
                stage
            };
            tracing::warn!(
                "benchmark, {stage}: {what} lost its connection before any response \
                 ({why}); sending it again"
            );
            earlier.push(why);
        }
        unreachable!("the last attempt returns")
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn cancel(&self) -> &Cancel {
        &self.cancel
    }

    /// Await `fut` unless the run is canceled first (and then drop it).
    ///
    /// A failure while the cancel is raised is the cancel: the hold's abort
    /// raises it and removes the container at once, so the request in flight
    /// breaks before the guard's next look at the flag — and that broken
    /// connection is not the server's error (review finding 9).
    pub async fn guarded<T>(
        &self,
        fut: impl std::future::Future<Output = Result<T, BenchError>>,
    ) -> Result<T, BenchError> {
        match self.cancel.guard(fut).await {
            None => Err(BenchError::Canceled),
            Some(Err(BenchError::Failed(_))) if self.cancel.is_raised() => {
                Err(BenchError::Canceled)
            }
            Some(r) => r,
        }
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<HttpAnswer, BenchError> {
        let url = format!("{}{path}", self.root);
        let what = format!("{method} {path}");
        self.guarded(async {
            let mut started = Instant::now();
            let build = || {
                let req = self.http.request(method.clone(), &url);
                match body {
                    Some(b) => req.json(b),
                    None => req,
                }
            };
            let resp = self.send(&what, build, || started = Instant::now()).await?;
            let status = resp.status().as_u16();
            let body = resp
                .text()
                .await
                .map_err(|e| failed(&format!("{what}: reading the body"), &e))?;
            Ok(HttpAnswer {
                status,
                body,
                ms: started.elapsed().as_millis() as u64,
            })
        })
        .await
    }

    /// A JSON body from a 200, or an error naming the endpoint and status.
    async fn json_200(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, BenchError> {
        let what = format!("{method} {path}");
        let ans = self.request(method, path, body).await?;
        if !ans.ok() {
            return Err(BenchError::Failed(format!(
                "{what}: HTTP {}: {}",
                ans.status,
                ans.body.trim()
            )));
        }
        ans.json().ok_or_else(|| {
            BenchError::Failed(format!("{what}: the answer is not JSON: {}", ans.body))
        })
    }

    /// `GET /health` → whether it answered 200.
    pub async fn health(&self) -> Result<bool, BenchError> {
        match self.request(reqwest::Method::GET, "/health", None).await {
            Ok(a) => Ok(a.ok()),
            Err(BenchError::Canceled) => Err(BenchError::Canceled),
            Err(BenchError::Failed(_)) => Ok(false),
        }
    }

    pub async fn props(&self) -> Result<Value, BenchError> {
        self.json_200(reqwest::Method::GET, "/props", None).await
    }

    /// `GET /slots`; `None` when the server has the endpoint switched off
    /// (`--no-slots` answers 501), which is not an error — `/props` still
    /// says enough.
    pub async fn slots(&self) -> Result<Option<Value>, BenchError> {
        let ans = self.request(reqwest::Method::GET, "/slots", None).await?;
        Ok(if ans.ok() { ans.json() } else { None })
    }

    /// `POST /tokenize` of plain text, special tokens not added and special
    /// strings read as text (`parse_special: false` — official llama.cpp
    /// honours it, ik_llama.cpp does not; see [`Self::tokenize_corpus`]).
    pub async fn tokenize(&self, text: &str) -> Result<Vec<u32>, BenchError> {
        self.tokenize_content(json!(text), false).await
    }

    /// The corpus as prompts are cut from it (§4.1, §13 decision 58): the
    /// text sent as [`corpus::pieces`] — so neither engine can read a quoted
    /// chat-template marker as a control token, and both tokenize it alike
    /// — and the BOS the vocabulary adds, if any, found by tokenizing one
    /// word with and without special tokens.
    pub async fn tokenize_corpus(&self, text: &str) -> Result<corpus::Corpus, BenchError> {
        let tokens = self
            .tokenize_content(json!(corpus::pieces(text)), false)
            .await?;
        let word = "The";
        let with = self.tokenize_content(json!(word), true).await?;
        let without = self.tokenize_content(json!(word), false).await?;
        Ok(corpus::Corpus {
            prefix: corpus::special_prefix(&with, &without),
            tokens,
        })
    }

    /// `POST /tokenize` of a string or an array of strings (tokenized one
    /// by one and joined).
    async fn tokenize_content(
        &self,
        content: Value,
        add_special: bool,
    ) -> Result<Vec<u32>, BenchError> {
        let v = self
            .json_200(
                reqwest::Method::POST,
                "/tokenize",
                Some(&json!({
                    "content": content,
                    "add_special": add_special,
                    "parse_special": false,
                })),
            )
            .await?;
        v.get("tokens")
            .and_then(Value::as_array)
            .ok_or_else(|| BenchError::Failed("POST /tokenize: no 'tokens' array".into()))?
            .iter()
            .map(|t| {
                t.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| {
                        BenchError::Failed(format!("POST /tokenize: not a token id: {t}"))
                    })
            })
            .collect()
    }

    /// `POST /detokenize`: token ids back to text.
    pub async fn detokenize(&self, tokens: &[u32]) -> Result<String, BenchError> {
        let v = self
            .json_200(
                reqwest::Method::POST,
                "/detokenize",
                Some(&json!({ "tokens": tokens })),
            )
            .await?;
        v.get("content")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| BenchError::Failed("POST /detokenize: no 'content'".into()))
    }

    /// `POST /apply-template`, whatever the status (ik answered 500 for
    /// gemma-4, §2.1).
    pub async fn apply_template(&self, body: &Value) -> Result<HttpAnswer, BenchError> {
        self.request(reqwest::Method::POST, "/apply-template", Some(body))
            .await
    }

    /// `POST /v1/chat/completions`, non-streamed, whatever the status.
    pub async fn chat(&self, body: &Value) -> Result<HttpAnswer, BenchError> {
        self.request(reqwest::Method::POST, "/v1/chat/completions", Some(body))
            .await
    }

    /// A streamed `POST /completion` to its final chunk.
    pub async fn completion(&self, body: &Value) -> Result<StreamRecord, BenchError> {
        let rec = Mutex::new(StreamRecord::new(Instant::now()));
        self.guarded(self.stream_into(body, &rec)).await?;
        Ok(rec.into_inner().unwrap_or_else(|p| p.into_inner()))
    }

    /// A streamed `POST /completion`, folded into `rec` chunk by chunk as it
    /// arrives. **Not** raced against the cancel: the caller owns that, so
    /// the mixed phase can drop a stream on its own schedule and still read
    /// what it recorded. `rec.sent` is reset just before the request goes
    /// out (again, when it is sent again).
    ///
    /// `Connection: close`, because llama-server closes the connection after
    /// the stream anyway (see the module's doc). It returns at the final
    /// chunk rather than reading on to the end of the body: the connection
    /// cannot be reused either way, and there is nothing after that chunk.
    pub async fn stream_into(
        &self,
        body: &Value,
        rec: &Mutex<StreamRecord>,
    ) -> Result<(), BenchError> {
        let url = format!("{}/completion", self.root);
        let mut body = body.clone();
        body["stream"] = Value::Bool(true);
        let build = || {
            self.http
                .post(&url)
                .header(reqwest::header::CONNECTION, "close")
                .json(&body)
        };
        let resp = self
            .send("POST /completion", build, || {
                lock(rec).sent = Instant::now();
            })
            .await?;
        let status = resp.status().as_u16();
        if status != 200 {
            let text = resp.text().await.unwrap_or_default();
            return Err(BenchError::Failed(format!(
                "POST /completion: HTTP {status}: {}",
                text.trim()
            )));
        }
        let mut bytes = resp.bytes_stream();
        let mut sse = SseDecoder::new();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| failed("POST /completion: stream", &e))?;
            let at = Instant::now();
            for ev in sse.feed(&chunk) {
                let Some(parsed) = parse_chunk(&ev.data)
                    .map_err(|e| BenchError::Failed(format!("POST /completion: {e}")))?
                else {
                    continue;
                };
                let stop = parsed.stop;
                lock(rec).push(at, parsed);
                if stop {
                    return Ok(());
                }
            }
        }
        Err(BenchError::Failed(
            "POST /completion: the stream ended without a final (stop) chunk".into(),
        ))
    }
}

pub(crate) fn lock(rec: &Mutex<StreamRecord>) -> std::sync::MutexGuard<'_, StreamRecord> {
    rec.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Layer(
        &'static str,
        Option<Box<dyn std::error::Error + Send + Sync>>,
    );

    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|e| e as _)
        }
    }

    fn layer(m: &'static str, under: impl std::error::Error + Send + Sync + 'static) -> Layer {
        Layer(m, Some(Box::new(under)))
    }

    #[test]
    fn the_chain_keeps_every_cause_in_order() {
        let e = layer(
            "error sending request for url (http://127.0.0.1:45999/completion)",
            layer(
                "client error (SendRequest)",
                layer(
                    "connection closed before message completed",
                    std::io::Error::from(std::io::ErrorKind::ConnectionReset),
                ),
            ),
        );
        assert_eq!(
            error_chain(&e),
            "error sending request for url (http://127.0.0.1:45999/completion): \
             client error (SendRequest): connection closed before message completed: \
             connection reset"
        );
        assert_eq!(
            failed("POST /completion", &e),
            BenchError::Failed(format!("POST /completion: {}", error_chain(&e)))
        );
    }

    #[test]
    fn a_cause_already_shown_is_not_repeated() {
        let e = layer(
            "tcp connect error: Connection refused (os error 111)",
            layer("Connection refused (os error 111)", Layer("", None)),
        );
        assert_eq!(
            error_chain(&e),
            "tcp connect error: Connection refused (os error 111)"
        );
        assert_eq!(error_chain(&Layer("alone", None)), "alone");
    }

    /// A server that reads each request and closes without answering.
    async fn hanging_up() -> std::net::SocketAddr {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
            }
        });
        addr
    }

    /// A real reqwest failure: the message now says why, not only which URL.
    #[tokio::test]
    async fn a_real_send_failure_names_its_cause() {
        let addr = hanging_up().await;
        let c = LlamaClient::new(
            reqwest::Client::new(),
            &format!("http://{addr}"),
            Cancel::none(),
        );
        let Err(BenchError::Failed(m)) = c.props().await else {
            panic!("a closed connection is a failure");
        };
        assert!(
            m.starts_with("GET /props: error sending request for url ("),
            "{m}"
        );
        assert!(
            m.contains("connection closed before message completed"),
            "{m}"
        );
    }

    #[tokio::test]
    async fn a_broken_connection_is_resendable_a_refused_one_is_not() {
        let addr = hanging_up().await;
        let e = reqwest::Client::new()
            .get(format!("http://{addr}/props"))
            .send()
            .await
            .unwrap_err();
        assert!(resendable(&e), "{}", error_chain(&e));

        let gone = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = gone.local_addr().unwrap();
        drop(gone);
        let e = reqwest::Client::new()
            .get(format!("http://{addr}/props"))
            .send()
            .await
            .unwrap_err();
        assert!(!resendable(&e), "{}", error_chain(&e));
    }

    /// A request that never gets an answer is sent [`SENDS`] times, and the
    /// resend is kept under the stage it belonged to.
    #[tokio::test]
    async fn a_resend_is_recorded_under_its_stage() {
        let addr = hanging_up().await;
        let c = LlamaClient::new(
            reqwest::Client::new(),
            &format!("http://{addr}"),
            Cancel::none(),
        );
        c.set_stage("reading the server's slots and context");
        let Err(BenchError::Failed(m)) = c.props().await else {
            panic!("no answer is a failure");
        };
        assert!(m.contains("(attempt 2 of 2; before it: "), "{m}");
        let retried = c.take_retried();
        assert_eq!(retried.len(), 1);
        assert_eq!(retried[0].stage, "reading the server's slots and context");
        assert_eq!(retried[0].request, "GET /props");
        assert!(c.take_retried().is_empty(), "taken once");
    }
}
