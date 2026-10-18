//! The text a transcription or a task answered, the one rule both sides
//! read: the gateway's file export (a transcript file of a run) and the
//! Audio lab card (whether "Save transcript…" is on).

use serde_json::Value;

/// The JSON payload of each record of an event stream, in order; a record
/// that carries none, `[DONE]` or no JSON is skipped. The event name lives
/// in the `event:` line or in the payload's `type` (the payload wins), so a
/// payload without a `type` gets the line's.
pub fn sse_events(text: &str) -> impl Iterator<Item = Value> + '_ {
    text.split("\n\n")
        .flat_map(|chunk| chunk.split("\r\n\r\n"))
        .filter_map(|record| {
            let mut event = None;
            let mut data = String::new();
            for line in record.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event = Some(rest.trim());
                } else if let Some(rest) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                }
            }
            if data.is_empty() || data == "[DONE]" {
                return None;
            }
            let mut v: Value = serde_json::from_str(&data).ok()?;
            if v.get("type").is_none() {
                if let (Some(e), Some(o)) = (event, v.as_object_mut()) {
                    o.insert("type".into(), Value::from(e));
                }
            }
            Some(v)
        })
}

/// The text a transcription or a task answered: a JSON answer's `text` (a
/// `/v1/tasks/stream` answer's `result.text`), or an event stream's
/// `transcript.text.delta` events folded, a `transcript.text.done` with a
/// text replacing them. `None` when it has no text.
pub fn transcript(response: &str) -> Option<String> {
    let text = match serde_json::from_str::<Value>(response) {
        Ok(j) => {
            let r = if j["result"].is_object() {
                &j["result"]
            } else {
                &j
            };
            r["text"].as_str().map(str::to_string)
        }
        Err(_) => {
            let mut said: Option<String> = None;
            for v in sse_events(response) {
                match v["type"].as_str() {
                    Some("transcript.text.delta") => {
                        let d = v["delta"].as_str().or_else(|| v["text"].as_str());
                        said.get_or_insert_with(String::new)
                            .push_str(d.unwrap_or_default());
                    }
                    Some("transcript.text.done") => {
                        if let Some(t) = v["text"].as_str() {
                            said = Some(t.to_string());
                        }
                    }
                    _ => {}
                }
            }
            said
        }
    };
    text.filter(|t| !t.trim().is_empty())
}

/// The transcript a run's answer holds: [`transcript`] of a 2xx answer, and
/// `None` for a failed run or no answer.
pub fn run_transcript(http_status: u16, response: Option<&str>) -> Option<String> {
    if !(200..300).contains(&http_status) {
        return None;
    }
    response.and_then(transcript)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transcript_is_the_answers_text_however_it_came() {
        assert_eq!(
            transcript(r#"{"text": "hello there", "language": "en"}"#).as_deref(),
            Some("hello there")
        );
        assert_eq!(
            transcript(r#"{"events": [], "result": {"text": "from a task"}}"#).as_deref(),
            Some("from a task")
        );
        assert_eq!(transcript(r#"{"text": "  "}"#), None, "blank is none");
        assert_eq!(transcript(r#"{"segments": []}"#), None);
        let deltas = "data: {\"type\": \"transcript.text.delta\", \"delta\": \"hel\"}\n\n\
                      event: transcript.text.delta\ndata: {\"delta\": \"lo\"}\n\n";
        assert_eq!(transcript(deltas).as_deref(), Some("hello"));
        let done = format!(
            "{deltas}data: {{\"type\": \"transcript.text.done\", \"text\": \"Hello.\"}}\n\n"
        );
        assert_eq!(transcript(&done).as_deref(), Some("Hello."));
        assert_eq!(transcript("not json, not a stream"), None);
    }

    #[test]
    fn a_failed_run_or_no_answer_has_no_transcript() {
        assert_eq!(
            run_transcript(200, Some(r#"{"text": "hi"}"#)).as_deref(),
            Some("hi")
        );
        assert_eq!(run_transcript(500, Some(r#"{"text": "hi"}"#)), None);
        assert_eq!(run_transcript(200, None), None);
        let blank = "data: {\"type\": \"transcript.text.delta\", \"delta\": \" \"}\n\n";
        assert_eq!(run_transcript(200, Some(blank)), None, "events of blanks");
    }
}
