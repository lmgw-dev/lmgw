//! Podman's own API for a pull (machine-check design §5): what lets a pull
//! report bytes. `podman pull` prints no byte counts when it is not on a
//! terminal; the libpod pull with `compatMode=true` streams them per blob
//! ([`super::compat`]).
//!
//! lmgw does not rely on the user's `podman.socket`, which is not enabled
//! by default. It starts a private `podman system service` on a socket of
//! this instance's own — `$XDG_RUNTIME_DIR/lmgw-machine/<instance id>/
//! podman.sock`, so a dev instance's service never answers production's
//! pulls — when a pull needs it. The service needs no root, uses the same
//! image store as every other podman call, and exits by itself once it has
//! been idle for [`SERVICE_IDLE`]; lmgw quitting stops it too
//! (`kill_on_drop` on the task that reaps it).
//!
//! Where the service cannot be had — no `$XDG_RUNTIME_DIR`, a podman that
//! will not start one — the pull job falls back to the `podman pull`
//! command and says "size not known" with the reason, never a made-up bar.
//!
//! **The seam.** The pull job reaches the API only through a [`PullApi`]
//! (`BuildSeams::pull_api`). The gateway installs [`ServicePull`] at
//! startup; a test state has [`NoPullApi`], so no test starts a real
//! service, and a test that wants the API path installs its own.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use crate::erased::Erased;

/// How long the private service stays after its last request ended. A
/// sampling of idleness, nothing waits on it: a pull keeps its connection
/// open, and the service does not count a connection in use as idle.
pub const SERVICE_IDLE: Duration = Duration::from_secs(60);

/// How long a service may take to answer — one starting, or one already
/// running that is asked whether it still answers — before the pull uses
/// the `podman pull` command instead and says why in its size note and the
/// log. Podman's own start takes a fraction of a second, and its `_ping`
/// takes no lock; one that takes this long is wedged.
pub const SERVICE_START: Duration = Duration::from_secs(30);

/// How often a starting service is asked whether it answers yet.
const START_POLL: Duration = Duration::from_millis(50);

/// The libpod API version lmgw speaks: Podman's own minimum since 4.0.
const API: &str = "v4.0.0";

/// How many of a service's last stderr lines are kept for the reason it
/// did not start.
const STDERR_LINES: usize = 10;

/// What asking Podman's API for a pull came to.
pub enum ApiPull {
    /// Podman's answer, line by line. The channel closes when the answer
    /// ended; an `Err` is a broken connection. Dropping the receiver drops
    /// the connection, which ends the pull on Podman's side.
    Streaming(mpsc::Receiver<Result<String, String>>),
    /// The API refused the pull with this message (`manifest unknown`): the
    /// pull failed.
    Refused(String),
    /// There is no API to ask: the pull runs through the `podman pull`
    /// command, and this is why.
    Unavailable(String),
}

/// How the pull job reaches Podman's API (module doc).
pub trait PullApi: Send + Sync {
    /// Pull `reference` with compat-mode progress.
    fn pull<'a>(&'a self, reference: &'a str) -> Erased<'a, ApiPull>;
}

/// What asking a service's `_ping` came to.
#[derive(Debug, PartialEq, Eq)]
enum Ping {
    Answers,
    /// Nothing listens on the socket.
    Nothing,
    /// Something listens, and did not answer as a service does: why.
    Silent(String),
}

/// The API a gateway has before one is installed: none, and why.
pub struct NoPullApi(pub String);

impl PullApi for NoPullApi {
    fn pull<'a>(&'a self, _reference: &'a str) -> Erased<'a, ApiPull> {
        Box::pin(async move { ApiPull::Unavailable(self.0.clone()) })
    }
}

/// The private `podman system service` of this instance (module doc).
pub struct ServicePull {
    /// The socket, or why there is none.
    socket: Result<PathBuf, String>,
    /// One start at a time: two pulls that find no service start one.
    starting: tokio::sync::Mutex<()>,
}

