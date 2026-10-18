//! A call's size bounds (calls design §6.3): on a bound call parts both
//! ways — the phone's splitter and joiner are lmgw-client's, so the contract
//! is read from the other side — a broken set and one past
//! `realtime.max_message_mb` refused; on an unbound call an oversized event
//! with its largest fields emptied and one `event_too_large` per field, and a
//! part refused.
//!
//! The sizes come from `measure`: of a bound self-admin session's events
//! only the chat turn's tool results pass libwebrtc's 256 KiB — an image
//! result from an image of 192 KiB on (a 500 KiB PNG's is 683 KB, three
//! parts), a text result from a text of about 117 KiB on (the text goes
//! twice) — so the parts here carry an event as large as a 500 KiB image's
//! result ([`LARGE`]) against the phone's real 256 KiB, and the emptied
//! fields a 100 KiB text against the 64 KiB a phone that names no limit
//! allows.

use lmgw_client::realtime::parts::max_message_size;
use lmgw_client::realtime::{Joiner, Splitter};
use serde_json::{json, Value};
use str0m::Rtc;

use super::{bound_with, manual, openai_types, types, unbound, unbound_with, update, Call};
use crate::realtime_calls::calls_world;
use crate::realtime_calls_net::client::Client;
use crate::support::realtime_fakes::Turn;

/// The text the parts carry, in characters: an event about as large as a
/// 500 KiB image's tool result (`measure`).
const LARGE: usize = 500 << 10;

/// What a phone advertises, as libwebrtc does.
const PHONE_LIMIT: usize = 256 << 10;

/// A phone whose own send buffer holds every part it sends at once (str0m's
/// default, 128 KiB, would refuse a part over it).
async fn phone() -> (Client, String) {
    let builder = Rtc::builder().set_sctp_max_buffered_amount(8 << 20);
    Client::with("127.0.0.1".parse().unwrap(), builder).await
}

/// `call`'s messages joined by lmgw-client's joiner: each whole event's
/// text, and for each set of parts its id and the messages it spanned, in
/// order.
fn joined(call: &Call) -> (Vec<String>, Vec<(String, std::ops::Range<usize>)>) {
    let mut j = Joiner::new();
    let (mut events, mut sets) = (Vec::new(), Vec::new());
    let mut start = None;
    for (i, m) in call.rec.messages.iter().enumerate() {
        let v: Value = serde_json::from_str(m).unwrap();
        if v["type"] == "lmgw.event.part" && v["index"] == 0 {
            start = Some(i);
        }
        let out = j.push(m.clone());
        assert!(out.broken.is_empty(), "{:?}", out.broken);
        if let Some(e) = out.event {
            if v["type"] == "lmgw.event.part" {
                let id = v["id"].as_str().unwrap().to_string();
                sets.push((id, start.take().unwrap()..i + 1));
            }
            events.push(e);
        }
    }
    (events, sets)
}

