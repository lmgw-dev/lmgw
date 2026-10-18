//! The route (§2.1–§2.5, §2.7): both bodies, the answer and `Location`,
//! the `session` part as the session's first update, §2.5's refusals and
//! the sideband refusal on both routes, `/v1/models`, and what
//! `lmgw-client` reads of it all.

use lmgw_client::requests::calls::{read_call, read_hangup, HungUp};
use lmgw_core::realtime::calls::{CallState, Group};
use serde_json::{json, Value};

use super::{
    anonymous, calls_world, hangup, multipart, post_chunked, post_raw, post_sdp, Answer, Phone,
};
use crate::device_chat::{get, pair};
use crate::realtime_calls_net::devices::devices_listening;
use crate::realtime_chat_thread::world;

#[tokio::test]
async fn a_raw_offer_is_answered_and_its_session_starts_when_the_channel_opens() {
    let w = calls_world(|_| {}).await;
    let (c, sdp) = Phone::offer().await;
    let a = post_sdp(&w, &anonymous(), "model=chatty", &sdp).await;
    assert_eq!(a.status, 201, "{}", a.body);
    assert_eq!(a.content_type.as_deref(), Some("application/sdp"));
    // What lmgw-client reads of it (WP5's reader).
    let read = read_call(a.status, a.location.as_deref(), &a.body).unwrap();
    assert!(read.call_id.starts_with("rtc_") && read.call_id.len() == 28);
    assert_eq!(
        a.location.as_deref(),
        Some(format!("/v1/realtime/calls/{}", read.call_id).as_str())
    );
    // ICE-lite, Opus only, one host candidate: the POST's own address.
    assert!(a.body.contains("a=ice-lite"), "{}", a.body);
    assert!(a.body.contains("opus/48000/2"), "{}", a.body);
    assert_eq!(a.body.matches("a=candidate:").count(), 1, "{}", a.body);
    assert!(a.body.contains("127.0.0.1"), "{}", a.body);

    let mut p = Phone::answered(c, a);
    let info = w.state.calls.get(&p.id).unwrap();
    assert_eq!(info.state, CallState::Answered);
    p.c.connect().await;
    let created = p.next().await;
    assert_eq!(created["type"], "session.created", "{created}");
    assert_eq!(created["session"]["model"], "chatty");
    assert!(created["event_id"].is_string(), "{created}");
    // An unbound call is OpenAI's: no thread, no turn mode.
    assert!(created["session"]["lmgw"]["resolved"]
        .get("chat_thread")
        .is_none());
    let info = w.state.calls.get(&p.id).unwrap();
    assert_eq!(info.state, CallState::Running);
    assert_eq!(
        info.session.as_deref(),
        created["session"]["id"].as_str(),
        "the registry names the session"
    );
    assert_eq!(info.thread, None);

    // The session answers on the channel.
    p.send(
        json!({"type": "session.update", "session": {"type": "realtime",
        "instructions": "Be brief."}}),
    );
    let updated = p.next().await;
    assert_eq!(updated["type"], "session.updated", "{updated}");
    assert_eq!(updated["session"]["instructions"], "Be brief.");
    // A hangup of a call that ended reads as ended.
    assert_eq!(p.hangup(&w, &anonymous()).await, 200);
    assert!(
        p.closed().await.is_empty(),
        "nothing in band on an unbound call"
    );
    let status = hangup(&w, &anonymous(), &p.id).await;
    assert_eq!(read_hangup(status, ""), Ok(HungUp::AlreadyEnded));
}