impl ServicePull {
    /// The service of instance `instance` (`BuildSeams::instance_id`).
    pub fn for_instance(instance: &str) -> Self {
        let socket = match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
            Some(dir) => Ok(PathBuf::from(dir)
                .join("lmgw-machine")
                .join(instance)
                .join("podman.sock")),
            None => Err(
                "no runtime directory ($XDG_RUNTIME_DIR is not set) for the socket of \
                         Podman's API, so podman pull reports no byte counts"
                    .to_string(),
            ),
        };
        Self {
            socket,
            starting: tokio::sync::Mutex::new(()),
        }
    }

    fn client(socket: &Path) -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .unix_socket(socket.to_path_buf())
            .connect_timeout(Duration::from_secs(5))
            .no_gzip()
            .build()
            .map_err(|e| format!("building the client for Podman's API: {e}"))
    }

    /// Whether a service answers on the client's socket, waiting `within`
    /// for its answer once something listens there.
    async fn ping(client: &reqwest::Client, within: Duration) -> Ping {
        match client
            .get(format!("http://podman/{API}/libpod/_ping"))
            .timeout(within)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => Ping::Answers,
            Ok(r) => Ping::Silent(format!("it answered {}", r.status())),
            // First: a connect that timed out reached a listener too busy
            // to take it, which is no reason to take its socket.
            Err(e) if e.is_timeout() => {
                Ping::Silent(format!("it did not answer within {} s", within.as_secs()))
            }
            // No socket, or a socket nothing listens on: a unix connect
            // fails at once.
            Err(e) if e.is_connect() => Ping::Nothing,
            Err(e) => Ping::Silent(e.to_string()),
        }
    }

    /// A client for a service that answers: the running one, or one started
    /// now. A service that runs but does not answer within
    /// [`SERVICE_START`] is left alone — its socket is never taken from
    /// it — and this pull uses the `podman pull` command, saying why.
    async fn ready(&self) -> Result<reqwest::Client, String> {
        let socket = self.socket.as_ref().map_err(Clone::clone)?;
        let client = Self::client(socket)?;
        let _one = self.starting.lock().await;
        match Self::ping(&client, SERVICE_START).await {
            Ping::Answers => return Ok(client),
            Ping::Nothing => {}
            Ping::Silent(why) => {
                let why = format!(
                    "Podman's API service on {} is running but does not answer ({why}), so this \
                     pull uses the podman command",
                    socket.display()
                );
                tracing::warn!("{why}");
                return Err(why);
            }
        }
        let dir = socket
            .parent()
            .ok_or_else(|| format!("{} has no directory", socket.display()))?;
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        // A socket a service that is gone left behind: nothing listens on
        // it (the ping above).
        let _ = std::fs::remove_file(socket);
        let mut cmd = tokio::process::Command::new("podman");
        crate::child::system_tool_tokio(&mut cmd)
            .args([
                "system".to_string(),
                "service".to_string(),
                format!("--time={}", SERVICE_IDLE.as_secs()),
                format!("unix://{}", socket.display()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("podman system service could not be started: {e}"))?;
        let said: Arc<Mutex<VecDeque<String>>> = Arc::default();
        if let Some(err) = child.stderr.take() {
            let said = said.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let mut s = said.lock().unwrap_or_else(|e| e.into_inner());
                    if s.len() == STDERR_LINES {
                        s.pop_front();
                    }
                    s.push_back(l);
                }
            });
        }
        let began = Instant::now();
        loop {
            let left = SERVICE_START.saturating_sub(began.elapsed());
            if Self::ping(&client, left).await == Ping::Answers {
                break;
            }
            let last_words = || {
                let s = said.lock().unwrap_or_else(|e| e.into_inner());
                if s.is_empty() {
                    "it printed nothing".to_string()
                } else {
                    s.iter().cloned().collect::<Vec<_>>().join(" / ")
                }
            };
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!(
                    "podman system service ended ({status}) before it answered: {}",
                    last_words()
                ));
            }
            if began.elapsed() >= SERVICE_START {
                let _ = child.kill().await;
                return Err(format!(
                    "podman system service did not answer within {} s: {}",
                    SERVICE_START.as_secs(),
                    last_words()
                ));
            }
            tokio::time::sleep(START_POLL).await;
        }
        tracing::info!(
            "started Podman's API service for pulls on {} (it exits after {} s idle)",
            socket.display(),
            SERVICE_IDLE.as_secs()
        );
        // Reaped when it exits on its own; killed with the runtime when lmgw
        // quits first.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Ok(client)
    }
}

