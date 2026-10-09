//! Gemini thought signatures (gateway design §7.1), after Google's "Thought
//! signatures" page for `generateContent`
//! (<https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures>,
//! read 2026-10-09):
//!
//! - Gemini 3 puts a signature on the first `functionCall` part of each
//!   step (parallel calls: the first only), and returning it is mandatory:
//!   the first `functionCall` of each step of the **current turn** without
//!   one is a 400. The current turn starts at the most recent user message
//!   with standard content (text, not a `functionResponse`); calls before
//!   it are not checked.
//! - A call the API did not generate (one lmgw or a client wrote, or one
//!   from a model without signatures) takes a documented dummy value that
//!   skips the check. lmgw gives it to every step's unsigned first call,
//!   older turns included ([`sign_unsigned_steps`]).
//! - Signatures on text parts (Gemini 3's last part, Gemini 2.5's first
//!   part) are recommended back but never checked; lmgw has no slot a
//!   client echoes for them and drops them.

use serde_json::{json, Value};

/// The part field the signature lives in, as the REST API writes it.
pub(super) const FIELD: &str = "thoughtSignature";

/// The value Google documents for a `functionCall` part the model did not
/// generate: it skips the signature's validation. Sent to every Gemini
/// model; that one before Gemini 3, which checks nothing, accepts it is
/// documented by omission only, not verified live.
pub(super) const SKIP: &str = "skip_thought_signature_validator";

/// The signature a response part carries, if any. The REST API writes
/// `thoughtSignature`; the snake-case spelling Google's history examples
/// use is read too.
pub(super) fn of_part(part: &Value) -> Option<&str> {
    [FIELD, "thought_signature"]
        .iter()
        .find_map(|k| part.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

/// Give the first `functionCall` of every model step that has no signature
/// the skip value, and touch nothing else: a call with its own signature
/// keeps it, and later calls of a parallel step need none.
///
/// Gemini 3 checks only the current turn's steps, but which steps those are
/// is Google's predicate, not lmgw's: an unsigned first call in an older
/// step gets the skip value too. Google documents the value for any call
/// the API did not generate, and an older step is not validated, so it is
/// harmless there — and a request never hinges on lmgw drawing the turn's
/// boundary exactly where Gemini does.
pub(super) fn sign_unsigned_steps(contents: &mut [Value]) {
    for step in contents.iter_mut() {
        if step.get("role").and_then(Value::as_str) != Some("model") {
            continue;
        }
        let Some(parts) = step.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        if let Some(first) = parts.iter_mut().find(|p| p.get("functionCall").is_some()) {
            if of_part(first).is_none() {
                first[FIELD] = json!(SKIP);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(content: &Value) -> &[Value] {
        content
            .get("parts")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    fn call(name: &str) -> Value {
        json!({"functionCall": {"name": name, "args": {}}})
    }

    fn signed(name: &str, sig: &str) -> Value {
        json!({"functionCall": {"name": name, "args": {}}, "thoughtSignature": sig})
    }

    fn response(name: &str) -> Value {
        json!({"functionResponse": {"name": name, "response": {}}})
    }

    fn sigs(contents: &[Value]) -> Vec<Vec<Option<&str>>> {
        contents
            .iter()
            .map(|c| parts(c).iter().map(|p| p[FIELD].as_str()).collect())
            .collect()
    }

    #[test]
    fn the_first_unsigned_call_of_every_step_gets_the_skip_value() {
        let mut contents = vec![
            json!({"role": "user", "parts": [{"text": "old question"}]}),
            json!({"role": "model", "parts": [call("a"), call("a2")]}),
            json!({"role": "user", "parts": [response("a"), response("a2")]}),
            json!({"role": "model", "parts": [{"text": "old answer"}]}),
            json!({"role": "user", "parts": [{"text": "new question"}]}),
            json!({"role": "model", "parts": [{"text": "let me look"}, call("b"), call("c")]}),
            json!({"role": "user", "parts": [response("b"), response("c")]}),
            json!({"role": "model", "parts": [signed("d", "SIG_D"), call("e")]}),
            json!({"role": "user", "parts": [response("d"), response("e")]}),
        ];
        sign_unsigned_steps(&mut contents);
        assert_eq!(
            sigs(&contents),
            vec![
                vec![None],
                vec![Some(SKIP), None],
                vec![None, None],
                vec![None],
                vec![None],
                vec![None, Some(SKIP), None],
                vec![None, None],
                vec![Some("SIG_D"), None],
                vec![None, None],
            ]
        );
    }

    #[test]
    fn a_signed_older_step_keeps_its_signature() {
        let mut contents = vec![
            json!({"role": "user", "parts": [{"text": "q"}]}),
            json!({"role": "model", "parts": [signed("a", "OLD")]}),
            json!({"role": "user", "parts": [response("a"), {"text": "never mind"}]}),
            json!({"role": "model", "parts": [call("b")]}),
        ];
        sign_unsigned_steps(&mut contents);
        assert_eq!(
            sigs(&contents),
            vec![
                vec![None],
                vec![Some("OLD")],
                vec![None, None],
                vec![Some(SKIP)]
            ]
        );
    }

    #[test]
    fn a_part_signature_is_read_in_either_spelling() {
        assert_eq!(of_part(&json!({"thoughtSignature": "A"})), Some("A"));
        assert_eq!(of_part(&json!({"thought_signature": "B"})), Some("B"));
        assert_eq!(of_part(&json!({"thoughtSignature": ""})), None);
        assert_eq!(of_part(&json!({"text": "x"})), None);
    }
}