#[tokio::test]
async fn the_session_part_is_the_sessions_first_update() {
    let w = calls_world(|_| {}).await;
    let (c, sdp) = Phone::offer().await;
    let session = json!({"type": "realtime", "instructions": "From the part."}).to_string();
    let (ct, body) = multipart(&[
        ("sdp", "application/sdp", &sdp),
        ("session", "application/json", &session),
        ("ignored", "text/plain", "an unknown part is ignored"),
    ]);
    let a = post_raw(
        &w,
        &anonymous(),
        "/v1/realtime/calls?model=chatty",
        &ct,
        body,
    )
    .await;
    let mut p = Phone::answered(c, a);
    p.c.connect().await;
    let created = p.next().await;
    assert_eq!(created["type"], "session.created", "{created}");
    let updated = p.next().await;
    assert_eq!(updated["type"], "session.updated", "{updated}");
    assert_eq!(updated["session"]["instructions"], "From the part.");

    // A part session.update refuses is answered as any update: an error.
    let (c, sdp) = Phone::offer().await;
    let session = json!({"type": "realtime", "output_modalities": ["video"]}).to_string();
    let (ct, body) = multipart(&[
        ("session", "application/json", &session),
        ("sdp", "application/sdp", &sdp),
    ]);
    let a = post_raw(
        &w,
        &anonymous(),
        "/v1/realtime/calls?model=chatty",
        &ct,
        body,
    )
    .await;
    let mut p = Phone::answered(c, a);
    p.c.connect().await;
    assert_eq!(p.next().await["type"], "session.created");
    let refused = p.next().await;
    assert_eq!(refused["type"], "error", "{refused}");
}

/// The call lmgw-client builds (WP5's `requests::calls::call`), as the
/// Android client sends it: by folder, `takeover=ask`, its client-owned
/// fields in the `session` part; and its hangup.
#[tokio::test]
async fn lmgw_clients_own_request_is_answered_and_its_session_part_applied() {
    use lmgw_client::realtime::BoundSession;
    use lmgw_client::requests::calls::{call, hangup as hangup_req, CallBind, Takeover};
    let w = calls_world(|_| {}).await;
    let owner = w.gw.client();
    let folder = crate::chat_ongoing::ongoing_folder(&w, &owner, "Assistant", 30, json!({})).await;
    let (mut c, sdp) = Phone::offer().await;
    let session = BoundSession {
        half_duplex: Some(true),
        ..BoundSession::default()
    };
    let req = call(
        CallBind::Folder {
            id: folder,
            new: false,
        },
        Takeover::Ask,
        &sdp,
        Some(&session),
    );
    let send = |req: lmgw_client::requests::Request| {
        let mut r = owner.post(format!("{}{}", w.gw, req.path));
        for h in &req.headers {
            r = r.header(&h.name, &h.value);
        }
        if let Some(body) = req.body {
            r = r.body(body);
        }
        r.send()
    };
    let r = send(req).await.unwrap();
    let status = r.status().as_u16();
    let location = r
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = r.text().await.unwrap();
    let answer = read_call(status, location.as_deref(), &body).unwrap();
    assert!(answer.max_message_size() > 0);
    c.accept(&answer.sdp);
    let mut p = Phone {
        c,
        id: answer.call_id.clone(),
    };
    p.c.connect().await;
    let created = p.next_but_states().await;
    assert_eq!(created["type"], "session.created", "{created}");
    assert!(created["session"]["lmgw"]["resolved"]["chat_thread"]["id"].is_i64());
    let updated = p.next_but_states().await;
    assert_eq!(updated["type"], "session.updated", "{updated}");
    assert_eq!(updated["session"]["lmgw"]["half_duplex"], true, "{updated}");

    let r = send(hangup_req(&answer.call_id)).await.unwrap();
    let status = r.status().as_u16();
    assert_eq!(
        read_hangup(status, &r.text().await.unwrap()),
        Ok(HungUp::Ended)
    );
    let last = p.closed().await;
    assert_eq!(last.last().unwrap()["kind"], "closed", "{last:?}");
}

/// `(what, answer)`: the refusal's status and code.
fn refused(what: &str, a: &Answer, status: u16, code: &str) {
    assert_eq!(
        (a.status, a.code().as_str()),
        (status, code),
        "{what}: {}",
        a.body
    );
}

