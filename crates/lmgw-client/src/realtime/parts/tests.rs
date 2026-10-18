use serde_json::{json, Value};

use super::*;
use crate::realtime::{parse, ServerEvent};

/// The text of part `index` of `count` in set `id`.
fn part(id: &str, index: u32, count: u32, data: &str) -> String {
    json!({"type": "lmgw.event.part", "event_id": "evt_p", "id": id, "index": index,
           "count": count, "data": data})
    .to_string()
}

/// Every message through `j`, the whole events and the errors in order.
fn feed(j: &mut Joiner, messages: Vec<String>) -> (Vec<String>, Vec<PartError>) {
    let (mut events, mut broken) = (Vec::new(), Vec::new());
    for m in messages {
        let out = j.push(m);
        broken.extend(out.broken);
        events.extend(out.event);
    }
    (events, broken)
}

fn err(id: &str, fault: PartFault) -> PartError {
    PartError {
        id: id.into(),
        fault,
    }
}

/// An event's text with every kind of character the escaping treats apart.
fn awkward_event(len: usize) -> String {
    let words = "quote \" backslash \\ tab \t newline \n bell \u{7} nul \0 \u{1f} umlaut ä \
                 euro € emoji 🎙️ cjk 語 slash / del \u{7f} ";
    let mut text = String::new();
    while text.len() < len {
        text.push_str(words);
    }
    json!({"type": "conversation.item.done", "event_id": "evt_42",
           "item": {"type": "function_call_output", "output": text}})
    .to_string()
}

#[test]
fn a_message_that_is_no_part_passes_as_it_is() {
    let mut j = Joiner::new();
    for m in [
        r#"{"type":"response.created","response":{"id":"r"}}"#,
        "not json",
        r#"["lmgw.event.part", "a", 0, 2, "x"]"#,
        r#"{"event_id":"e"}"#,
        "",
    ] {
        assert_eq!(
            j.push(m.to_string()),
            Joined {
                broken: vec![],
                event: Some(m.to_string())
            },
            "{m}"
        );
    }
}

#[test]
fn a_split_event_joins_to_itself_and_parses_as_one() {
    for (len, max) in [
        (5_000, 256),
        (40_000, 1_000),
        (300_000, 262_144),
        (3_000, 80),
    ] {
        let event = awkward_event(len);
        let parts = split(&event, "evt_42", max).unwrap();
        assert!(parts.len() >= 2, "{len} at {max}");
        let mut j = Joiner::new();
        let mut joined = None;
        for (i, p) in parts.iter().enumerate() {
            assert!(p.len() as u64 <= max, "part {i}: {} > {max}", p.len());
            let v: Value = serde_json::from_str(p).unwrap();
            assert_eq!(
                (&v["type"], &v["id"], &v["index"], &v["count"]),
                (
                    &json!("lmgw.event.part"),
                    &json!("evt_42"),
                    &json!(i),
                    &json!(parts.len())
                )
            );
            let out = j.push(p.clone());
            assert!(out.broken.is_empty(), "{:?}", out.broken);
            if i + 1 < parts.len() {
                assert_eq!(out.event, None, "held until whole");
            } else {
                joined = out.event;
            }
        }
        assert_eq!(joined.as_deref(), Some(event.as_str()), "{len} at {max}");
    }
    // What the joined text is: the event, parsed as one.
    let created = json!({"type": "session.created", "event_id": "evt_1", "session": {
        "type": "realtime", "instructions": "x".repeat(10_000),
        "lmgw": {"turn_mode": "push_to_talk"}}})
    .to_string();
    let mut j = Joiner::new();
    let (events, broken) = feed(&mut j, split(&created, "evt_1", 1_024).unwrap());
    assert!(broken.is_empty());
    let Some(ServerEvent::SessionCreated(f)) = parse(&events[0]) else {
        panic!("a session")
    };
    assert_eq!(f.turn_mode, Some(crate::realtime::TurnMode::PushToTalk));
}

