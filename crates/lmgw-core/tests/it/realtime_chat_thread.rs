//! Chat voice WP8 (chat-voice design §8): a realtime session bound to a
//! Chat thread — the handshake's binding and refusals, takeover, what the
//! thread owns, the turn through the chat engine, the journal's writes in
//! their fixed order with the heard cut, the `lmgw.*` extension events and
//! the drain at disconnect.
//!
//! Over a real socket (`tokio-tungstenite`) with the dashboard's cookie,
//! manual commits of synthetic PCM (silence: the ASR fake answers what the
//! test scripts), and the chat, ASR and TTS fakes of the realtime suites
//! (`support::realtime_*`); the GPU world for the warm and the hold.

use std::time::Duration;

use lmgw_core::config::Settings;
use lmgw_core::state::{AppState, SharedState};
use serde_json::{json, Value};

use crate::common::{serve, Gw};
use crate::support::realtime_audio::{add_asr_alias, append, asr_fake, silence, AsrFake};
use crate::support::realtime_fakes::{add_chat_aliases, chat_fake, next_event, send, ChatFake, Ws};
use crate::support::realtime_tts::{add_tts_alias, speech, tts_fake, wav, TtsFake, VOICES};

mod binding;
mod cuts;
mod ends;
mod journal;
mod models;
mod reasoning;
mod refusals;
mod stopped_rows;
mod turns;

// -- harness -------------------------------------------------------------------

/// A gateway with the chat fake (`chatty`, `other`), the TTS fake (`speak`)
/// and the ASR fake (`hear`) as the Chat's voice, realtime's default voice
/// `alba`, then `tweak`.
pub(crate) struct World {
    pub state: SharedState,
    pub gw: Gw,
    pub chat: ChatFake,
    pub tts: TtsFake,
    pub asr: AsrFake,
}

pub(crate) async fn world(tweak: impl FnOnce(&mut Settings)) -> World {
    world_on(AppState::init_for_tests().await.unwrap(), tweak).await
}

/// [`world`] on a gateway state set up elsewhere (the knowledge fixtures').
pub(crate) async fn world_on(state: SharedState, tweak: impl FnOnce(&mut Settings)) -> World {
    let chat = chat_fake().await;
    let tts = tts_fake(&VOICES).await;
    let asr = asr_fake().await;
    add_chat_aliases(&state, &chat).await;
    add_tts_alias(&state, &tts).await;
    add_asr_alias(&state, &asr).await;
    // Short clauses: every answer plays out in a few hundred ms.
    tts.set_default(wav(&speech(300), 24_000));
    settings(&state, |s| {
        s.chat_tts_alias = "speak".into();
        s.chat_stt_alias = "hear".into();
        s.realtime.default_voice = "alba".into();
        tweak(s);
    })
    .await;
    let gw = serve(state.clone()).await;
    World {
        state,
        gw,
        chat,
        tts,
        asr,
    }
}

pub(crate) async fn settings(state: &SharedState, f: impl FnOnce(&mut Settings)) {
    let mut s = state.snapshot().settings.clone();
    f(&mut s);
    lmgw_core::store::save_settings(&state.db, &s)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
}

impl World {
    pub fn addr(&self) -> String {
        self.gw.addr().to_string()
    }

    /// The dashboard's cookie, as its page carries it.
    pub fn cookie(&self) -> String {
        format!("lmgw_session={}", self.gw.key)
    }

    pub async fn post(&self, route: &str, body: Value) -> reqwest::Response {
        self.gw
            .client()
            .post(format!("{}{route}", self.gw))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    pub async fn get(&self, route: &str) -> Value {
        self.gw
            .client()
            .get(format!("{}{route}", self.gw))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// A thread on `model` whose voice speaks German; `extra` goes into its
    /// creation.
    pub async fn thread(&self, model: &str, extra: Value) -> i64 {
        let mut body = json!({ "model_alias": model });
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            body[k] = v;
        }
        let r = self.post("/chat/api/threads", body).await;
        assert_eq!(r.status(), 200);
        let tid = r.json::<Value>().await.unwrap()["id"].as_i64().unwrap();
        let r = self
            .post(
                &format!("/chat/api/threads/{tid}/settings"),
                json!({ "voice": { "language": "de" } }),
            )
            .await;
        assert_eq!(r.status(), 200);
        tid
    }

    /// The thread's messages: `(role, content, voice)`.
    pub async fn messages(&self, tid: i64) -> Vec<(String, String, Value)> {
        let v = self.get(&format!("/chat/api/threads/{tid}")).await;
        v["messages"]
            .as_array()
            .unwrap_or_else(|| panic!("no messages in {v}"))
            .iter()
            .map(|m| {
                (
                    m["role"].as_str().unwrap().to_string(),
                    m["content"].as_str().unwrap().to_string(),
                    m["voice"].clone(),
                )
            })
            .collect()
    }

    /// Write `body` into the thread's settings.
    pub async fn set(&self, tid: i64, body: Value) {
        let r = self
            .post(&format!("/chat/api/threads/{tid}/settings"), body)
            .await;
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    }

    /// A bound session on `tid` with manual turns and audio output, all of
    /// its audio sent at once: the socket.
    pub async fn voice(&self, tid: i64) -> Ws {
        let (mut ws, _) = self.bind(tid).await;
        manual(&mut ws, 60_000).await;
        ws
    }

    /// The handshake for thread `tid` with `headers`: the socket, or the
    /// refusal's status and body.
    pub async fn connect(&self, query: &str, headers: &[(&str, &str)]) -> Result<Ws, (u16, Value)> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::http::HeaderName;
        use tokio_tungstenite::tungstenite::Error as WsError;
        let mut req = format!("ws://{}/v1/realtime?{query}", self.addr())
            .into_client_request()
            .unwrap();
        for (k, v) in headers {
            req.headers_mut().insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => Ok(ws),
            Err(WsError::Http(resp)) => {
                let status = resp.status().as_u16();
                let body = resp
                    .body()
                    .as_deref()
                    .and_then(|b| serde_json::from_slice(b).ok())
                    .unwrap_or(Value::Null);
                Err((status, body))
            }
            Err(e) => panic!("handshake failed below HTTP: {e}"),
        }
    }

