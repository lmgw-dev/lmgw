//! The whole app on a loopback port, against the fake lmgw: a seeded folder,
//! a [`Server`] with its router, and request builders that carry what lmgw's
//! proxy would (`X-Forwarded-Host`, `X-Lmgw-Face`, `X-Forwarded-For`) and what
//! the UI does (`X-Folder-Chat`).

use std::sync::Arc;
use std::time::Duration;

use folder_chat::config::AgentConfig;
use folder_chat::server::Server;
use folder_chat::FolderChat;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::fake::{config, gateway, seed, serve, Fake, Shared};

/// What lmgw would set as `LMGW_APP_ORIGIN`.
pub const ORIGIN: &str = "http://folder-chat.localhost:8001";
/// What lmgw's proxy would put in `X-Forwarded-Host`.
pub const AUTHORITY: &str = "folder-chat.localhost:8001";
/// What lmgw's proxy would put in `X-Forwarded-For` for a browser on the
/// machine lmgw runs on.
pub const LOCAL_CLIENT: &str = "127.0.0.1";

/// How long a test waits for something that should happen at once before it
/// fails instead of hanging.
pub const PATIENCE: Duration = Duration::from_secs(20);

pub struct App {
    pub base: String,
    pub server: Server,
    pub fake: Shared,
    pub tmp: tempfile::TempDir,
    pub http: reqwest::Client,
    pub shutdown: CancellationToken,
}

pub async fn start(fake: Fake, rerank: bool) -> App {
    start_with(fake, rerank, false).await
}

/// [`start`], with the owner's `allow_remote` switch set.
pub async fn start_with(fake: Fake, rerank: bool, allow_remote: bool) -> App {
    start_configured(fake, |c| AgentConfig {
        allow_remote,
        rerank_model: rerank.then(|| "rerank/fixture".to_string()),
        ..c
    })
    .await
}

/// [`start`], with the owner's config changed by `set` (it gets the test
/// default: no rerank, no vision model, this machine only).
pub async fn start_configured(fake: Fake, set: impl FnOnce(AgentConfig) -> AgentConfig) -> App {
    let (gw, shared) = serve(fake).await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let config = set(config(tmp.path(), false));
    let app = FolderChat::open(config, gateway(&gw)).await.unwrap();
    let shutdown = CancellationToken::new();
    let server = Server::new(Arc::new(app), ORIGIN, shutdown.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server.router();
    let sd = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(sd.cancelled_owned())
            .await
            .unwrap()
    });
    App {
        base: format!("http://{addr}"),
        server,
        fake: shared,
        tmp,
        http: reqwest::Client::new(),
        shutdown,
    }
}

impl App {
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// A `GET` from a browser on this machine, as lmgw's proxy forwards it
    /// from the agent origin.
    pub fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(self.url(path))
            .header("x-forwarded-host", AUTHORITY)
            .header("x-lmgw-face", "app")
            .header("x-forwarded-for", LOCAL_CLIENT)
    }

    /// A `POST` as the UI sends it, through lmgw's proxy.
    pub fn post(&self, path: &str, body: &Value) -> reqwest::RequestBuilder {
        self.http
            .post(self.url(path))
            .header("x-forwarded-host", AUTHORITY)
            .header("x-lmgw-face", "app")
            .header("x-forwarded-for", LOCAL_CLIENT)
            .header("x-folder-chat", "1")
            .header("content-type", "application/json")
            .body(body.to_string())
    }

    /// Wait until no sync is running.
    pub async fn wait_idle(&self) {
        tokio::time::timeout(PATIENCE, async {
            while self.server.hub().running() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the sync did not finish");
    }

    /// Start a sync through the API and wait for it to end.
    pub async fn sync(&self) {
        let r = self
            .post("/api/sync", &serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 202, "{}", r.text().await.unwrap_or_default());
        self.wait_idle().await;
    }
}

/// One server-sent event: its name and its JSON data.
#[derive(Debug, Clone)]
pub struct Sse {
    pub name: String,
    pub data: Value,
}

/// Read an SSE response until `stop` says so (the matching event included) or
/// the stream ends; fails the test after [`PATIENCE`].
pub async fn sse_until(mut resp: reqwest::Response, stop: impl Fn(&Sse) -> bool) -> Vec<Sse> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let (mut name, mut data) = (String::from("message"), Vec::<String>::new());
    let read = async {
        loop {
            let Some(bytes) = resp.chunk().await.unwrap() else {
                return;
            };
            buf.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(i) = buf.find('\n') {
                let line: String = buf.drain(..=i).collect();
                let line = line.trim_end_matches(['\n', '\r']);
                if line.is_empty() {
                    if !data.is_empty() {
                        let ev = Sse {
                            name: std::mem::replace(&mut name, "message".into()),
                            data: serde_json::from_str(&data.join("\n")).unwrap(),
                        };
                        data.clear();
                        let done = stop(&ev);
                        out.push(ev);
                        if done {
                            return;
                        }
                    }
                } else if let Some(n) = line.strip_prefix("event:") {
                    name = n.trim().to_string();
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push(d.strip_prefix(' ').unwrap_or(d).to_string());
                }
            }
        }
    };
    tokio::time::timeout(PATIENCE, read)
        .await
        .expect("the event stream stalled");
    out
}

/// Every event until the stream ends.
pub async fn sse_all(resp: reqwest::Response) -> Vec<Sse> {
    sse_until(resp, |_| false).await
}