#[test]
fn each_part_is_filled_up_to_the_limit() {
    // Text that needs no escape: the fewest parts the limit allows.
    let event = "a".repeat(9_000);
    let max = 500;
    let parts = split(&event, "e1", max).unwrap();
    let digits = decimal_digits(parts.len() as u64);
    let room = max - (PART_FRAME.len() as u64 + r#""e1""#.len() as u64 + 2 * digits);
    assert_eq!(parts.len() as u64, (event.len() as u64).div_ceil(room));
    // Past ten and a hundred parts the numbers take more room.
    for n in [12_000, 120_000] {
        let event = format!(r#"{{"data":"{}"}}"#, "b".repeat(n));
        let parts = split(&event, "e", 200).unwrap();
        assert!(parts.len() > if n > 100_000 { 100 } else { 10 });
        assert!(parts.iter().all(|p| p.len() <= 200));
        let (events, _) = feed(&mut Joiner::new(), parts);
        assert_eq!(events, [event]);
    }
}

#[test]
fn an_event_that_fits_is_one_message() {
    let event = r#"{"type":"lmgw.response.stop"}"#;
    assert_eq!(split(event, "e", 64).unwrap(), [event]);
    assert_eq!(
        split(event, "e", event.len() as u64).unwrap(),
        [event],
        "exactly at it"
    );
    assert_eq!(
        split(&awkward_event(100_000), "e", 0).unwrap().len(),
        1,
        "0: no limit"
    );
    // A larger one takes the splitter's next id; a fitting one takes none.
    let mut s = Splitter::new(256);
    assert_eq!(s.split(event).unwrap(), [event]);
    let big = awkward_event(1_000);
    let first = s.split(&big).unwrap();
    let second = s.split(&big).unwrap();
    assert!(first.iter().all(|p| p.contains(r#""id":"part_1""#)));
    assert!(second.iter().all(|p| p.contains(r#""id":"part_2""#)));
    assert_eq!(s.max_message_size(), 256);
}

#[test]
fn a_limit_with_no_room_for_data_is_an_error_naming_it() {
    let event = format!(r#"{{"data":"{}"}}"#, "a".repeat(200));
    let frame = (PART_FRAME.len() + r#""evt_1""#.len()) as u64;
    let e = split(&event, "evt_1", frame + 2).unwrap_err();
    assert_eq!(
        e,
        SplitError::LimitTooSmall {
            max_message_size: frame + 2,
            needed: frame + 3
        }
    );
    assert!(e.to_string().contains("max-message-size of"), "{e}");
    // One byte of room: no room for an escaped quote, and once the parts
    // number more than nine, none at all.
    for event in [event.as_str(), &"a".repeat(200)] {
        let e = split(event, "evt_1", frame + 3).unwrap_err();
        assert!(matches!(e, SplitError::LimitTooSmall { .. }), "{e}");
    }
    let parts = split(&event, "evt_1", frame + 16).unwrap();
    assert_eq!(feed(&mut Joiner::new(), parts).0, [event]);
    // A character whose escape is wider than the room.
    let e = split(&"\u{1}".repeat(100), "evt_1", frame + 2 + 5).unwrap_err();
    assert_eq!(
        e,
        SplitError::LimitTooSmall {
            max_message_size: frame + 7,
            needed: frame + 8
        }
    );
}

#[test]
fn the_escaped_widths_are_serde_json_s() {
    for c in (0u32..0x80)
        .filter_map(char::from_u32)
        .chain(['ä', '€', '語', '🎙', '\u{2028}', '\u{fe0f}'])
    {
        let written = serde_json::to_string(&c.to_string()).unwrap().len() - 2;
        assert_eq!(escaped_len(c), written as u64, "{c:?}");
    }
}

#[test]
fn a_set_broken_by_another_message_is_an_error_naming_its_id() {
    let mut j = Joiner::new();
    let other = r#"{"type":"response.output_audio_transcript.delta","delta":"Hi"}"#.to_string();
    let (events, broken) = feed(
        &mut j,
        vec![
            part("evt_7", 0, 3, "{\"type\":"),
            other.clone(),
            // The rest of the broken set goes without another word.
            part("evt_7", 1, 3, "\"x\""),
            part("evt_7", 2, 3, "}"),
        ],
    );
    assert_eq!(broken, [err("evt_7", PartFault::Interrupted)]);
    assert_eq!(events, [other], "the message that broke it is whole");
    assert!(broken[0].to_string().contains("evt_7"), "{}", broken[0]);
    // A new set joins after it.
    let (events, broken) = feed(
        &mut j,
        vec![
            part("evt_8", 0, 2, "{\"type\":"),
            part("evt_8", 1, 2, "\"y\"}"),
        ],
    );
    assert!(broken.is_empty());
    assert_eq!(events, [r#"{"type":"y"}"#]);
}

#[test]
fn a_part_of_another_set_breaks_the_open_one() {
    let mut j = Joiner::new();
    // Another set's first part breaks this one and opens its own.
    let (events, broken) = feed(
        &mut j,
        vec![
            part("a", 0, 2, "{"),
            part("b", 0, 2, "{\"type\":"),
            part("b", 1, 2, "\"z\"}"),
        ],
    );
    assert_eq!(broken, [err("a", PartFault::Interrupted)]);
    assert_eq!(events, [r#"{"type":"z"}"#]);
    // Another set's middle part: both broken.
    let (events, broken) = feed(&mut j, vec![part("c", 0, 3, "{"), part("d", 1, 2, "}")]);
    assert_eq!(
        broken,
        [
            err("c", PartFault::Interrupted),
            err(
                "d",
                PartFault::OutOfOrder {
                    expected: 0,
                    got: 1
                }
            )
        ]
    );
    assert!(events.is_empty());
}

#[test]
fn interleaved_sets_are_each_said_once() {
    let mut j = Joiner::new();
    let (events, broken) = feed(
        &mut j,
        vec![
            part("a", 0, 2, "{"),
            part("b", 0, 2, "{"),
            part("a", 1, 2, "}"),
            part("b", 1, 2, "}"),
            // A broken set's part later on, even one that looks like a
            // start, goes without a word.
            r#"{"type":"response.created"}"#.to_string(),
            part("a", 0, 2, "{"),
        ],
    );
    assert_eq!(
        broken,
        [
            err("a", PartFault::Interrupted),
            err("b", PartFault::Interrupted)
        ]
    );
    assert_eq!(events, [r#"{"type":"response.created"}"#]);
}

#[test]
fn a_wrong_index_or_count_breaks_the_set() {
    let mut j = Joiner::new();
    let (events, broken) = feed(
        &mut j,
        vec![
            part("a", 0, 3, "{"),
            part("a", 2, 3, "}"),
            part("b", 0, 3, "{"),
            part("b", 1, 4, "\"x\""),
            part("b", 2, 3, "}"),
            // A set that starts in its middle.
            part("c", 1, 2, "}"),
        ],
    );
    assert!(events.is_empty());
    assert_eq!(
        broken,
        [
            err(
                "a",
                PartFault::OutOfOrder {
                    expected: 1,
                    got: 2
                }
            ),
            err("b", PartFault::CountChanged { count: 3, got: 4 }),
            err(
                "c",
                PartFault::OutOfOrder {
                    expected: 0,
                    got: 1
                }
            ),
        ]
    );
    // A repeated part is out of order too.
    let (_, broken) = feed(&mut j, vec![part("d", 0, 2, "{"), part("d", 0, 2, "{")]);
    assert_eq!(
        broken,
        [err(
            "d",
            PartFault::OutOfOrder {
                expected: 1,
                got: 0
            }
        )]
    );
}

#[test]
fn a_part_that_does_not_read_is_an_error_naming_its_id() {
    for (text, id) in [
        (part("a", 0, 1, "{}"), "a"),
        (part("b", 2, 2, "{}"), "b"),
        (
            json!({"type": "lmgw.event.part", "id": "c", "index": 0, "count": 2}).to_string(),
            "c",
        ),
        (
            json!({"type": "lmgw.event.part", "index": 0, "count": 2, "data": "{"}).to_string(),
            "",
        ),
        (
            json!({"type": "lmgw.event.part", "id": "d", "index": -1, "count": 2, "data": "{"})
                .to_string(),
            "d",
        ),
    ] {
        let out = Joiner::new().push(text.clone());
        assert_eq!(out.event, None, "{text}");
        assert_eq!(out.broken.len(), 1, "{text}");
        assert_eq!(out.broken[0].id, id, "{text}");
        assert!(
            matches!(out.broken[0].fault, PartFault::Unreadable { .. }),
            "{text}"
        );
    }
    // Inside a set, an unreadable part of it breaks it.
    let bad = json!({"type": "lmgw.event.part", "id": "e", "index": "1", "count": 2, "data": "}"});
    let (events, broken) = feed(
        &mut Joiner::new(),
        vec![part("e", 0, 2, "{"), bad.to_string()],
    );
    assert!(events.is_empty());
    assert_eq!(broken.len(), 1);
    assert_eq!(broken[0].id, "e");
    assert!(broken[0].to_string().contains("does not read"));
}

#[test]
fn a_bound_joiner_refuses_a_set_past_it_naming_the_setting() {
    let event = format!(r#"{{"data":"{}"}}"#, "a".repeat(5_000));
    let parts = split(&event, "evt_1", 1_000).unwrap();
    let mut j = Joiner::bounded(4_096, "voice.max_event_kib");
    let (events, broken) = feed(&mut j, parts.clone());
    assert!(events.is_empty());
    let fault = PartFault::TooLarge {
        limit: 4_096,
        name: "voice.max_event_kib".into(),
    };
    assert_eq!(broken, [err("evt_1", fault)]);
    assert!(
        broken[0].to_string().contains("voice.max_event_kib"),
        "{}",
        broken[0]
    );
    // At the bound exactly, it joins.
    let mut j = Joiner::bounded(event.len() as u64, "voice.max_event_kib");
    assert_eq!(feed(&mut j, parts.clone()).0, std::slice::from_ref(&event));
    // 0 is no bound, as a max-message-size of 0 is none.
    let mut j = Joiner::bounded(0, "voice.max_event_kib");
    assert_eq!(feed(&mut j, parts), (vec![event], vec![]));
}

#[test]
fn parts_that_join_to_a_part_are_refused() {
    let inner = part("x", 0, 2, &"{".repeat(200));
    let (events, broken) = feed(&mut Joiner::new(), split(&inner, "outer", 120).unwrap());
    assert!(events.is_empty());
    assert_eq!(broken, [err("outer", PartFault::Nested)]);
}

#[test]
fn the_limit_is_read_from_the_sdp() {
    let answer = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n\
                  m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                  a=sctp-port:5000\r\na=max-message-size:262144\r\n";
    assert_eq!(max_message_size(answer), 262_144);
    assert_eq!(
        max_message_size("v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n"),
        DEFAULT_MAX_MESSAGE_SIZE,
        "RFC 8841's default"
    );
    assert_eq!(max_message_size("a=max-message-size:0\r\n"), 0);
    assert_eq!(
        max_message_size("a=max-message-size:lots\r\n"),
        DEFAULT_MAX_MESSAGE_SIZE
    );
}
