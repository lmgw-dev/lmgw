//! The reasoning trace's two names (module doc of [`super::reasoning_text`]).

use serde_json::{json, Value};

use crate::egress::openai::OpenaiEgress;
use crate::egress::Egress;
use crate::ir::StreamDelta;
use crate::sse::SseEvent;

/// OpenRouter's `reasoning` string is read as well as llama-server's
/// `reasoning_content`, which wins when both come — streamed and whole.
#[test]
fn reasoning_is_read_under_either_name() {
    let mut dec = OpenaiEgress.new_decoder();
    let mut deltas = Vec::new();
    for c in [
        json!({"choices":[{"delta":{"role":"assistant","reasoning":"Let me "}}]}),
        json!({"choices":[{"delta":{"reasoning":"x","reasoning_content":"think."}}]}),
        // An object under the name is no text.
        json!({"choices":[{"delta":{"reasoning":{"effort":"low"},"content":"OK"}}]}),
    ] {
        deltas.extend(dec.on_event(&SseEvent {
            event: None,
            data: c.to_string(),
        }));
    }
    assert_eq!(
        deltas,
        vec![
            StreamDelta::ReasoningDelta("Let me ".into()),
            StreamDelta::ReasoningDelta("think.".into()),
            StreamDelta::TextDelta("OK".into()),
        ]
    );
    let whole = |m: Value| {
        let body = json!({"model": "m", "choices": [{"message": m}]}).to_string();
        OpenaiEgress
            .parse_completion(body.as_bytes())
            .unwrap()
            .reasoning
    };
    assert_eq!(whole(json!({"content": "OK", "reasoning": "Hm."})), "Hm.");
    assert_eq!(
        whole(json!({"content": "OK", "reasoning": "a", "reasoning_content": "b"})),
        "b"
    );
    assert_eq!(whole(json!({"content": "OK"})), "");
}
