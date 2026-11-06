//! The failed run's `error.code` on the Responses loop's response object
//! (openai egress design §7.2.3): OpenAI's own code when the upstream's
//! Response failed with one, else the nearest of OpenAI's codes.

use serde_json::{json, Value};

use super::ResponsesEncoder;
use crate::error::{FailedAnswer, GatewayError};

/// The `response.failed` event's response object after `e`.
fn failed_with(e: &GatewayError) -> Value {
    let mut enc = ResponsesEncoder::new("m", json!({}), true);
    let frames = enc.fail(e);
    frames
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .find(|v| v["type"] == "response.failed")
        .expect("a response.failed event")["response"]
        .clone()
}

#[test]
fn a_failed_upstream_response_keeps_openai_s_code() {
    let e = GatewayError::UpstreamFailed(Box::new(FailedAnswer {
        code: Some("server_error".into()),
        message: "The model broke.".into(),
        usage: Default::default(),
        service_tier: None,
    }));
    let r = failed_with(&e);
    assert_eq!(r["status"], "failed");
    assert_eq!(r["error"]["code"], "server_error", "{r}");
    assert_eq!(
        r["error"]["message"],
        "the upstream's response failed: The model broke. (server_error)"
    );
}

/// Always a word of OpenAI's enum (api-compat design §9 finding 8): the
/// nearest by the error's kind and status, lmgw's text in the message.
#[test]
fn a_failure_without_openai_s_code_takes_the_nearest_of_openai_s() {
    let code = |e: &GatewayError| failed_with(e)["error"]["code"].clone();
    let failed = |code: Option<&str>| {
        GatewayError::UpstreamFailed(Box::new(FailedAnswer {
            code: code.map(String::from),
            message: "The model broke.".into(),
            usage: Default::default(),
            service_tier: None,
        }))
    };
    let no_code = failed(None);
    // A code outside OpenAI's enum is not passed on.
    let odd = failed(Some("something_new"));
    let refused = GatewayError::Upstream {
        status: 400,
        provider_type: Some("invalid_request_error".into()),
        message: "bad".into(),
    };
    let schema = GatewayError::InvalidJsonSchema {
        message: "Invalid schema.".into(),
    };
    assert_eq!(code(&no_code), "server_error");
    assert_eq!(code(&odd), "server_error");
    assert_eq!(code(&GatewayError::Timeout), "server_error");
    assert_eq!(code(&refused), "invalid_prompt");
    let r = failed_with(&schema);
    assert_eq!(r["error"]["code"], "invalid_prompt", "{r}");
    assert_eq!(
        r["error"]["message"],
        schema.to_string(),
        "lmgw's text kept"
    );
    for e in [&no_code, &odd, &refused, &schema, &GatewayError::Timeout] {
        let c = code(e);
        assert!(
            super::RESPONSE_ERROR_CODES.contains(&c.as_str().unwrap()),
            "{c}"
        );
    }
}