/// The phone asks for a response with metadata of [`LARGE`], split by
/// lmgw-client's splitter at the limit lmgw's answer advertises: lmgw joins
/// it and applies it. Its `response.created` and `response.done`, which
/// echo the metadata, come back as parts, each within the phone's 256 KiB,
/// back to back with nothing between them, `id` the event's own
/// `event_id`; lmgw-client's joiner joins each to the event, and `stopped`
/// follows the joined `response.done`.
#[tokio::test]
async fn a_bound_calls_events_over_the_limit_travel_as_parts_both_ways() {
    let w = calls_world(|_| {}).await;
    w.chat.push(Turn::text(&["Trains leave every hour."]));
    let (c, sdp) = phone().await;
    let mut call = bound_with(&w, c, &sdp).await;
    update(&mut call, manual()).await;
    let limit = max_message_size(&call.answer);
    assert_eq!(limit, PHONE_LIMIT as u64, "str0m's own, advertised");
    let note: String = "Ein Zug fährt stündlich. \"Gleis 3\"\n"
        .chars()
        .cycle()
        .take(LARGE)
        .collect();
    let silence = crate::realtime_calls_audio::opus(&crate::support::realtime_audio::silence(200));
    call.speak(&silence).await;
    call.send(json!({"type": "input_audio_buffer.commit"}));
    let create = json!({"type": "response.create", "response": {"metadata": {"note": note}}});
    let parts = Splitter::new(limit).split(&create.to_string()).unwrap();
    assert!(parts.len() >= 3, "{} parts", parts.len());
    for p in &parts {
        call.p.c.send_text(p);
    }
    // `stopped` comes right after the done's last part.
    call.until_type("output_audio_buffer.stopped").await;
    for m in &call.rec.messages {
        assert!(m.len() <= PHONE_LIMIT, "a message of {} bytes", m.len());
    }
    let (events, sets) = joined(&call);
    let parsed: Vec<Value> = events
        .iter()
        .map(|e| serde_json::from_str(e).unwrap())
        .collect();
    let created = parsed
        .iter()
        .find(|e| e["type"] == "response.created")
        .expect("the response.created, joined");
    assert_eq!(created["response"]["metadata"]["note"], note.as_str());
    let done = parsed
        .iter()
        .find(|e| e["type"] == "response.done")
        .expect("the response.done, joined");
    assert_eq!(done["response"]["metadata"]["note"], note.as_str());
    assert_eq!(
        done["response"]["status"], "completed",
        "{}",
        done["response"]["status"]
    );
    // Two sets, each the parts of one event, back to back.
    assert!(sets.len() >= 2, "{sets:?}");
    for (id, span) in &sets {
        for m in &call.rec.messages[span.clone()] {
            let v: Value = serde_json::from_str(m).unwrap();
            assert_eq!(v["type"], "lmgw.event.part", "nothing between the parts");
            assert_eq!(v["id"], id.as_str());
        }
        let whole = events
            .iter()
            .map(|e| serde_json::from_str::<Value>(e).unwrap())
            .find(|e| e["event_id"] == id.as_str());
        assert!(whole.is_some(), "the set's id is its event's event_id");
    }
    let t = types(&parsed);
    let done_at = t.iter().position(|t| *t == "response.done").unwrap();
    assert_eq!(t[done_at + 1], "output_audio_buffer.stopped", "{t:?}");
    // lmgw-client reads them as the events they are.
    assert!(events.iter().any(|e| matches!(
        lmgw_client::realtime::parse(e),
        Some(lmgw_client::realtime::ServerEvent::ResponseCreated { .. })
    )));
}

/// A set broken by another message is refused naming its id, the message
/// goes on as the event it is, and the set's last part is dropped without a
/// word; a set that joins past `realtime.max_message_mb` (1 MiB here) is
/// refused naming the setting. The session goes on.
#[tokio::test]
async fn a_broken_set_and_one_past_max_message_mb_are_refused_naming_the_set() {
    let w = calls_world(|s| s.realtime.max_message_mb = 1).await;
    let (c, sdp) = phone().await;
    let mut call = bound_with(&w, c, &sdp).await;
    let part = |id: &str, index: usize, count: usize, data: &str| {
        json!({"type": "lmgw.event.part", "event_id": format!("{id}_{index}"), "id": id,
               "index": index, "count": count, "data": data})
        .to_string()
    };
    call.p.c.send_text(&part("set_a", 0, 2, "{\"type\":"));
    let after = call.probe().await;
    let t = openai_types(&after);
    assert_eq!(t, ["error", "session.updated"], "{after:?}");
    let e = &after[0]["error"];
    assert_eq!(e["code"], "invalid_event");
    let message = e["message"].as_str().unwrap();
    assert!(
        message.contains("set_a") && message.contains("another message came between"),
        "{message}"
    );
    call.p.c.send_text(&part("set_a", 1, 2, "\"x\"}"));
    assert_eq!(openai_types(&call.probe().await), ["session.updated"]);

    // Five parts joining to the setting plus one byte. One setting bounds the
    // join and the call's inbound budget alike (§3.7), so the parts cannot be
    // read one by one (a message between them breaks the set); with the set
    // this close to the setting, the budget trips only if the session has
    // read none of its parts by the time the fifth is sent.
    let limit = 1usize << 20;
    let each = limit / 5;
    for i in 0..4 {
        call.p.c.send_text(&part("set_b", i, 5, &"x".repeat(each)));
    }
    call.p
        .c
        .send_text(&part("set_b", 4, 5, &"x".repeat(limit + 1 - 4 * each)));
    let e = call.next_of("error").await;
    assert_eq!(e["error"]["code"], "event_too_large", "{e}");
    assert_eq!(e["error"]["event_id"], "set_b_4");
    let message = e["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("set_b") && message.contains("realtime.max_message_mb (1048576 bytes)"),
        "{message}"
    );
    assert_eq!(openai_types(&call.probe().await), ["session.updated"]);
}