#[tokio::test]
async fn every_refusal_before_the_answer() {
    let w = calls_world(|s| s.max_body_mb = 1).await;
    let owner = w.gw.client();
    let (_c, sdp) = Phone::offer().await;
    let tid = w.thread("chatty", json!({})).await;
    let call = "/v1/realtime/calls";

    let a = post_raw(&w, &owner, call, "application/json", "{}".into()).await;
    refused("JSON", &a, 415, "unsupported_media_type");
    let (ct, body) = multipart(&[("session", "application/json", "{}")]);
    let a = post_raw(&w, &owner, call, &ct, body).await;
    refused("no sdp part", &a, 400, "missing_sdp");
    for part in ["[1]", "not json"] {
        let (ct, body) = multipart(&[
            ("sdp", "application/sdp", &sdp),
            ("session", "application/json", part),
        ]);
        let a = post_raw(&w, &owner, call, &ct, body).await;
        refused(part, &a, 400, "invalid_value");
        assert_eq!(a.json()["error"]["param"], "session", "{}", a.body);
    }
    let a = post_sdp(&w, &owner, "", "v=0\r\nthis is no offer").await;
    refused("garbage", &a, 400, "invalid_sdp");
    // Over max_body_mb: declared, and chunked with nothing declared, raw and
    // multipart — the bound tripping as the handler reads.
    let a = post_raw(&w, &owner, call, "application/sdp", "x".repeat(1 << 21)).await;
    refused("declared", &a, 413, "body_limit");
    let over = "x".repeat((1 << 20) + (64 << 10));
    let a = post_chunked(&w, &owner, call, "application/sdp", over.clone()).await;
    refused("chunked", &a, 413, "body_limit");
    let (ct, body) = multipart(&[("sdp", "application/sdp", &over)]);
    let a = post_chunked(&w, &owner, call, &ct, body).await;
    refused("chunked multipart", &a, 413, "body_limit");
    let (ct, body) = multipart(&[
        ("ignored", "text/plain", &over),
        ("sdp", "application/sdp", &sdp),
    ]);
    let a = post_chunked(&w, &owner, call, &ct, body).await;
    refused("chunked multipart, an ignored part", &a, 413, "body_limit");
    let a = post_raw(
        &w,
        &owner,
        call,
        "multipart/form-data; boundary=b",
        "no parts".into(),
    )
    .await;
    refused("multipart that does not parse", &a, 400, "bad_request");

    // An offer without a data channel, and one without Opus.
    let no_channel: String = sdp
        .split("\r\n")
        .take_while(|l| !l.starts_with("m=application"))
        .map(|l| match l.strip_prefix("a=group:BUNDLE ") {
            // The application's mid, last, leaves the bundle with it.
            Some(mids) => {
                let mut mids: Vec<&str> = mids.split(' ').collect();
                mids.pop();
                format!("a=group:BUNDLE {}\r\n", mids.join(" "))
            }
            None => format!("{l}\r\n"),
        })
        .collect();
    let a = post_sdp(&w, &owner, "", &no_channel).await;
    refused("no channel", &a, 400, "unsupported_offer");
    assert!(a.body.contains("oai-events"), "{}", a.body);
    let no_opus = sdp.replace("opus/48000/2", "PCMU/8000");
    let a = post_sdp(&w, &owner, "", &no_opus).await;
    refused("no Opus", &a, 400, "unsupported_offer");

    // The query's rules.
    for (query, code) in [
        ("chat_thread=seven", "invalid_value"),
        (
            &*format!("chat_folder=1&chat_thread={tid}"),
            "invalid_value",
        ),
        ("new=true", "invalid_value"),
        ("takeover=never", "invalid_value"),
        ("takeover=sometimes", "invalid_value"),
        (
            &*format!("model=chatty&chat_thread={tid}"),
            "owned_by_thread",
        ),
        ("call_id=rtc_1", "sideband_unsupported"),
    ] {
        let a = post_sdp(&w, &owner, query, &sdp).await;
        refused(query, &a, 400, code);
    }
    let beta =
        w.gw.client()
            .post(format!("{}{call}", w.gw))
            .header("content-type", "application/sdp")
            .header("openai-beta", "realtime=v1")
            .body(sdp.clone())
            .send()
            .await
            .unwrap();
    assert_eq!(beta.status(), 400);
    let v: Value = beta.json().await.unwrap();
    assert_eq!(v["error"]["code"], "beta_protocol_unsupported");
    let foreign = anonymous()
        .post(format!("{}{call}", w.gw))
        .header("content-type", "application/sdp")
        .header("origin", "https://elsewhere.example")
        .body(sdp.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 403);
    let v: Value = foreign.json().await.unwrap();
    assert_eq!(v["error"]["code"], "cross_origin_refused");

    // The bind's own, checked on the thread.
    let a = post_sdp(&w, &owner, "chat_thread=9999", &sdp).await;
    refused("no thread", &a, 404, "chat_thread_not_found");
    let a = post_sdp(&w, &owner, "chat_folder=9999", &sdp).await;
    refused("no folder", &a, 404, "chat_folder_not_found");
    let a = post_sdp(&w, &anonymous(), &format!("chat_thread={tid}"), &sdp).await;
    refused("no chat capability", &a, 403, "chat_thread_not_allowed");

    // Nothing was answered: no call is live.
    assert!(w.state.calls.list().is_empty());
}

