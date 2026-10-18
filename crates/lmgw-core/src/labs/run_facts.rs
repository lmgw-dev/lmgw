//! What a run keeps of a route's answer besides its bytes (labs-store design
//! §1.1): the `x-lmgw-*` headers, a failure's code and message, a success's
//! upstream model and tokens. One reading for every lab that dispatches
//! through a `/v1` route.

use std::collections::BTreeMap;

use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::Value;

/// The `x-lmgw-*` headers of an answer.
pub(crate) fn gateway_headers(h: &HeaderMap) -> BTreeMap<String, String> {
    h.iter()
        .filter(|(name, _)| name.as_str().starts_with("x-lmgw-"))
        .filter_map(|(name, v)| Some((name.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect()
}

/// What a failure's body says: the envelope's code and message; the whole
/// text as the message when it is no envelope. The code is `lmgw_code`,
/// else a string `code`, else `type`, else a numeric `code`: llama.cpp's
/// envelope carries `"code": 500, "type": "server_error"`, and the number
/// only repeats the status the run keeps already.
pub(crate) fn failure_of(text: Option<&str>) -> (Option<String>, Option<String>) {
    #[derive(Deserialize)]
    struct Envelope {
        error: Option<ErrorBody>,
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        message: Option<String>,
        lmgw_code: Option<String>,
        code: Option<Value>,
        #[serde(rename = "type")]
        kind: Option<String>,
    }
    let Some(text) = text else {
        return (None, None);
    };
    match serde_json::from_str::<Envelope>(text) {
        Ok(Envelope { error: Some(e) }) => {
            let (named, number) = match e.code {
                Some(Value::String(s)) => (Some(s), None),
                Some(Value::Number(n)) => (None, Some(n.to_string())),
                _ => (None, None),
            };
            let code = e.lmgw_code.or(named).or(e.kind).or(number);
            (code, e.message)
        }
        _ => (
            None,
            Some(text.trim().to_string()).filter(|t| !t.is_empty()),
        ),
    }
}

/// What a success's body says: the upstream's own `model`, and the tokens
/// it reported.
pub(crate) fn answer_of(text: Option<&str>) -> (Option<String>, Option<i64>, Option<i64>) {
    #[derive(Deserialize)]
    struct Answered {
        model: Option<String>,
        usage: Option<Counts>,
    }
    #[derive(Deserialize)]
    struct Counts {
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
    }
    match text.and_then(|t| serde_json::from_str::<Answered>(t).ok()) {
        Some(a) => (
            a.model,
            a.usage.as_ref().and_then(|u| u.input_tokens),
            a.usage.as_ref().and_then(|u| u.output_tokens),
        ),
        None => (None, None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_keeps_the_code_that_says_most() {
        let of = |t: &str| failure_of(Some(t));
        // llama.cpp: a number that repeats the status, and a type
        assert_eq!(
            of(r#"{"error":{"code":500,"message":"boom","type":"server_error"}}"#),
            (Some("server_error".into()), Some("boom".into()))
        );
        // lmgw's own code wins over everything
        assert_eq!(
            of(r#"{"error":{"code":"x","lmgw_code":"gpu_hold","type":"t","message":"held"}}"#).0,
            Some("gpu_hold".into())
        );
        // a string code before the type
        assert_eq!(
            of(r#"{"error":{"code":"model_not_found","type":"invalid_request_error"}}"#).0,
            Some("model_not_found".into())
        );
        // a number when nothing else names it
        assert_eq!(of(r#"{"error":{"code":501}}"#).0, Some("501".into()));
        // no envelope: the text is the message
        assert_eq!(
            of(" upstream died \n"),
            (None, Some("upstream died".into()))
        );
        assert_eq!(failure_of(None), (None, None));
    }
}
