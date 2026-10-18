//! The size bounds' rules on their own (calls design §6.3): lmgw's parts
//! against lmgw-client's joiner and the phone's splitter against lmgw's
//! joiner, so each side reads the other's contract; the emptied fields of
//! an unbound call's event; a set's refusals.

use std::sync::Arc;

use lmgw_api_types::realtime::EVENT_PART;
use serde_json::{json, Value};

use super::empty::{fit, Emptied};
use super::*;

fn shaper(max: usize, bound: bool) -> Shaper {
    Shaper::new(
        "rtc_test".into(),
        (max, Limit::Client),
        bound,
        Arc::new(Ids::new()),
    )
}

/// An event of about `n` bytes whose text needs escaping everywhere: quotes,
/// backslashes, newlines, a control character and multibyte characters.
fn awkward(n: usize) -> String {
    let unit = "say \"hi\" \\ then\nsome ä€😀\u{1} ";
    let delta: String = unit.chars().cycle().take(n).collect();
    json!({"event_id": "event_abc1", "type": "lmgw.chat.frame", "response_id": "resp_1",
           "event": "delta", "data": {"text": delta}})
    .to_string()
}

#[test]
fn a_bound_calls_oversized_event_goes_as_parts_lmgw_clients_joiner_reads_whole() {
    let event = awkward(100_000);
    for max in [1_000, 4_096, 65_536] {
        let parts = shaper(max, true).shape(event.clone());
        assert!(parts.len() >= 2, "{max}: {} parts", parts.len());
        let mut j = lmgw_client::realtime::Joiner::new();
        let mut joined = None;
        for (i, p) in parts.iter().enumerate() {
            assert!(p.len() <= max, "part {i} of {max}: {} bytes", p.len());
            let v: Value = serde_json::from_str(p).unwrap();
            assert_eq!(v["type"], EVENT_PART);
            assert_eq!(v["id"], "event_abc1", "the event's own id");
            assert_eq!(v["index"], i);
            assert_eq!(v["count"], parts.len());
            assert!(
                v["event_id"].as_str().unwrap().starts_with("event_"),
                "each part an event of its own"
            );
            let out = j.push(p.clone());
            assert!(out.broken.is_empty(), "{:?}", out.broken);
            joined = out.event.or(joined);
        }
        assert_eq!(joined.as_deref(), Some(event.as_str()), "whole at {max}");
    }
    // One that fits goes as it is, on either call.
    let small = awkward(100);
    assert_eq!(
        shaper(65_536, true).shape(small.clone()),
        std::slice::from_ref(&small)
    );
    assert_eq!(shaper(65_536, false).shape(small.clone()), [small]);
}

#[test]
fn a_limit_too_small_for_any_part_sends_nothing() {
    assert!(shaper(60, true).shape(awkward(1_000)).is_empty());
}

#[test]
fn the_phones_parts_from_lmgw_clients_splitter_join_on_lmgw_and_go_on_as_one_event() {
    let event = json!({"type": "conversation.item.create", "item": {"type": "message",
        "role": "user", "content": [{"type": "input_text", "text": awkward(30_000)}]}})
    .to_string();
    let mut s = lmgw_client::realtime::Splitter::new(2_048);
    let parts = s.split(&event).unwrap();
    assert!(parts.len() > 10);
    let mut j = Joiner::new(true, Some(1 << 20));
    let mut got = Vec::new();
    for p in parts {
        got.extend(j.push(p));
    }
    assert_eq!(got, [Joined::Event(event.clone())]);
    // A message that is no part passes as it is.
    assert_eq!(
        j.push("{\"type\":\"x\"}".into()),
        [Joined::Event("{\"type\":\"x\"}".into())]
    );
}

/// The text of the phone's part `index` of `count` in set `id`.
fn part(id: &str, index: u32, count: u32, data: &str) -> String {
    json!({"type": EVENT_PART, "event_id": format!("c{index}"), "id": id, "index": index,
           "count": count, "data": data})
    .to_string()
}

fn refused(j: &Joined) -> &ErrorObject {
    match j {
        Joined::Refused(e) => e,
        Joined::Event(t) => panic!("an event: {t}"),
    }
}

