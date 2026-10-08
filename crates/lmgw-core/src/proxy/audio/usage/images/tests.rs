use serde_json::json;

use super::*;

/// What [`DataCount`] reads of `body` fed whole, byte by byte, and split at
/// every position — which must all agree.
fn count(body: &str) -> Option<u64> {
    let b = body.as_bytes();
    let mut whole = DataCount::default();
    whole.feed(b);
    let got = whole.images();
    let mut bytes = DataCount::default();
    for c in b.chunks(1) {
        bytes.feed(c);
    }
    assert_eq!(bytes.images(), got, "byte by byte: {body}");
    for at in 0..=b.len() {
        let mut split = DataCount::default();
        split.feed(&b[..at]);
        split.feed(&b[at..]);
        assert_eq!(split.images(), got, "split at {at}: {body}");
    }
    got
}

#[test]
fn the_top_level_data_array_is_counted() {
    let two = json!({"created": 1, "background": "opaque",
        "data": [{"b64_json": "aVZCT1J3MEs="}, {"b64_json": "QUJD", "revised_prompt": "a cat"}],
        "usage": {"total_tokens": 100}});
    assert_eq!(count(&two.to_string()), Some(2));
    assert_eq!(count(&serde_json::to_string_pretty(&two).unwrap()), Some(2));
    assert_eq!(count(r#"{"data":[{"b64_json":"x"}]}"#), Some(1));
    assert_eq!(count(r#"{"data": ["a", 2, null, true]}"#), Some(4));
}

#[test]
fn an_empty_data_is_a_measured_zero() {
    assert_eq!(count(r#"{"created":1,"data":[]}"#), Some(0));
    assert_eq!(count(r#"{"data":[ ] }"#), Some(0));
}

#[test]
fn data_inside_a_string_is_not_data() {
    assert_eq!(
        count(r#"{"revised_prompt":"\"data\": [1, 2, 3]","data":[{"b64_json":"x"}]}"#),
        Some(1)
    );
    assert_eq!(count(r#"{"note":"data","x":[1, 2]}"#), None, "a value");
    assert_eq!(count(r#"{"data_url":[1],"dat":[1],"x":"data"}"#), None);
}

#[test]
fn escaped_quotes_and_backslashes_stay_inside_their_string() {
    let body = r#"{"revised_prompt":"say \"hi\", \\\"]}, \\","data":[{"b64_json":"a\"]}"},{}]}"#;
    assert_eq!(count(body), Some(2));
}

#[test]
fn nested_arrays_and_objects_are_one_element_each() {
    assert_eq!(
        count(r#"{"data":[[1,2],[3],{"a":[4,5],"data":[6]}]}"#),
        Some(3)
    );
}

#[test]
fn only_the_top_level_data_counts() {
    assert_eq!(count(r#"{"result":{"data":[1,2]}}"#), None);
    assert_eq!(
        count(r#"[{"data":[1]}]"#),
        None,
        "the top level is no object"
    );
    assert_eq!(count(r#"{"data":null}"#), None, "no array");
    assert_eq!(count(r#"{"data":{"0":{}}}"#), None, "no array");
    assert_eq!(count(r#"{"created":1}"#), None, "no data");
}

#[test]
fn a_document_that_ends_early_is_unknown() {
    assert_eq!(count(r#"{"data":[{"b64_json":"aVZC"#), None);
    assert_eq!(count(r#"{"data":[{},{}]"#), None, "the object never closed");
    assert_eq!(count(""), None);
}

#[test]
fn what_follows_the_document_is_whitespace_or_nothing() {
    assert_eq!(count("{\"data\":[{}]}\n\r\n"), Some(1));
    assert_eq!(count(r#"{"data":[{}]}{"data":[]}"#), None);
    assert_eq!(count("not json"), None);
}

#[test]
fn an_escaped_key_is_not_read_as_data() {
    assert_eq!(count(r#"{"d\u0061ta":[{}]}"#), None);
}

#[test]
fn the_last_data_wins_as_in_a_parser() {
    assert_eq!(count(r#"{"data":[1,2,3],"data":[4]}"#), Some(1));
    assert_eq!(count(r#"{"data":[1],"data":null}"#), None);
}

// --- Event streams -----------------------------------------------------------

/// OpenAI's final-image events, pinned: anything else in a stream, the
/// partial images included, is never an image.
#[test]
fn the_final_image_events_are_openais() {
    assert_eq!(
        FINAL_IMAGE_EVENTS,
        ["image_generation.completed", "image_edit.completed"]
    );
}

fn event(kind: &str) -> String {
    let data = json!({"type": kind, "b64_json": "aVZCT1J3MEs=", "created_at": 1,
        "partial_image_index": 0, "usage": {"total_tokens": 100}});
    format!("event: {kind}\ndata: {data}\n\n")
}

/// What [`FinalEvents`] counts in `stream` fed whole, in 7-byte chunks,
/// byte by byte, and split at every position — which must all agree.
fn stream_count(stream: &str, whole: bool) -> Option<u64> {
    let b = stream.as_bytes();
    let mut once = FinalEvents::default();
    once.feed(b);
    let got = once.images(whole);
    for size in [7, 1] {
        let mut chunked = FinalEvents::default();
        for c in b.chunks(size) {
            chunked.feed(c);
        }
        assert_eq!(chunked.images(whole), got, "{size}-byte chunks: {stream:?}");
    }
    for at in 0..=b.len() {
        let mut split = FinalEvents::default();
        split.feed(&b[..at]);
        split.feed(&b[at..]);
        assert_eq!(split.images(whole), got, "split at {at}: {stream:?}");
    }
    got
}

#[test]
fn final_events_count_and_partial_ones_do_not() {
    let gen = [
        event("image_generation.partial_image"),
        event("image_generation.partial_image"),
        event("image_generation.completed"),
    ]
    .concat();
    assert_eq!(stream_count(&gen, true), Some(1));
    let edits = [
        event("image_edit.partial_image"),
        event("image_edit.completed"),
        event("image_edit.completed"),
    ]
    .concat();
    assert_eq!(stream_count(&edits, true), Some(2));
}

#[test]
fn a_stream_without_a_recognised_final_event_is_unknown() {
    let partial = [
        event("image_generation.partial_image"),
        event("image_generation.partial_image"),
    ]
    .concat();
    assert_eq!(stream_count(&partial, true), None);
    assert_eq!(stream_count(&event("image_generation.done"), true), None);
    assert_eq!(stream_count("", true), None);
}

#[test]
fn a_stream_that_broke_off_is_unknown() {
    let one = event("image_generation.completed");
    assert_eq!(stream_count(&one, false), None);
}

#[test]
fn the_event_name_stands_in_for_a_missing_type() {
    let named = "event: image_generation.completed\ndata: {\"b64_json\":\"x\"}\n\n";
    assert_eq!(stream_count(named, true), Some(1));
    let unterminated = format!("data: {}", json!({"type": "image_edit.completed"}));
    assert_eq!(stream_count(&unterminated, true), Some(1));
}

/// The stream is read as `crate::sse::SseDecoder` reads one.
#[test]
fn lines_and_fields_are_read_as_the_sse_decoder_reads_them() {
    let done = json!({"type": "image_edit.completed", "b64_json": "QQ=="});
    for stream in [
        format!("data: {done}\r\n\r\n"),
        format!("data: {done}\r\r"),
        format!("data:{done}\n\n"),
        format!(": keep-alive\nid: 7\nretry: 10\ndata: {done}\n\n"),
        format!("\n\ndata: {done}\n\n\n\n"),
        "event:   image_edit.completed\r\ndata: {}\r\n\r\n".to_string(),
        "data: {\"type\":\ndata: \"image_generation.completed\"}\n\n".to_string(),
    ] {
        assert_eq!(stream_count(&stream, true), Some(1), "{stream:?}");
    }
    // A field is a name and its colon at the start of a line.
    let no_colon = "event image_edit.completed\ndata image_edit.completed\n\n";
    assert_eq!(stream_count(no_colon, true), None);
    let indented = " event: image_edit.completed\n\n";
    assert_eq!(stream_count(indented, true), None);
    // The last `event:` of an event names it.
    let renamed = "event: image_edit.completed\nevent: image_edit.partial_image\n\n";
    assert_eq!(stream_count(renamed, true), None);
}

/// The data's top-level `type` names the event when it is a string in a
/// whole object; otherwise the `event:` name does — as before.
#[test]
fn the_datas_type_wins_over_the_event_name() {
    let partial = json!({"type": "image_generation.partial_image"});
    let renamed = format!("event: image_generation.completed\ndata: {partial}\n\n");
    assert_eq!(stream_count(&renamed, true), None);
    for data in [
        r#"{"type":null}"#,
        r#"{"x":{"type":"image_edit.partial_image"}}"#,
        r#"{"type":"image_edit.partial_image""#,
        r#"{"type":"image_edit.partial_image"} trailing"#,
        "[DONE]",
        "",
    ] {
        let stream = format!("event: image_edit.completed\ndata: {data}\n\n");
        assert_eq!(stream_count(&stream, true), Some(1), "{data}");
    }
    // A type spelled with an escape, or longer than every final name, is
    // none of them.
    for data in [
        r#"{"type":"image_edit.complete\u0064"}"#,
        r#"{"type":"image_edit.completed.v2"}"#,
        r#"{"note":"\"type\":\"image_edit.completed\"","type":"x"}"#,
    ] {
        assert_eq!(
            stream_count(&format!("data: {data}\n\n"), true),
            None,
            "{data}"
        );
    }
}

/// A streamed image event is one base64 image of several megabytes, relayed
/// in small chunks. Its count is right, and comes in linear time. Measured
/// in a debug build, 2026-10-08, on this input: about 0.25 s here, against
/// 12 s for the `SseDecoder` this replaced, which kept each event whole and
/// searched it again on every chunk (45 s at 4 KiB chunks). The bound is
/// loose, for a loaded machine, and still far below the quadratic figure.
#[test]
fn multi_megabyte_events_in_small_chunks_are_counted_in_linear_time() {
    let image = "aVZCT1J3MEs=".repeat(6 * 1024 * 1024 / 12);
    let event = |kind: &str| {
        let data = json!({"type": kind, "b64_json": image, "created_at": 1});
        format!("event: {kind}\ndata: {data}\n\n")
    };
    let stream = [
        event("image_generation.partial_image"),
        event("image_generation.completed"),
        event("image_generation.completed"),
        event("image_generation.partial_image"),
        event("image_generation.completed"),
        event("image_generation.completed"),
    ]
    .concat();
    assert!(stream.len() > 36 * 1024 * 1024);
    let started = std::time::Instant::now();
    let mut count = FinalEvents::default();
    for c in stream.as_bytes().chunks(16 * 1024) {
        count.feed(c);
    }
    assert_eq!(count.images(true), Some(4));
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_secs(5), "{took:?}");
}
