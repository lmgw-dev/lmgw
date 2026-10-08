//! The review's test gaps (G-9, 2026-10-07), each through the router where
//! a client meets it:
//!
//! - the access settings refused on every surface a device's model calls
//!   the admin tools from — `/v1/responses`, an unbound realtime session, a
//!   bound one (the Chat turn and an agent run are `admin_guards` and
//!   `admin_agents`);
//! - a device's running text turn on a toolset thread stopped when the
//!   gateway's level goes to `off`;
//! - a catch-up across both kinds of level record;
//! - the first settings save that moves the level, and a save whose reload
//!   fails.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use lmgw_core::config::SelfAdmin;
use serde_json::json;
use tokio::sync::Notify;

use super::admin_guards::toolset_thread;
use super::{frames, op, pair, self_admin_thread};
use crate::chat_feed::{about_thread, subject, Feed};
use crate::realtime_chat_thread::{manual, next, say, until, until_type, world, World};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::{send, user_text, Step, Turn};

/// The model's turn that calls `lmgw__settings_set {auth_enabled: false}`.
fn turns_auth_off() -> Turn {
    Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id: Some("call_1"),
            name: "lmgw__settings_set",
        },
        Step::CallArgs {
            index: 0,
            args: r#"{"auth_enabled": false}"#,
        },
        Step::Finish("tool_calls"),
        Step::Usage(10, 5),
    ])
}

const REFUSED: &str = "auth_enabled cannot be changed through lmgw's admin tools";

async fn full_device_world() -> (World, super::Device) {
    let w = world(|s| {
        s.self_admin = SelfAdmin::Full;
        s.auth_enabled = true;
    })
    .await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    (w, desk)
}

#[tokio::test]
async fn the_access_settings_are_refused_on_v1_responses() {
    let (w, desk) = full_device_world().await;
    w.chat.push(turns_auth_off());
    w.chat.push(Turn::text(&["Done."]));
    let body = desk
        .client
        .post(format!("{}/v1/responses", w.gw))
        .json(&json!({
            "model": "chatty", "input": "turn auth off", "stream": true,
            "tools": [{ "type": "mcp", "server_label": "lmgw", "require_approval": "never" }],
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains(REFUSED), "{body}");
    assert!(w.state.snapshot().settings.auth_enabled);
}

#[tokio::test]
async fn the_access_settings_are_refused_in_an_unbound_realtime_session() {
    let (w, desk) = full_device_world().await;
    let bearer = format!("Bearer {}", desk.key);
    let mut ws = w
        .connect("model=chatty", &[("authorization", bearer.as_str())])
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut ws).await["type"], "session.created");
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "output_modalities": ["text"],
               "tools": [{"type": "mcp", "server_label": "lmgw"}]}}),
    )
    .await;
    until(&mut ws, |e| e["type"] == "conversation.item.done").await;
    w.chat.push(turns_auth_off());
    send(&mut ws, user_text("turn auth off")).await;
    send(&mut ws, json!({"type": "response.create"})).await;
    let events = until(&mut ws, |e| e["type"] == "response.done").await;
    let call = events
        .iter()
        .filter(|e| e["type"] == "response.output_item.done")
        .map(|e| &e["item"])
        .find(|i| i["type"] == "mcp_call")
        .unwrap_or_else(|| panic!("the call: {events:#?}"));
    assert!(
        call["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains(REFUSED),
        "{call}"
    );
    assert!(w.state.snapshot().settings.auth_enabled);
}

#[tokio::test]
async fn the_access_settings_are_refused_in_a_bound_realtime_session() {
    let (w, desk) = full_device_world().await;
    let tid = toolset_thread(&w, &desk.client).await;
    let bearer = format!("Bearer {}", desk.key);
    let mut ws = w
        .connect(
            &format!("chat_thread={tid}"),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut ws).await["type"], "session.created");
    manual(&mut ws, 60_000).await;
    w.asr.push(Asr::Text("Turn auth off."));
    w.chat.push(turns_auth_off());
    w.chat.push(Turn::text(&["I could not."]));
    let before = w.chat.seen.chat_count();
    say(&mut ws).await;
    until_type(&mut ws, "lmgw.response.timing").await;
    let answered = w.chat.seen.chat(before + 1).to_string();
    assert!(answered.contains(REFUSED), "{answered}");
    assert!(w.state.snapshot().settings.auth_enabled);
}