#[test]
fn a_broken_set_is_refused_once_naming_its_id_and_the_message_that_broke_it_goes_on() {
    let mut j = Joiner::new(true, None);
    assert!(j.push(part("p1", 0, 3, "{\"type\"")).is_empty());
    let out = j.push("{\"type\":\"response.cancel\"}".into());
    let e = refused(&out[0]);
    assert_eq!(e.code.as_deref(), Some("invalid_event"));
    assert!(
        e.message.contains("p1") && e.message.contains("between"),
        "{}",
        e.message
    );
    assert_eq!(
        out[1],
        Joined::Event("{\"type\":\"response.cancel\"}".into())
    );
    // Its remaining parts go unsaid.
    assert!(j.push(part("p1", 1, 3, ":")).is_empty());
    assert!(j.push(part("p1", 2, 3, "1}")).is_empty());

    // A wrong index, a changed count, a part that does not read.
    let e = j.push(part("p2", 1, 2, "x"));
    assert!(refused(&e[0])
        .message
        .contains("part 1 came where part 0 was due"));
    assert_eq!(refused(&e[0]).event_id.as_deref(), Some("c1"));
    j.push(part("p3", 0, 2, "x"));
    let e = j.push(part("p3", 1, 3, "x"));
    assert!(refused(&e[0])
        .message
        .contains("a part says 3 parts, the set 2"));
    let e = j.push(json!({"type": EVENT_PART, "id": "p4", "index": "one"}).to_string());
    assert!(refused(&e[0]).message.contains("p4"), "{:?}", e[0]);
    let e = j.push(part("p5", 0, 1, "x"));
    assert!(refused(&e[0]).message.contains("at least 2"));
    // A set that joins to a part.
    let inner = part("p7", 0, 2, "x");
    let (a, b) = inner.split_at(inner.len() / 2);
    j.push(part("p6", 0, 2, a));
    let e = j.push(part("p6", 1, 2, b));
    assert!(refused(&e[0]).message.contains("another lmgw.event.part"));
}

#[test]
fn a_retry_with_the_broken_sets_id_opens_a_new_set() {
    let mut j = Joiner::new(true, None);
    j.push(part("e1", 0, 3, "{\"type\""));
    let out = j.push("{\"type\":\"response.cancel\"}".into());
    assert_eq!(out.len(), 2, "the break is answered, the message goes on");
    // The rest of the broken set goes unsaid, a part 0 is a retry.
    assert!(j.push(part("e1", 1, 3, ":")).is_empty());
    assert!(j.push(part("e1", 0, 2, "{\"type\":")).is_empty());
    let out = j.push(part("e1", 1, 2, "\"x\"}"));
    assert_eq!(out, [Joined::Event("{\"type\":\"x\"}".into())]);
}

#[test]
fn a_set_joined_past_max_message_mb_is_refused_naming_the_setting() {
    let mut j = Joiner::new(true, Some(10));
    assert!(j.push(part("big", 0, 2, "123456")).is_empty());
    let out = j.push(part("big", 1, 2, "78901"));
    let e = refused(&out[0]);
    assert_eq!(e.code.as_deref(), Some("event_too_large"));
    assert!(
        e.message.contains("realtime.max_message_mb (10 bytes)"),
        "{}",
        e.message
    );
    // At the bound exactly it joins.
    let mut j = Joiner::new(true, Some(10));
    j.push(part("ok", 0, 2, "{\"a\""));
    assert_eq!(
        j.push(part("ok", 1, 2, ":12}")),
        [Joined::Event("{\"a\":12}".into())]
    );
}

#[test]
fn a_part_on_an_unbound_call_is_refused_once_per_set() {
    let mut j = Joiner::new(false, None);
    let out = j.push(part("u1", 0, 3, "{"));
    let e = refused(&out[0]);
    assert_eq!(
        (e.code.as_deref(), e.param.as_deref()),
        (Some("invalid_event"), Some("type"))
    );
    assert!(e.message.contains("u1") && e.message.contains("bound to a Chat thread"));
    assert!(j.push(part("u1", 1, 3, "}")).is_empty());
    assert!(j.push(part("u1", 2, 3, "")).is_empty());
    assert_eq!(
        j.push(part("u2", 0, 2, "{")).len(),
        1,
        "another set, another answer"
    );
}

/// `event` emptied to fit `max`: the event, and the fields emptied.
fn fitted(event: &Value, max: usize) -> (Value, Vec<Emptied>) {
    let f = fit(&event.to_string(), max, "4 KiB").unwrap();
    assert!(f.text.len() <= max, "{} over {max}", f.text.len());
    (serde_json::from_str(&f.text).unwrap(), f.emptied)
}

#[test]
fn an_unbound_calls_event_loses_its_largest_field_first_and_never_an_id() {
    let long = "x".repeat(6_000);
    let event = json!({"event_id": "event_1", "type": "session.updated", "session": {
    "id": "sess_1", "instructions": long, "tools": [
        {"type": "function", "name": "a", "description": "y".repeat(500)}
    ]}});
    let (v, emptied) = fitted(&event, 4_096);
    let marker = "[omitted: 6 KiB over this call's 4 KiB event limit]";
    assert_eq!(v["session"]["instructions"], marker);
    assert_eq!(v["session"]["id"], "sess_1");
    assert_eq!(v["type"], "session.updated");
    assert_eq!(v["event_id"], "event_1");
    assert_eq!(
        v["session"]["tools"][0]["name"], "a",
        "the smaller field stays"
    );
    assert_eq!(
        emptied,
        [Emptied {
            path: "session.instructions".into(),
            size: 6_002
        }]
    );
}

