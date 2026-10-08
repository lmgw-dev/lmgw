//! Two guards on a device's admin tools (2026-10-07, after the client-apps
//! merge):
//!
//! - the settings that decide who reaches lmgw, and with what credential,
//!   are no tool's to change, whoever calls it (review G-1): a device at
//!   `full` is refused them by name in its own turn and still changes an
//!   ordinary setting, the owner's own turn is refused them too, and the
//!   dashboard's save changes them (L5's note);
//! - the gateway's self-admin level moving plays as a device's own level
//!   moving, for each device whose capped level moved with it: its feed says
//!   the level in a fresh `state`, its realtime session lists `lmgw` again,
//!   its bound session's next turn is offered the narrower set, and a write
//!   call is refused from the level's commit (L3's note);
//! - what a device sees follows the same capped level: the gateway's level
//!   set to `off` takes the toolset's threads and folders from a device at
//!   `read_only` as its own switch-off does — removals live, a `resync` in a
//!   catch-up, its bound session closed with the neutral 4004 — and raised
//!   again gives them back.
//!
//! Driven through the router as the device and as the owner: a Chat turn in
//! which the chat fake calls `lmgw__settings_set`, the feed, `/v1/realtime`,
//! and `settings_set_full` for the dashboard's save.

use lmgw_core::config::SelfAdmin;
use serde_json::{json, Value};

use super::admin_levels::call_as;
use super::{chat_thread, get, op, pair, post, self_admin_thread, sse, Device};
use crate::chat_feed::Feed;
use crate::realtime_chat_thread::{manual, next, say, until, until_type, world, World};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::{send, Step, Turn};

