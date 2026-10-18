//! `POST /v1/realtime/calls` end to end (realtime calls design §2, §13.1;
//! WP2 part B, WP7): the route with both bodies and its refusals (`route`),
//! the hangup (`hangup`), the bind when `oai-events` opens (`binding`),
//! `takeover=ask` (`ask`), `chat_folder` (`folder`), the audio kept before
//! the open (`kept`), and paired devices calling through the device
//! listener (`devices`).
//!
//! A real gateway (the realtime fakes of `realtime_chat_thread`'s world)
//! with its plain socket group bound on loopback, and str0m as the phone
//! (`realtime_calls_net::client`), driven in the test's task: it POSTs its
//! offer over HTTP, applies the answer and talks to lmgw over UDP. A
//! paired device POSTs through its proxy to the device listener, whose
//! device socket group carries its media. Every wait is for an event, under
//! the harness's patience.

use lmgw_core::realtime::calls::{Group, GroupState};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use serde_json::Value;
use str0m::Event;

use crate::realtime_calls_net::client::Client;
use crate::realtime_chat_thread::{world, World};

mod ask;
mod binding;
mod devices;
mod folder;
mod hangup;
mod kept;
mod route;

/// [`world`] with its plain socket group bound on loopback, as
/// `server::serve` binds it beside the listener.
pub(crate) async fn calls_world(tweak: impl FnOnce(&mut lmgw_core::config::Settings)) -> World {
    let w = world(tweak).await;
    let bound = w
        .state
        .calls
        .bind_group(Group::Plain, &["127.0.0.1:0".parse().unwrap()], None)
        .await;
    assert!(matches!(bound, GroupState::Bound(_)), "{bound:?}");
    w
}