#[test]
fn a_list_too_large_as_a_whole_goes_as_an_empty_list_not_item_by_item() {
    let tools: Vec<Value> = (0..200)
        .map(
            |i| json!({"type": "function", "name": format!("t{i}"), "description": "d".repeat(60)}),
        )
        .collect();
    let event = json!({"event_id": "event_2", "type": "session.created",
        "session": {"id": "sess_2", "instructions": "short", "tools": tools}});
    let (v, emptied) = fitted(&event, 4_096);
    assert_eq!(v["session"]["tools"], json!([]));
    assert_eq!(v["session"]["instructions"], "short");
    assert_eq!(emptied.len(), 1);
    assert_eq!(emptied[0].path, "session.tools");
}

#[test]
fn schema_properties_named_id_do_not_protect_a_tools_list() {
    let tools: Vec<Value> = (0..200)
        .map(|i| {
            json!({"type": "function", "name": format!("t{i}"), "description": "d".repeat(60),
                "input_schema": {"type": "object", "properties": {
                    "id": {"type": "string"}, "thread_id": {"type": "string"}}}})
        })
        .collect();
    let event = json!({"event_id": "event_4", "type": "session.created",
        "session": {"id": "sess_4", "tools": tools}});
    let (v, emptied) = fitted(&event, 4_096);
    assert_eq!(v["session"]["tools"], json!([]));
    assert_eq!(v["session"]["id"], "sess_4");
    assert_eq!(emptied.len(), 1, "{emptied:?}");
}

#[test]
fn a_string_is_emptied_once() {
    // Two strings of 1,010 bytes: emptying one leaves a marker of about 50
    // bytes, which must not be marked again.
    let event = json!({"event_id": "event_5", "type": "x",
        "a": "a".repeat(1_008), "b": "b".repeat(1_008)});
    let (_, emptied) = fitted(&event, 1_100);
    let mut paths: Vec<_> = emptied.iter().map(|e| e.path.clone()).collect();
    let n = paths.len();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), n, "{emptied:?}");
}

#[test]
fn the_narrowest_field_that_suffices_goes_and_items_keep_their_ids() {
    // response.done: every item carries an id, so neither the output nor an
    // item is emptied whole; the long text inside goes.
    let event = json!({"event_id": "event_3", "type": "response.done", "response": {
        "id": "resp_3", "status": "completed", "output": [
            {"id": "item_1", "type": "message", "content": [
                {"type": "output_text", "text": "t".repeat(9_000)}]}]}});
    let (v, emptied) = fitted(&event, 4_096);
    let out = &v["response"]["output"][0];
    assert_eq!(out["id"], "item_1");
    assert_eq!(v["response"]["id"], "resp_3");
    assert_eq!(out["content"][0]["type"], "output_text");
    assert!(out["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("[omitted: 9 KiB"));
    assert_eq!(emptied[0].path, "response.output[0].content[0].text");

    // Two fields, neither alone enough: the largest first, then the other.
    let event = json!({"event_id": "event_4", "type": "conversation.item.done",
        "item": {"id": "item_2", "type": "function_call_output", "call_id": "call_1",
                 "output": "o".repeat(3_000), "note": "n".repeat(2_500)}});
    let (v, emptied) = fitted(&event, 1_000);
    let paths: Vec<_> = emptied.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["item.output", "item.note"]);
    assert_eq!(v["item"]["call_id"], "call_1");
}

#[test]
fn an_event_whose_ids_alone_are_over_the_limit_cannot_be_made_to_fit() {
    let event = json!({"event_id": "event_5", "type": "x", "response_id": "r".repeat(5_000)});
    assert!(fit(&event.to_string(), 1_000, "1000 bytes").is_err());
    // The shaper sends nothing then.
    assert!(shaper(1_000, false).shape(event.to_string()).is_empty());
}

#[test]
fn the_shaper_sends_the_emptied_event_then_one_event_too_large_per_field() {
    let event = json!({"event_id": "event_6", "type": "response.output_text.done",
        "response_id": "resp_6", "item_id": "item_6", "output_index": 0, "content_index": 0,
        "text": "w".repeat(20_000)})
    .to_string();
    let out = shaper(8_192, false).shape(event);
    assert_eq!(out.len(), 2, "{out:?}");
    let ev: Value = serde_json::from_str(&out[0]).unwrap();
    assert_eq!(ev["type"], "response.output_text.done");
    assert_eq!(ev["item_id"], "item_6");
    let err: Value = serde_json::from_str(&out[1]).unwrap();
    assert_eq!(err["type"], "error");
    assert_eq!(err["error"]["code"], "event_too_large");
    assert_eq!(err["error"]["param"], "text");
    let message = err["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("response.output_text.done event_6")
            && message.contains("20 KiB")
            && message.contains("the client's max-message-size of 8192 bytes"),
        "{message}"
    );
    assert!(err["event_id"].as_str().unwrap().starts_with("event_"));
}

#[test]
fn sizes_read_as_a_person_reads_them() {
    assert_eq!(human(512), "512 bytes");
    assert_eq!(human(262_144), "256 KiB");
    assert_eq!(human(319_488), "312 KiB");
    assert_eq!(human(3 << 20), "3.0 MiB");
}