    /// A session bound to `tid` with the dashboard's cookie: the socket and
    /// its `session.created`.
    pub async fn bind(&self, tid: i64) -> (Ws, Value) {
        let cookie = self.cookie();
        let mut ws = self
            .connect(&format!("chat_thread={tid}"), &[("cookie", &cookie)])
            .await
            .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
        let created = next_event(&mut ws).await;
        assert_eq!(created["type"], "session.created", "{created}");
        (ws, created)
    }
}

/// Manual turns, audio output, and `lead_ms` of lead (60 s: every clause
/// leaves at once, so nothing waits for pacing): past its
/// `session.updated`.
pub(crate) async fn manual(ws: &mut Ws, lead_ms: u32) {
    send(
        ws,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["audio"],
            "audio": {"input": {"turn_detection": null}},
            "lmgw": {"output_lead_ms": lead_ms}}}),
    )
    .await;
    let ev = next(ws).await;
    assert_eq!(ev["type"], "session.updated", "{ev}");
}

/// The next event but the connect warm's model states, which come when
/// they come.
pub(crate) async fn next(ws: &mut Ws) -> Value {
    loop {
        let ev = next_event(ws).await;
        if ev["type"] != "lmgw.model.state" {
            return ev;
        }
    }
}

/// A spoken turn as push-to-talk sends it: 200 ms of synthetic audio,
/// `commit` and `response.create`.
pub(crate) async fn say(ws: &mut Ws) {
    append(ws, &silence(200)).await;
    send(ws, json!({"type": "input_audio_buffer.commit"})).await;
    send(ws, json!({"type": "response.create"})).await;
}

/// Every event up to and with the first for which `f` holds.
pub(crate) async fn until(ws: &mut Ws, f: impl Fn(&Value) -> bool) -> Vec<Value> {
    let mut out = Vec::new();
    loop {
        let ev = next_event(ws).await;
        let hit = f(&ev);
        out.push(ev);
        if hit {
            return out;
        }
    }
}

/// Events up to and with the first of type `t`.
pub(crate) async fn until_type(ws: &mut Ws, t: &str) -> Vec<Value> {
    until(ws, |e| e["type"] == t).await
}

/// The next event within `secs`, or `None`.
pub(crate) async fn try_next(ws: &mut Ws, secs: u64) -> Option<Value> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .ok()??
            .ok()?;
        match msg {
            Message::Text(t) => return serde_json::from_str(t.as_str()).ok(),
            Message::Ping(_) | Message::Pong(_) => continue,
            _ => return None,
        }
    }
}

/// Events up to and with the `lmgw.chat.reply` whose content is `content`.
pub(crate) async fn until_reply(ws: &mut Ws, content: &str) -> Vec<Value> {
    until(ws, |e| {
        e["type"] == "lmgw.chat.reply" && e["content"] == content
    })
    .await
}

/// The events of type `t` in `events`.
pub(crate) fn of_type<'a>(events: &'a [Value], t: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == t).collect()
}

/// The `lmgw.chat.frame` events' `event` names, in order.
pub(crate) fn frames(events: &[Value]) -> Vec<String> {
    of_type(events, "lmgw.chat.frame")
        .iter()
        .map(|e| e["event"].as_str().unwrap().to_string())
        .collect()
}

/// Poll `f` until it holds, for up to five seconds.
pub(crate) async fn eventually<F, Fut>(what: &str, f: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..500 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}