#[tokio::test]
async fn the_binds_conflicts_are_refused_at_the_post() {
    let w = calls_world(|_| {}).await;
    let owner = w.gw.client();
    let (_c, sdp) = Phone::offer().await;
    let (_, v) =
        crate::device_chat::post(&w, &owner, "/chat/api/folders", json!({"name": "Plain"})).await;
    let plain = v["id"].as_i64().unwrap();
    let a = post_sdp(&w, &owner, &format!("chat_folder={plain}"), &sdp).await;
    refused("a folder that is no ongoing one", &a, 409, "not_ongoing");
    let bare = crate::chat_ongoing::ongoing_folder(&w, &owner, "Bare", 30, json!({})).await;
    sqlx::query("UPDATE chat_folders SET defaults = '{}' WHERE id = ?1")
        .bind(bare)
        .execute(&w.state.db)
        .await
        .unwrap();
    let a = post_sdp(&w, &owner, &format!("chat_folder={bare}"), &sdp).await;
    refused(
        "a folder whose defaults name no model",
        &a,
        409,
        "folder_no_model",
    );

    // Held by another key: never is the 409 at once, naming the holder;
    // ask and the default are no refusal at the POST.
    let tid = w.thread("chatty", json!({})).await;
    let desktop = crate::device_chat::pair(&w, "desktop", json!({})).await;
    let _ws = w
        .connect_as(&desktop, &format!("chat_thread={tid}"))
        .await
        .unwrap_or_else(|(s, b)| panic!("{s}: {b}"));
    let a = post_sdp(
        &w,
        &owner,
        &format!("chat_thread={tid}&takeover=never"),
        &sdp,
    )
    .await;
    refused("held", &a, 409, "chat_thread_bound");
    assert_eq!(
        a.json()["error"]["held_by"],
        "device 'desktop'",
        "{}",
        a.body
    );
    let read = read_call(a.status, a.location.as_deref(), &a.body).unwrap_err();
    assert_eq!(read.held_by.as_deref(), Some("device 'desktop'"));
    for query in [
        format!("chat_thread={tid}&takeover=ask"),
        format!("chat_thread={tid}"),
    ] {
        let (_c, sdp) = Phone::offer().await;
        let a = post_sdp(&w, &owner, &query, &sdp).await;
        assert_eq!(a.status, 201, "{query}: {}", a.body);
    }
}

#[tokio::test]
async fn a_listener_without_its_socket_group_answers_503_naming_why() {
    // Never bound: as before the listener started.
    let w = world(|s| s.realtime.tts_alias = "speak".into()).await;
    let (_c, sdp) = Phone::offer().await;
    let a = post_sdp(&w, &anonymous(), "", &sdp).await;
    assert_eq!(
        (a.status, a.code().as_str()),
        (503, "webrtc_unavailable"),
        "{}",
        a.body
    );
    assert_eq!(a.json()["error"]["type"], "server_error");

    // Off: the message names the setting.
    let off = lmgw_core::config::WebRtcSettings {
        webrtc_enabled: false,
        ..Default::default()
    };
    let released = w.state.calls.serve_plain(&off, w.gw.addr(), None).await;
    assert!(released.is_none());
    let a = post_sdp(&w, &anonymous(), "", &sdp).await;
    assert_eq!(a.status, 503, "{}", a.body);
    assert!(a.body.contains("realtime.webrtc_enabled"), "{}", a.body);
    // The model list does not offer what is off.
    chatty_is_chat(&w).await;
    let models = w.get("/v1/models").await;
    let endpoints = endpoints_of(&models, "chatty");
    assert!(
        endpoints.iter().any(|e| e == "/v1/realtime"),
        "{endpoints:?}"
    );
    assert!(
        !endpoints.iter().any(|e| e == "/v1/realtime/calls"),
        "{endpoints:?}"
    );
}

