//! The Chat change feed (client-apps design §2, WP4): `GET /chat/api/feed`
//! through the router and the gate, read as a client reads it — SSE over
//! HTTP, frame by frame.
//!
//! - `stored`: thread and folder writes in commit order with their cursors,
//!   `since` and `Last-Event-ID`, rendering at delivery (tombstones), paged
//!   catch-up across a restart (the epoch), `resync` for a cursor the feed
//!   cannot honour, the sweep's events and the retention prune;
//! - `live`: `hello`, keep-alive comments, `turn.*` with who, `hold` on a
//!   fallback changed through the settings patch, `state` for a reader that
//!   fell behind the live buffer;
//! - `device`: L3 — a device's feed never carries an Admin Chat thread,
//!   live, in its catch-up, in `hello` or in a folder's counts;
//! - `revoke`: Disable, Rotate, Delete and expiry end a device's feed with
//!   `revoked`;
//! - `profiles`: the `profile.*` events (personality-profiles design §3.2);
//! - `deleted_device`: `device.revoked` and who hears it (MCP Tasks design
//!   §4.1, client-apps design §2.2).

use std::time::Duration;

use serde_json::Value;

use crate::realtime_chat_thread::{settings, World};

mod deleted_device;
mod device;
mod gaps;
mod live;
mod profiles;
mod revoke;
mod stored;
mod tasks;

/// One SSE record: its `id:`, its `event:` (`":"` for a comment) and its
/// JSON `data:`.
#[derive(Debug, Clone)]
pub(crate) struct Frame {
    pub id: Option<String>,
    pub event: String,
    pub data: Value,
}

/// An open feed, read as it comes.
pub(crate) struct Feed {
    resp: reqwest::Response,
    buf: String,
    /// Every frame read so far, in order.
    pub frames: Vec<Frame>,
    /// The server ended the stream.
    pub ended: bool,
}

impl Feed {
    /// `GET /chat/api/feed{query}` as `client`, with `Last-Event-ID` when
    /// given; the response must be the stream.
    pub(crate) async fn open(
        w: &World,
        client: &reqwest::Client,
        query: &str,
        last_event_id: Option<&str>,
    ) -> Self {
        let mut req = client.get(format!("{}/chat/api/feed{query}", w.gw));
        if let Some(id) = last_event_id {
            req = req.header("last-event-id", id);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), 200, "the feed opens");
        let mut feed = Self::reading(resp);
        feed.until(10, |f| !f.is_empty()).await;
        assert_eq!(feed.frames[0].event, "hello", "{:?}", feed.frames);
        feed
    }

    /// The feed's response as it starts, read from its first frame on,
    /// whatever that is.
    pub(crate) fn reading(resp: reqwest::Response) -> Self {
        Self {
            resp,
            buf: String::new(),
            frames: Vec::new(),
            ended: false,
        }
    }

    /// Read until `done` holds over every frame read so far (or the stream
    /// ended); fail after `secs`.
    pub(crate) async fn until(&mut self, secs: u64, done: impl Fn(&[Frame]) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        while !done(&self.frames) && !self.ended {
            let chunk = tokio::time::timeout_at(deadline, self.resp.chunk())
                .await
                .unwrap_or_else(|_| panic!("timed out; read so far: {:#?}", self.frames))
                .unwrap();
            match chunk {
                Some(c) => {
                    self.buf.push_str(&String::from_utf8_lossy(&c));
                    self.parse();
                }
                None => self.ended = true,
            }
        }
    }

    fn parse(&mut self) {
        while let Some(end) = self.buf.find("\n\n") {
            let record: String = self.buf.drain(..end + 2).collect();
            let mut id = None;
            let mut event = None;
            let mut data = String::new();
            for line in record.lines() {
                if line.starts_with(':') {
                    event = Some(":".to_string());
                } else if let Some(v) = line.strip_prefix("id:") {
                    id = Some(v.trim().to_string());
                } else if let Some(v) = line.strip_prefix("event:") {
                    event = Some(v.trim().to_string());
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push_str(v.trim_start());
                }
            }
            if let Some(event) = event {
                self.frames.push(Frame {
                    id,
                    event,
                    data: serde_json::from_str(&data).unwrap_or(Value::Null),
                });
            }
        }
    }

    pub(crate) fn hello(&self) -> &Value {
        &self.frames[0].data
    }

    /// The frames named `event`.
    pub(crate) fn named(&self, event: &str) -> Vec<&Frame> {
        self.frames.iter().filter(|f| f.event == event).collect()
    }

    /// The stored events (those with an id; a keep-alive that carries the
    /// cursor is none), as `(event, thread or folder id)`.
    pub(crate) fn stored(&self) -> Vec<(String, i64)> {
        self.frames
            .iter()
            .filter(|f| f.id.is_some() && f.event != ":")
            .map(|f| (f.event.clone(), subject(f)))
            .collect()
    }

    /// The cursor to resume from: the last stored event's id, else
    /// `hello.cursor`.
    pub(crate) fn cursor(&self) -> String {
        self.frames
            .iter()
            .rev()
            .find_map(|f| f.id.clone())
            .unwrap_or_else(|| self.hello()["cursor"].as_str().unwrap().to_string())
    }
}

/// The thread or folder a stored event is about; `-1` for one about
/// neither (a `profile.*` event, whose `id` is the profile's).
pub(crate) fn subject(f: &Frame) -> i64 {
    let d = &f.data;
    if f.event.starts_with("profile.") {
        return -1;
    }
    if f.event.starts_with("thread.") {
        d.get("thread_id")
            .or_else(|| d.get("id"))
            .and_then(Value::as_i64)
            .unwrap_or(-1)
    } else {
        d.get("folder_id")
            .or_else(|| d.get("id"))
            .and_then(Value::as_i64)
            .unwrap_or(-1)
    }
}

/// Every frame of `frames` that names thread `id`, stored or live.
pub(crate) fn about_thread(frames: &[Frame], id: i64) -> Vec<&Frame> {
    frames
        .iter()
        .filter(|f| {
            f.data.get("thread_id").and_then(Value::as_i64) == Some(id)
                || (f.event.starts_with("thread.")
                    && f.data.get("id").and_then(Value::as_i64) == Some(id))
        })
        .collect()
}

/// Change a setting of `w` and publish it.
pub(crate) async fn set(w: &World, f: impl FnOnce(&mut lmgw_core::config::Settings)) {
    settings(&w.state, f).await;
}