impl PullApi for ServicePull {
    fn pull<'a>(&'a self, reference: &'a str) -> Erased<'a, ApiPull> {
        Box::pin(async move {
            let client = match self.ready().await {
                Ok(c) => c,
                Err(why) => return ApiPull::Unavailable(why),
            };
            let mut url =
                match reqwest::Url::parse(&format!("http://podman/{API}/libpod/images/pull")) {
                    Ok(u) => u,
                    Err(e) => return ApiPull::Unavailable(format!("the pull URL: {e}")),
                };
            url.query_pairs_mut()
                .append_pair("reference", reference)
                .append_pair("compatMode", "true")
                // `podman pull`'s own default: the registry is asked even
                // when the store has the reference.
                .append_pair("policy", "always");
            let resp = match client.post(url).send().await {
                Ok(r) => r,
                Err(e) => {
                    return ApiPull::Unavailable(format!(
                        "Podman's API service did not take the pull: {e}"
                    ))
                }
            };
            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                return ApiPull::Refused(refusal(status, &body));
            }
            let (tx, rx) = mpsc::channel(256);
            tokio::spawn(async move {
                let mut body = resp.bytes_stream();
                let mut buf: Vec<u8> = Vec::new();
                while let Some(chunk) = body.next().await {
                    match chunk {
                        Ok(bytes) => {
                            buf.extend_from_slice(&bytes);
                            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                                let line: Vec<u8> = buf.drain(..=nl).collect();
                                let line = String::from_utf8_lossy(&line).into_owned();
                                // The reader gone: the pull is not wanted any
                                // more, and dropping the body ends it.
                                if tx.send(Ok(line)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx
                                .send(Err(format!("the answer of Podman's API broke off: {e}")))
                                .await;
                            return;
                        }
                    }
                }
                if !buf.is_empty() {
                    let _ = tx
                        .send(Ok(String::from_utf8_lossy(&buf).into_owned()))
                        .await;
                }
            });
            ApiPull::Streaming(rx)
        })
    }
}

/// An error answer's words: Podman's `message`, else the body, else the
/// status.
fn refusal(status: u16, body: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| body.trim().to_string());
    if message.is_empty() {
        format!("Podman's API answered {status}")
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_says_podmans_message() {
        assert_eq!(
            refusal(
                404,
                r#"{"cause":"manifest unknown","message":"manifest unknown: manifest unknown","response":404}"#
            ),
            "manifest unknown: manifest unknown"
        );
        assert_eq!(refusal(500, "boom"), "boom");
        assert_eq!(refusal(502, ""), "Podman's API answered 502");
    }

    /// No socket is "nothing listens" — a service is started in its place —
    /// while a listener that takes the connection and says nothing is a
    /// running service that does not answer, whose socket is left alone.
    #[tokio::test]
    async fn the_ping_tells_no_service_from_a_silent_one() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("podman.sock");
        let client = ServicePull::client(&socket).unwrap();
        let within = Duration::from_millis(300);
        assert_eq!(ServicePull::ping(&client, within).await, Ping::Nothing);

        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let held = tokio::spawn(async move {
            let mut kept = Vec::new();
            while let Ok((conn, _)) = listener.accept().await {
                kept.push(conn);
            }
        });
        match ServicePull::ping(&client, within).await {
            Ping::Silent(why) => assert!(why.contains("did not answer within"), "{why}"),
            other => panic!("a silent listener is no missing service: {other:?}"),
        }
        assert!(socket.exists());
        held.abort();
    }
}