/// The fake's `chatty` declared a chat model by the owner, so the list
/// gives it its routes (the fake upstream's catalog says nothing).
async fn chatty_is_chat(w: &crate::realtime_chat_thread::World) {
    sqlx::query(
        "UPDATE models SET capabilities_override = '{\"capabilities\": {\"task\": \"chat\"}}'
         WHERE alias = 'chatty'",
    )
    .execute(&w.state.db)
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
}

/// The `capabilities.endpoints` of model `id` in a `/v1/models` list.
fn endpoints_of(models: &Value, id: &str) -> Vec<String> {
    models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .and_then(|m| m["capabilities"]["endpoints"].as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `/v1/models` lists `/v1/realtime/calls` per listener while that
/// listener's socket group is bound (`advertise::calls`, calls design §9):
/// the plain group on the plain listener, the device group on the device
/// listener, each whatever the other does. The route answers either way.
#[tokio::test]
async fn models_list_the_calls_route_where_the_listeners_group_is_bound() {
    let w = calls_world(|s| s.realtime.tts_alias = "speak".into()).await;
    chatty_is_chat(&w).await;
    let calls = |models: &Value| {
        let endpoints = endpoints_of(models, "chatty");
        assert!(
            endpoints.iter().any(|e| e == "/v1/realtime"),
            "{endpoints:?}"
        );
        endpoints.iter().any(|e| e == "/v1/realtime/calls")
    };
    // The plain listener, its group bound.
    assert!(
        calls(&w.get("/v1/models").await),
        "the plain group is bound"
    );
    // The device listener, its group bound as it starts; a paired device
    // reads the list through it.
    devices_listening(&w.state).await;
    let phone = pair(&w, "phone", json!({})).await;
    let on_device = || async {
        let (status, models) = get(&w, &phone.client, "/v1/models").await;
        assert_eq!(status, 200, "{models}");
        models
    };
    assert!(calls(&on_device().await), "the device group is bound");
    // The device group closed (its listener still serving): gone there, and
    // still listed on the plain listener.
    w.state.calls.close_group(Group::Devices).await;
    assert!(!calls(&on_device().await), "the device group is down");
    assert!(
        calls(&w.get("/v1/models").await),
        "the plain group is bound"
    );
    let (_c, sdp) = Phone::offer().await;
    let a = post_sdp(&w, &anonymous(), "model=chatty", &sdp).await;
    assert_eq!(a.status, 201, "{}", a.body);
    // The plain group closed: gone there too.
    w.state.calls.close_group(Group::Plain).await;
    assert!(
        !calls(&w.get("/v1/models").await),
        "the plain group is down"
    );
}

/// The `session` part is a session event: `realtime.max_message_mb` bounds
/// it, as it bounds a WebSocket's `session.update`, whatever `max_body_mb`
/// allows the body.
#[tokio::test]
async fn the_session_part_is_bounded_by_max_message_mb() {
    let w = calls_world(|s| {
        s.realtime.max_message_mb = 1;
        s.max_body_mb = 4;
    })
    .await;
    let (_c, sdp) = Phone::offer().await;
    let session =
        json!({"type": "realtime", "instructions": "i".repeat((1 << 20) + 1)}).to_string();
    let (ct, body) = multipart(&[
        ("sdp", "application/sdp", &sdp),
        ("session", "application/json", &session),
    ]);
    let a = post_raw(
        &w,
        &anonymous(),
        "/v1/realtime/calls?model=chatty",
        &ct,
        body,
    )
    .await;
    refused("a session part over the bound", &a, 400, "invalid_value");
    assert_eq!(a.json()["error"]["param"], "session", "{}", a.body);
    assert!(a.body.contains("realtime.max_message_mb"), "{}", a.body);
    assert!(w.state.calls.list().is_empty());
}

#[tokio::test]
async fn the_websocket_route_refuses_call_id_and_ask() {
    let w = world(|_| {}).await;
    let tid = w.thread("chatty", json!({})).await;
    let (status, body) = w.connect("call_id=rtc_1", &[]).await.unwrap_err();
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (400, Some("sideband_unsupported")),
        "{body}"
    );
    let cookie = w.cookie();
    let (status, body) = w
        .connect(
            &format!("chat_thread={tid}&takeover=ask"),
            &[("cookie", &cookie)],
        )
        .await
        .unwrap_err();
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (400, Some("invalid_value")),
        "{body}"
    );
}
