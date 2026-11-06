//! The end of a relayed native stream whose upstream failed before its
//! terminal event: fell silent for longer than its timeout
//! (`proxy::silence`), or broke. The upstream's own events stop there, so
//! the client is told in the API's terms that the response failed, rather
//! than left with a stream that just ends. An upstream that said so itself
//! (an `error` event, a `response.failed`) is not told twice: the relay
//! appends nothing then.

use serde_json::{json, Value};

use super::Meter;

impl Meter {
    /// The frame that ends the stream: `response.failed` on the Response
    /// object `response.created` opened, marked failed with OpenAI's
    /// `server_error` and `message`, its `output` the items finished so far;
    /// or, before any Response object came, an `error` event. Its
    /// `sequence_number` follows the last one the upstream sent (the events
    /// relayed count, when the last carried none).
    ///
    /// The relayed bytes are the upstream's as they came, so the last of
    /// them may be half an event: the frame opens with a blank line, which
    /// ends that one (a blank line after a whole event dispatches nothing),
    /// so the failure is an event of its own and parses.
    pub(super) fn failed_event(&self, message: &str) -> String {
        let seq = self
            .last_data
            .as_deref()
            .and_then(|d| serde_json::from_str::<Value>(d).ok())
            .and_then(|d| d.get("sequence_number").and_then(Value::as_u64))
            .map_or(self.events, |s| s + 1);
        let error = json!({"code": "server_error", "message": message});
        let (name, data) = match &self.opened {
            Some(opened) => {
                let mut response = opened.clone();
                if let Some(r) = response.as_object_mut() {
                    r.insert("status".into(), json!("failed"));
                    r.insert("error".into(), error);
                    r.insert("usage".into(), Value::Null);
                    let mut done = self.done_items.clone();
                    done.sort_by_key(|(index, _)| *index);
                    r.insert(
                        "output".into(),
                        Value::Array(done.into_iter().map(|(_, item)| item).collect()),
                    );
                }
                (
                    "response.failed",
                    json!({"type": "response.failed", "sequence_number": seq, "response": response}),
                )
            }
            None => (
                "error",
                json!({"type": "error", "code": "server_error", "message": message,
                       "param": null, "sequence_number": seq}),
            ),
        };
        format!("\n\n{}", crate::sse::frame(Some(name), &data.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(frame: &str) -> Value {
        let ev = crate::sse::SseDecoder::new().feed(frame.as_bytes());
        assert_eq!(ev.len(), 1, "{frame}");
        serde_json::from_str(&ev[0].data).unwrap()
    }

    #[test]
    fn a_failed_stream_ends_on_its_own_response_marked_failed() {
        let mut m = Meter::default();
        let _ = m.feed(
            b"event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\
              \"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\",\"output\":[]}}\n\n\
              event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\
              \"sequence_number\":4,\"output_index\":1,\"item\":{\"type\":\"message\",\"id\":\"msg_1\"}}\n\n\
              event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\
              \"sequence_number\":3,\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n\
              event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\
              \"sequence_number\":7,\"delta\":\"Hel\"}\n\n",
        );
        let ev = data(&m.failed_event("silent"));
        assert_eq!(ev["type"], "response.failed");
        assert_eq!(ev["sequence_number"], 8);
        assert_eq!(ev["response"]["id"], "resp_1");
        assert_eq!(ev["response"]["status"], "failed");
        assert_eq!(
            ev["response"]["error"],
            json!({"code": "server_error", "message": "silent"})
        );
        let ids: Vec<_> = ev["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["rs_1", "msg_1"]);
    }

    #[test]
    fn without_sequence_numbers_it_counts_the_events() {
        let mut m = Meter::default();
        let _ = m.feed(
            b"event: response.created\ndata: {\"type\":\"response.created\",\
              \"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\",\"output\":[]}}\n\n\
              event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\
              \"delta\":\"Hel\"}\n\n",
        );
        assert_eq!(data(&m.failed_event("silent"))["sequence_number"], 2);
    }

    #[test]
    fn after_half_an_event_the_failure_is_an_event_of_its_own() {
        let mut m = Meter::default();
        let half = b"event: response.output_text.delta\ndata: {\"type\":\"response.out";
        let _ = m.feed(half);
        let mut relayed = half.to_vec();
        relayed.extend_from_slice(m.failed_event("broke").as_bytes());
        let evs = crate::sse::SseDecoder::new().feed(&relayed);
        let last = evs.last().unwrap();
        assert_eq!(last.event.as_deref(), Some("error"));
        let last: Value = serde_json::from_str(&last.data).unwrap();
        assert_eq!(last["message"], "broke");
    }

    #[test]
    fn before_any_response_object_it_is_an_error_event() {
        let ev = data(&Meter::default().failed_event("silent"));
        assert_eq!(
            (ev["type"].as_str(), ev["message"].as_str()),
            (Some("error"), Some("silent"))
        );
    }
}