/// A thread of `client`'s with the self-admin toolset attached: its id.
pub(super) async fn toolset_thread(w: &World, client: &reqwest::Client) -> i64 {
    let tid = chat_thread(w, client, "chatty").await;
    let (s, v) = post(
        w,
        client,
        &format!("/chat/api/threads/{tid}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    tid
}

/// A turn of thread `tid`, sent by `client`, in which the model calls
/// `lmgw__settings_set` with `args`: what the tool answered.
async fn settings_set_in_turn(
    w: &World,
    client: &reqwest::Client,
    tid: i64,
    args: &'static str,
) -> String {
    tool_in_turn(w, client, tid, "lmgw__settings_set", args).await
}

/// A turn of thread `tid`, sent by `client`, in which the model calls
/// `name` with `args`: what the tool answered.
pub(super) async fn tool_in_turn(
    w: &World,
    client: &reqwest::Client,
    tid: i64,
    name: &'static str,
    args: &'static str,
) -> String {
    w.chat.push(Turn::Stream(vec![
        Step::CallStart {
            index: 0,
            id: Some("call_1"),
            name,
        },
        Step::CallArgs { index: 0, args },
        Step::Finish("tool_calls"),
        Step::Usage(10, 5),
    ]));
    w.chat.push(Turn::text(&["Done."]));
    let before = w.chat.seen.chat_count();
    let (s, frames) = sse(
        w,
        client,
        &format!("/chat/api/threads/{tid}/send"),
        json!({ "content": "change it" }),
    )
    .await;
    assert_eq!(s, 200);
    assert!(frames.iter().any(|(e, _)| e == "done"), "{frames:?}");
    let answered = w.chat.seen.chat(before + 1);
    let tool = answered["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .cloned()
        .unwrap_or_else(|| panic!("the call ran: {answered}"));
    match &tool["content"] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A device at `full`, under a gateway at `full`, is refused
/// `auth_enabled` and the self-admin level by name, and nothing changes —
/// an ordinary setting beside one included; alone, the ordinary setting is
/// its to change.
#[tokio::test]
async fn a_device_at_full_cannot_change_who_reaches_lmgw() {
    let w = world(|s| {
        s.self_admin = SelfAdmin::Full;
        s.auth_enabled = true;
        s.retention_days = 30;
    })
    .await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    let tid = toolset_thread(&w, &desk.client).await;

    for (args, key) in [
        (r#"{"auth_enabled": false}"#, "auth_enabled"),
        (r#"{"self_admin": "off"}"#, "self_admin"),
        (
            r#"{"retention_days": 7, "bind_addr": "0.0.0.0:8787"}"#,
            "bind_addr",
        ),
        (
            r#"{"agent_origin_suffix": "evil.example"}"#,
            "agent_origin_suffix",
        ),
    ] {
        let said = settings_set_in_turn(&w, &desk.client, tid, args).await;
        assert!(
            said.contains(&format!(
                "{key} cannot be changed through lmgw's admin tools"
            )) && said.contains("Change it on Settings"),
            "{said}"
        );
    }
    let s = w.state.snapshot().settings.clone();
    assert!(s.auth_enabled, "auth stays on");
    assert_eq!(s.self_admin, SelfAdmin::Full);
    assert_eq!(s.retention_days, 30, "nothing of a refused call is written");
    assert_eq!(s.agent_origin_suffix, "localhost");

    let said = settings_set_in_turn(&w, &desk.client, tid, r#"{"retention_days": 7}"#).await;
    assert!(said.contains("retention_days"), "{said}");
    assert!(!said.contains("cannot be changed"), "{said}");
    assert_eq!(w.state.snapshot().settings.retention_days, 7);
}

/// The owner's own turn is refused them too (review G-1): a model's call,
/// steered or not, never widens who reaches lmgw. The dashboard's save
/// changes both, with the owner's credential.
#[tokio::test]
async fn only_the_dashboard_changes_them() {
    let w = world(|s| {
        s.self_admin = SelfAdmin::Full;
        s.auth_enabled = true;
    })
    .await;
    let owner = w.gw.client();
    let tid = toolset_thread(&w, &owner).await;
    let said = settings_set_in_turn(&w, &owner, tid, r#"{"auth_enabled": false}"#).await;
    assert!(
        said.contains("auth_enabled cannot be changed through lmgw's admin tools"),
        "{said}"
    );
    assert!(w.state.snapshot().settings.auth_enabled);

    let (s, v) = op(
        &w,
        "settings_set_full",
        json!({ "auth_enabled": false, "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let settings = w.state.snapshot().settings.clone();
    assert!(!settings.auth_enabled);
    assert_eq!(settings.self_admin, SelfAdmin::ReadOnly);
}

/// The names of the tools `events`' last `conversation.item.done` lists.
fn listed(events: &[Value]) -> Vec<String> {
    events.last().unwrap()["item"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

/// The `lmgw__*` tools chat request `n` offered.
fn offered(w: &World, n: usize) -> Vec<String> {
    w.chat.seen.chat(n)["tools"]
        .as_array()
        .map(|t| {
            t.iter()
                .filter_map(|t| t["function"]["name"].as_str())
                .filter(|n| n.starts_with("lmgw__"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// One spoken turn of bound session `ws`: the `lmgw__*` tools it offered.
async fn bound_turn(w: &World, ws: &mut crate::support::realtime_fakes::Ws) -> Vec<String> {
    w.asr.push(Asr::Text("Wie geht es lmgw?"));
    w.chat.push(Turn::text(&["Gut."]));
    let before = w.chat.seen.chat_count();
    say(ws).await;
    until_type(ws, "lmgw.response.timing").await;
    offered(w, before)
}

/// A device's realtime session as `d`, without a thread, that carries the
/// `lmgw` label: the socket and the tools first listed.
async fn unbound_with_lmgw(
    w: &World,
    d: &Device,
) -> (crate::support::realtime_fakes::Ws, Vec<String>) {
    let bearer = format!("Bearer {}", d.key);
    let mut ws = w
        .connect("model=chatty", &[("authorization", bearer.as_str())])
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut ws).await["type"], "session.created");
    send(
        &mut ws,
        json!({"type": "session.update", "session": {"type": "realtime",
               "tools": [{"type": "mcp", "server_label": "lmgw"}]}}),
    )
    .await;
    let first = until(&mut ws, |e| e["type"] == "conversation.item.done").await;
    let names = listed(&first);
    (ws, names)
}

/// Publish a hold whose fallback alias is `marker` (a live event every
/// feed reads, the hold itself untouched), read it on each of `feeds`, and
/// count the `state` frames each holds by then.
async fn marked<const N: usize>(w: &World, feeds: [&mut Feed; N], marker: &str) -> [usize; N] {
    let alias = marker.to_string();
    crate::chat_feed::set(w, |s| s.hold.fallback_alias = Some(alias)).await;
    let mut counts = [0; N];
    for (i, f) in feeds.into_iter().enumerate() {
        f.until(10, |f| {
            f.iter()
                .any(|f| f.event == "hold" && f.data["fallback_alias"] == json!(marker))
        })
        .await;
        counts[i] = f.named("state").len();
    }
    counts
}

/// The gateway's level lowered from full to read only narrows a device at
/// full at once, as its own level lowered does: its feed's `state` says
/// `read_only`, its unbound session lists `lmgw` again without the write
/// tools, its bound session's next turn is offered none, and a write call
/// is refused — from the stored level too, before a snapshot holds it. A
/// device at read only, whose capped level did not move, hears no `state`,
/// and neither does the owner's feed. Raised again, the device hears
/// `full`.
#[tokio::test]
async fn the_gateway_s_level_lowered_narrows_a_device_at_full_at_once() {
    let w = world(|s| s.self_admin = SelfAdmin::Full).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "full" })).await;
    let phone = pair(&w, "phone", json!({ "self_admin": "read_only" })).await;
    let tools = self_admin_thread(&w).await;
    let mut feed = Feed::open(&w, &desk.client, "", None).await;
    let mut phones = Feed::open(&w, &phone.client, "", None).await;
    let mut owners = Feed::open(&w, &w.gw.client(), "", None).await;
    assert_eq!(feed.hello()["self_admin"], json!("full"));
    assert_eq!(phones.hello()["self_admin"], json!("read_only"));

    let (mut unbound, names) = unbound_with_lmgw(&w, &desk).await;
    assert!(names.contains(&"mcp_server_set".to_string()), "{names:?}");
    let bearer = format!("Bearer {}", desk.key);
    let mut bound = w
        .connect(
            &format!("chat_thread={}", tools.id),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut bound).await["type"], "session.created");
    manual(&mut bound, 60_000).await;
    let names = bound_turn(&w, &mut bound).await;
    assert!(names.contains(&"lmgw__hold_set".to_string()), "{names:?}");
    // The `state` frames the other feeds had before the move: everything
    // published up to a marker they have read.
    let before = marked(&w, [&mut phones, &mut owners], "before").await;

    let (s, v) = op(
        &w,
        "settings_set_full",
        json!({ "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");

    feed.until(10, |f| {
        f.iter()
            .any(|f| f.event == "state" && f.data["self_admin"] == json!("read_only"))
    })
    .await;
    let again = until(&mut unbound, |e| e["type"] == "conversation.item.done").await;
    assert!(
        again
            .iter()
            .any(|e| e["type"] == "mcp_list_tools.completed"),
        "{again:#?}"
    );
    let names = listed(&again);
    assert!(names.contains(&"status".to_string()), "{names:?}");
    assert!(!names.contains(&"mcp_server_set".to_string()), "{names:?}");
    let names = bound_turn(&w, &mut bound).await;
    assert!(names.contains(&"lmgw__status".to_string()), "{names:?}");
    assert!(
        !names
            .iter()
            .any(|n| n == "lmgw__hold_set" || n == "lmgw__settings_set"),
        "{names:?}"
    );
    let (refused, said) = call_as(&w, &desk, "lmgw__hold_set", json!({ "active": true })).await;
    assert!(refused && said.contains("read_only"), "{said}");
    assert!(!w.state.snapshot().settings.hold.active);

    // A live event published after the move reaches the other feeds behind
    // anything the move queued for them: neither heard a `state`.
    let after = marked(&w, [&mut phones, &mut owners], "after").await;
    assert_eq!(after, before, "{:#?}", phones.frames);

    // Raised again: the device hears `full`.
    let (s, v) = op(&w, "settings_set_full", json!({ "self_admin": "full" })).await;
    assert_eq!(s, 200, "{v}");
    feed.until(10, |f| {
        f.iter()
            .any(|f| f.event == "state" && f.data["self_admin"] == json!("full"))
    })
    .await;
    let back = until(&mut unbound, |e| e["type"] == "conversation.item.done").await;
    assert!(
        listed(&back).contains(&"mcp_server_set".to_string()),
        "{back:#?}"
    );

    // Lowered by a commit no snapshot holds yet: the stored level decides.
    sqlx::query(
        "UPDATE settings SET value = json_set(value, '$.self_admin', 'read_only') \
         WHERE key = 'settings'",
    )
    .execute(&w.state.db)
    .await
    .unwrap();
    assert_eq!(w.state.snapshot().settings.self_admin, SelfAdmin::Full);
    let (refused, said) = call_as(&w, &desk, "lmgw__hold_set", json!({ "active": true })).await;
    assert!(refused && said.contains("read_only"), "{said}");
    assert!(!w.state.snapshot().settings.hold.active);
}

/// The gateway's level is one of the three words on `/api/status` and
/// `/api/settings-full`, the bytes it always was, and reads as the typed
/// level the dashboard's DTOs carry.
#[tokio::test]
async fn the_gateway_s_level_is_said_as_it_always_was() {
    for (level, word) in [
        (SelfAdmin::Off, "off"),
        (SelfAdmin::ReadOnly, "read_only"),
        (SelfAdmin::Full, "full"),
    ] {
        let w = world(|s| s.self_admin = level).await;
        let pinned = format!("\"self_admin\":\"{word}\"");
        for path in ["/api/status", "/api/settings-full"] {
            let body =
                w.gw.client()
                    .get(format!("{}{path}", w.gw))
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap();
            assert!(body.contains(&pinned), "{path}: {body}");
        }
        let (_, status) = get(&w, &w.gw.client(), "/api/status").await;
        let status: lmgw_api_types::GatewayStatus = serde_json::from_value(status).unwrap();
        assert_eq!(status.self_admin, lmgw_api_types::AdminLevel::from(level));
        let (_, full) = get(&w, &w.gw.client(), "/api/settings-full").await;
        let full: lmgw_api_types::SettingsFull = serde_json::from_value(full).unwrap();
        assert_eq!(full.self_admin.as_str(), word);
    }
}

/// A folder of the owner's whose defaults attach the toolset: its id.
async fn toolset_folder(w: &World) -> i64 {
    let (s, v) = post(
        w,
        &w.gw.client(),
        "/chat/api/folders",
        json!({ "name": "Ops", "defaults": { "mcp_tools": [{ "server_label": "lmgw" }] } }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    v["id"].as_i64().unwrap()
}

/// A plain thread of the owner's, made to mark a point of the feed: its id.
async fn marker(w: &World) -> i64 {
    chat_thread(w, &w.gw.client(), "chatty").await
}

/// The close of `ws`, within 10 s.
async fn closed(ws: &mut crate::support::realtime_fakes::Ws) -> (u16, String) {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let close = tokio::time::timeout(std::time::Duration::from_secs(10), async {
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
    .expect("a close frame with a reason");
    (u16::from(close.code), close.reason.to_string())
}

/// The gateway's level set to `off` takes the toolset's threads and folders
/// from a device at `read_only` at once, as its own switch-off does: its
/// bound session on such a thread closes with the neutral 4004, its feed
/// hears the thread and the folder go, in a delete's order, then `state`
/// with `off`; by id the thread is not found, and it may not attach the
/// toolset. A device that was away across the change gets one `resync` at
/// it and nothing of the toolset's thread. Raised again, they come back.
#[tokio::test]
async fn the_gateway_s_level_set_to_off_hides_the_toolset_from_every_device() {
    let w = world(|s| s.self_admin = SelfAdmin::ReadOnly).await;
    let desk = pair(&w, "desktop", json!({ "self_admin": "read_only" })).await;
    let tablet = pair(&w, "tablet", json!({ "self_admin": "read_only" })).await;
    let tools = self_admin_thread(&w).await;
    let folder = toolset_folder(&w).await;
    let mut feed = Feed::open(&w, &desk.client, "", None).await;
    assert_eq!(feed.hello()["self_admin"], json!("read_only"));
    let away = Feed::open(&w, &tablet.client, "", None).await;
    let cursor = away.cursor();
    drop(away);
    let (s, _) = get(&w, &desk.client, &format!("/chat/api/threads/{}", tools.id)).await;
    assert_eq!(s, 200, "read only sees the toolset's thread");
    let bearer = format!("Bearer {}", desk.key);
    let mut bound = w
        .connect(
            &format!("chat_thread={}", tools.id),
            &[("authorization", bearer.as_str())],
        )
        .await
        .unwrap_or_else(|(s, b)| panic!("expected a 101, got {s}: {b}"));
    assert_eq!(next(&mut bound).await["type"], "session.created");

    let (s, v) = op(&w, "settings_set_full", json!({ "self_admin": "off" })).await;
    assert_eq!(s, 200, "{v}");

    let (code, reason) = closed(&mut bound).await;
    assert_eq!(
        (code, reason.as_str()),
        (
            4004,
            format!("chat thread {} is out of reach for this key", tools.id).as_str()
        ),
        "neutral, as the device's own switch-off closes it"
    );
    // Not a revocation: the key is still good, and a client asks for its
    // current thread again rather than pairing.
    assert_eq!(
        lmgw_client::realtime::close_kind(code, &reason),
        lmgw_client::realtime::CloseKind::OutOfReach
    );
    feed.until(10, |f| {
        f.iter().any(|f| f.event == "folder.deleted")
            && f.iter()
                .any(|f| f.event == "state" && f.data["self_admin"] == json!("off"))
    })
    .await;
    let order: Vec<(String, i64)> = feed
        .frames
        .iter()
        .filter(|f| f.event == "thread.deleted" || f.event == "folder.deleted")
        .map(|f| (f.event.clone(), crate::chat_feed::subject(f)))
        .collect();
    assert_eq!(
        order,
        [
            ("thread.deleted".to_string(), tools.id),
            ("folder.deleted".to_string(), folder),
        ],
        "{:#?}",
        feed.frames
    );
    let (s, _) = get(&w, &desk.client, &format!("/chat/api/threads/{}", tools.id)).await;
    assert_eq!(s, 404);
    let own = chat_thread(&w, &desk.client, "chatty").await;
    let (s, v) = post(
        &w,
        &desk.client,
        &format!("/chat/api/threads/{own}/settings"),
        json!({ "mcp_tools": [{ "server_label": "lmgw" }] }),
    )
    .await;
    assert_eq!(s, 403, "{v}");
    assert!(
        v.to_string().contains("not allowed lmgw's admin tools"),
        "{v}"
    );

    // Away across it: one `resync`, at its place, and nothing of the thread.
    let last = marker(&w).await;
    let mut back = Feed::open(&w, &tablet.client, "", Some(&cursor)).await;
    assert_eq!(back.hello()["self_admin"], json!("off"));
    back.until(10, |f| {
        f.iter()
            .any(|f| f.event == "thread.created" && crate::chat_feed::subject(f) == last)
    })
    .await;
    let resyncs = back.named("resync");
    assert_eq!(resyncs.len(), 1, "one at the change: {:#?}", back.frames);
    assert!(resyncs[0].id.is_some(), "carrying the change's cursor");
    assert!(
        crate::chat_feed::about_thread(&back.frames, tools.id).is_empty(),
        "{:#?}",
        back.frames
    );

    // Raised again: the folder and the thread come back, then `state`.
    let (s, v) = op(
        &w,
        "settings_set_full",
        json!({ "self_admin": "read_only" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    feed.until(10, |f| {
        f.iter()
            .any(|f| f.event == "thread.created" && crate::chat_feed::subject(f) == tools.id)
            && f.iter()
                .any(|f| f.event == "state" && f.data["self_admin"] == json!("read_only"))
    })
    .await;
    assert!(
        feed.frames
            .iter()
            .any(|f| { f.event == "folder.created" && crate::chat_feed::subject(f) == folder }),
        "{:#?}",
        feed.frames
    );
    let (s, _) = get(&w, &desk.client, &format!("/chat/api/threads/{}", tools.id)).await;
    assert_eq!(s, 200);
}