/// A stock call's phone takes messages of up to 64 KiB (what an SDP without
/// `a=max-message-size` allows): a text answer of 100 KiB comes with its
/// largest field emptied in every event that carries it — the delta, the
/// text's done, the part's, the item's, `response.done` — each still its
/// type with every id, and right behind each one `event_too_large` naming
/// the field's path, its size and the limit. Nothing goes over the limit.
#[tokio::test]
async fn an_unbound_calls_oversized_event_arrives_with_its_largest_fields_emptied() {
    let w = calls_world(|_| {}).await;
    let text: String = "the train leaves at noon; "
        .chars()
        .cycle()
        .take(100 << 10)
        .collect();
    w.chat.push(Turn::text(&[Box::leak(text.into_boxed_str())]));
    let (c, sdp) = Client::new("127.0.0.1".parse().unwrap()).await;
    let sdp = sdp.replace("a=max-message-size:262144", "a=max-message-size:65536");
    let mut call = unbound_with(&w, c, &sdp).await;
    update(
        &mut call,
        json!({"type": "session.update", "session": {"type": "realtime",
            "output_modalities": ["text"]}}),
    )
    .await;
    call.send(
        json!({"type": "conversation.item.create", "item": {"type": "message",
        "role": "user", "content": [{"type": "input_text", "text": "when?"}]}}),
    );
    call.send(json!({"type": "response.create"}));
    let events = call
        .until("response.done's error", |v| {
            v["type"] == "error" && v["error"]["param"] == "response.output[0].content[0].text"
        })
        .await;
    for m in &call.rec.messages {
        assert!(m.len() <= 65_536, "a message of {} bytes", m.len());
    }
    let emptied = [
        ("response.output_text.delta", "delta"),
        ("response.output_text.done", "text"),
        ("response.content_part.done", "part.text"),
        ("response.output_item.done", "item.content[0].text"),
        ("conversation.item.done", "item.content[0].text"),
        ("response.done", "response.output[0].content[0].text"),
    ];
    for (kind, path) in emptied {
        // The answer's, not the user's item.
        let at = events
            .iter()
            .position(|e| e["type"] == kind && e["item"]["role"] != "user")
            .unwrap_or_else(|| panic!("no {kind} in {:?}", types(&events)));
        let e = &events[at];
        let field = path
            .split(['.', '['])
            .fold(e, |v, step| match step.strip_suffix(']') {
                Some(i) => &v[i.parse::<usize>().unwrap()],
                None => &v[step],
            });
        let marker = field.as_str().unwrap_or_else(|| panic!("{kind}: {e}"));
        assert!(
            marker.starts_with("[omitted: 100 KiB over this call's 64 KiB event limit]"),
            "{kind}: {marker}"
        );
        let error = &events[at + 1];
        assert_eq!(error["type"], "error", "{kind} is followed by its error");
        assert_eq!(error["error"]["code"], "event_too_large");
        assert_eq!(error["error"]["param"], path);
        let message = error["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with(&format!("{kind} {}", e["event_id"].as_str().unwrap()))
                && message.contains("100 KiB")
                && message.contains("the client's max-message-size of 65536 bytes"),
            "{message}"
        );
    }
    let done = events
        .iter()
        .find(|e| e["type"] == "response.done")
        .unwrap();
    assert!(done["response"]["id"]
        .as_str()
        .unwrap()
        .starts_with("resp_"));
    assert!(done["response"]["output"][0]["id"]
        .as_str()
        .unwrap()
        .starts_with("item_"));
    assert_eq!(done["response"]["status"], "completed");
}

/// A part from a stock client is answered `invalid_event` naming its set,
/// once: the set is dropped.
#[tokio::test]
async fn a_part_on_an_unbound_call_is_refused_once_per_set() {
    let w = calls_world(|_| {}).await;
    let mut call = unbound(&w).await;
    for i in 0..3 {
        call.send(
            json!({"type": "lmgw.event.part", "event_id": format!("p{i}"),
                         "id": "mine", "index": i, "count": 3, "data": "{}"}),
        );
    }
    let after = call.probe().await;
    assert_eq!(types(&after), ["error", "session.updated"], "{after:?}");
    let e = &after[0]["error"];
    assert_eq!(e["code"], "invalid_event");
    assert_eq!(e["param"], "type");
    assert_eq!(e["event_id"], "p0");
    let message = e["message"].as_str().unwrap();
    assert!(
        message.contains("mine") && message.contains("a call bound to a Chat thread"),
        "{message}"
    );
}