/// The gateway's level set to `off` stops a device's running text turn on
/// a toolset thread at once, as its own switch-off does (review P-8): the
/// stream says `superseded`, and nothing more of the reply arrives.
#[tokio::test]
async fn the_gateway_s_off_stops_a_device_s_turn_on_a_toolset_thread() {
    let w = world(|_| {}).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let tools = self_admin_thread(&w).await;
    let hold = Arc::new(Notify::new());
    w.chat.push(Turn::Stream(vec![
        Step::Text("Thinking"),
        Step::Wait(hold.clone()),
        Step::Text(" more."),
        Step::Finish("stop"),
        Step::Usage(12, 8),
    ]));
    let mut resp = desk
        .client
        .post(format!("{}/chat/api/threads/{}/send", w.gw, tools.id))
        .json(&json!({ "content": "a long one" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut got = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !got.contains("Thinking") {
        let c = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .expect("the reply begins")
            .unwrap()
            .expect("not ended");
        got.push_str(&String::from_utf8_lossy(&c));
    }

    let (s, v) = op(&w, "settings_set_full", json!({ "self_admin": "off" })).await;
    assert_eq!(s, 200, "{v}");
    let rest = tokio::time::timeout(Duration::from_secs(10), resp.text())
        .await
        .expect("the stream ends")
        .unwrap();
    assert!(
        frames(&rest)
            .iter()
            .any(|(e, d)| e == "error" && d["code"] == "superseded"),
        "{rest}"
    );
    assert!(!rest.contains(" more."), "{rest}");
    let dropped = tokio::time::timeout(Duration::from_secs(5), async {
        while w.chat.seen.closed_early.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(dropped.is_ok(), "the upstream request was never dropped");
    hold.notify_one();
}

/// Away across its own switch-on and then the gateway's `off`: each move of
/// the reach it had is one `resync` at its place, as two switches of its
/// own are (review P-1), and nothing of the toolset's thread arrives — not
/// as the "on" would have brought it, nor what was written meanwhile.
#[tokio::test]
async fn a_catch_up_across_both_kinds_of_level_record() {
    let w = world(|_| {}).await;
    let tablet = pair(&w, "tablet", json!({})).await;
    let tools = self_admin_thread(&w).await;
    let away = Feed::open(&w, &tablet.client, "", None).await;
    assert_eq!(away.hello()["self_admin"], json!("off"));
    let cursor = away.cursor();
    drop(away);

    let (s, _) = op(
        &w,
        "key_set",
        json!({ "id": tablet.id, "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 200);
    super::seed(&w, tools.id, "written while it could see it").await;
    let (s, v) = op(&w, "settings_set_full", json!({ "self_admin": "off" })).await;
    assert_eq!(s, 200, "{v}");
    let last = super::chat_thread(&w, &w.gw.client(), "chatty").await;

    let mut back = Feed::open(&w, &tablet.client, "", Some(&cursor)).await;
    assert_eq!(back.hello()["self_admin"], json!("off"));
    back.until(10, |f| {
        f.iter()
            .any(|f| f.event == "thread.created" && subject(f) == last)
    })
    .await;
    assert!(
        about_thread(&back.frames, tools.id).is_empty(),
        "{:#?}",
        back.frames
    );
    let resyncs = back.named("resync");
    assert_eq!(resyncs.len(), 2, "one at each move: {:#?}", back.frames);
    assert!(resyncs.iter().all(|r| r.id.is_some()), "at their places");
}

/// Before the first save the level in force is the default, `read_only`:
/// a first save away from it is a move the feed records (review G-5); one
/// that keeps it records nothing.
#[tokio::test]
async fn the_first_save_that_moves_the_level_is_recorded() {
    for (level, records) in [(SelfAdmin::Off, 1), (SelfAdmin::ReadOnly, 0)] {
        let pool = lmgw_core::store::open_in_memory().await.unwrap();
        let settings = lmgw_core::config::Settings {
            self_admin: level,
            ..Default::default()
        };
        lmgw_core::store::save_settings(&pool, &settings)
            .await
            .unwrap();
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM chat_feed WHERE type = 'gateway.reach'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, records, "{level:?}");
    }
}

/// A level save whose reload fails is still a save (review G-6): it says
/// so beside the save, the saved settings apply, and what a moved level
/// does happens — the device's session on a toolset thread closes 4004.
#[tokio::test]
async fn a_level_save_whose_reload_fails_still_applies() {
    let w = world(|_| {}).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let tools = self_admin_thread(&w).await;
    let bearer = format!("Bearer {}", desk.key);
    let mut ws = w
        .connect(
            &format!("chat_thread={}", tools.id),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut ws).await["type"], "session.created");

    sqlx::query("ALTER TABLE candidate_aliases RENAME TO candidate_aliases_away")
        .execute(&w.state.db)
        .await
        .unwrap();
    let (s, v) = op(&w, "settings_set_full", json!({ "self_admin": "off" })).await;
    sqlx::query("ALTER TABLE candidate_aliases_away RENAME TO candidate_aliases")
        .execute(&w.state.db)
        .await
        .unwrap();
    assert_eq!(s, 200, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("could not be reloaded"),
        "{v}"
    );
    assert_eq!(w.state.snapshot().settings.self_admin, SelfAdmin::Off);
    let close = tokio::time::timeout(Duration::from_secs(10), async {
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::Message;
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(c))) => break c,
                Some(Ok(_)) => continue,
                other => panic!("expected the close, got {other:?}"),
            }
        }
    })
    .await
    .expect("the session closes")
    .map(|c| u16::from(c.code));
    assert_eq!(close, Some(4004));
}