/// A key of `kind` ("client" or "owner") named `name`, with `policy`
/// (`key_set`'s fields) applied, and a client that sends it as its bearer on
/// the plain listener: its id and the client. A stock client's calls are
/// made by a key like this; a paired device calls through the device
/// listener (`devices`, `crate::device_chat::pair`). Binding a chat thread
/// needs the chat capability, which a client key lacks: those calls use an
/// owner key.
pub(crate) async fn key_client(
    w: &World,
    kind: &str,
    name: &str,
    policy: Value,
) -> (i64, reqwest::Client) {
    let (status, v) = crate::device_chat::op(
        w,
        "key_create",
        serde_json::json!({ "name": name, "kind": kind }),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let id = v["id"].as_i64().unwrap();
    if policy.as_object().is_some_and(|p| !p.is_empty()) {
        let mut body = policy;
        body["id"] = id.into();
        let (status, v) = crate::device_chat::op(w, "key_set", body).await;
        assert_eq!(status, 200, "{v}");
    }
    (id, crate::device_chat::bearer(v["key"].as_str().unwrap()))
}

/// What a POST answered.
#[derive(Debug)]
pub(crate) struct Answer {
    pub status: u16,
    pub location: Option<String>,
    pub content_type: Option<String>,
    pub body: String,
}

impl Answer {
    /// The refusal's JSON.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    /// The refusal's `error.code`.
    pub fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

/// `POST {path}` as `http` with `content_type` and `body`.
pub(crate) async fn post_raw(
    w: &World,
    http: &reqwest::Client,
    path: &str,
    content_type: &str,
    body: String,
) -> Answer {
    post_body(w, http, path, content_type, body.into()).await
}

/// [`post_raw`] with the body sent chunked, in pieces of 64 KiB: no
/// `Content-Length` to refuse it by, so a bound trips as it is read.
pub(crate) async fn post_chunked(
    w: &World,
    http: &reqwest::Client,
    path: &str,
    content_type: &str,
    body: String,
) -> Answer {
    let pieces: Vec<Result<Vec<u8>, std::io::Error>> = body
        .into_bytes()
        .chunks(64 << 10)
        .map(|c| Ok(c.to_vec()))
        .collect();
    let body = reqwest::Body::wrap_stream(futures::stream::iter(pieces));
    post_body(w, http, path, content_type, body).await
}

async fn post_body(
    w: &World,
    http: &reqwest::Client,
    path: &str,
    content_type: &str,
    body: reqwest::Body,
) -> Answer {
    let r = http
        .post(format!("{}{path}", w.gw))
        .header(CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await
        .unwrap();
    let header = |h| {
        r.headers()
            .get(h)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let (location, content_type) = (header(LOCATION), header(CONTENT_TYPE));
    Answer {
        status: r.status().as_u16(),
        location,
        content_type,
        body: r.text().await.unwrap(),
    }
}

/// `POST /v1/realtime/calls?{query}` with the offer alone.
pub(crate) async fn post_sdp(w: &World, http: &reqwest::Client, query: &str, sdp: &str) -> Answer {
    let path = format!("/v1/realtime/calls?{query}");
    post_raw(w, http, &path, "application/sdp", sdp.to_string()).await
}

/// A `multipart/form-data` body of `(name, content type, text)` parts, and
/// its content type.
pub(crate) fn multipart(parts: &[(&str, &str, &str)]) -> (String, String) {
    let boundary = "lmgw-test-boundary";
    let mut body = String::new();
    for (name, ct, text) in parts {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\
             Content-Type: {ct}\r\n\r\n{text}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// A phone on loopback: str0m with its call.
pub(crate) struct Phone {
    pub c: Client,
    /// The call's id, from `Location`.
    pub id: String,
}

impl Phone {
    /// A phone's offer, before any POST.
    pub async fn offer() -> (Client, String) {
        Client::new("127.0.0.1".parse().unwrap()).await
    }

    /// Call with `query` as `http`, the offer alone: answered, not yet
    /// connected.
    pub async fn call(w: &World, http: &reqwest::Client, query: &str) -> Phone {
        let (c, sdp) = Self::offer().await;
        let a = post_sdp(w, http, query, &sdp).await;
        Self::answered(c, a)
    }

    /// The phone `c` with the `201` `a`.
    pub fn answered(mut c: Client, a: Answer) -> Phone {
        assert_eq!(a.status, 201, "{}", a.body);
        let location = a.location.expect("a Location");
        let id = location
            .strip_prefix("/v1/realtime/calls/")
            .unwrap_or_else(|| panic!("{location}"))
            .to_string();
        c.accept(&a.body);
        Phone { c, id }
    }

    /// [`Self::call`], connected: ICE, DTLS and `oai-events` up.
    pub async fn connected(w: &World, http: &reqwest::Client, query: &str) -> Phone {
        let mut p = Self::call(w, http, query).await;
        p.c.connect().await;
        p
    }

    /// lmgw's next event on `oai-events`.
    pub async fn next(&mut self) -> Value {
        self.c
            .until("lmgw's next event on the call", |_, e| match e {
                Event::ChannelData(d) => Some(serde_json::from_slice(&d.data).unwrap()),
                _ => None,
            })
            .await
    }

    /// The next event but the connect warm's model states, which come when
    /// they come.
    pub async fn next_but_states(&mut self) -> Value {
        loop {
            let ev = self.next().await;
            if ev["type"] != "lmgw.model.state" {
                return ev;
            }
        }
    }

    /// Send `event` on `oai-events`.
    pub fn send(&mut self, event: Value) {
        self.c.send_text(&event.to_string());
    }

    /// Run until lmgw closed the call: the events that came before the
    /// close, the model states left out — those the connect warm sends when
    /// it sends them, but never one after a `lmgw.session.closed`, which is
    /// kept, so a check that the closed event is the last fails on it.
    pub async fn closed(&mut self) -> Vec<Value> {
        let mut seen: Vec<Value> = Vec::new();
        self.c
            .until("lmgw closed the call", |_, e| match e {
                Event::ChannelData(d) => {
                    let v: Value = serde_json::from_slice(&d.data).unwrap();
                    let after_closed = seen.iter().any(|e| e["type"] == "lmgw.session.closed");
                    if v["type"] != "lmgw.model.state" || after_closed {
                        seen.push(v);
                    }
                    None
                }
                Event::Closed => Some(()),
                _ => None,
            })
            .await;
        seen
    }

    /// `POST …/hangup` for this call as `http`: the status.
    pub async fn hangup(&self, w: &World, http: &reqwest::Client) -> u16 {
        hangup(w, http, &self.id).await
    }
}

/// `POST /v1/realtime/calls/{id}/hangup` as `http`: the status, after the
/// body (empty on a 200) was read.
pub(crate) async fn hangup(w: &World, http: &reqwest::Client, id: &str) -> u16 {
    let r = http
        .post(format!("{}/v1/realtime/calls/{id}/hangup", w.gw))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.text().await.unwrap();
    if status == 200 {
        assert_eq!(body, "", "a hangup's 200 has an empty body");
    }
    status
}

/// A client with no credential (Require API key is off in these worlds).
pub(crate) fn anonymous() -> reqwest::Client {
    reqwest::Client::new()
}
