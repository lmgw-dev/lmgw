//! System One (decision-models design §2.1, §4.1): the client's object, and
//! the `POST {base}/systemone` that carries it upstream.
//!
//! llama-server, TypeSafe and OpenRouter all serve the route under the base an
//! OpenAI-protocol row already has (`…/v1`, `https://openrouter.ai/api/v1`),
//! so the `openai` and `llama_cpp` egresses share this builder, as they share
//! the rerank one. The body is the client's own with `model` rewritten and
//! nothing else touched: not re-read through a `Value`, whose map would sort
//! the members (`serde_json::Map` is a `BTreeMap` in this build), and the
//! order of a `choice` question's `criteria` is the order of its options.

use axum::http::header;
use serde_json::value::RawValue;

use crate::config::Upstream;
use crate::egress::apply_bearer_auth;
use crate::error::GatewayError;
use lmgw_api_types::systemone::Ordered;

/// A client's System One object, as it came: its members in order, each value
/// the bytes it was sent as.
#[derive(Debug, Clone)]
pub struct SystemOneBody(Ordered<Box<RawValue>>);

impl SystemOneBody {
    /// Read `bytes` as one JSON object: a `400` when they are not JSON, or are
    /// JSON but not an object. Nothing inside it is looked at.
    pub fn parse(bytes: &[u8]) -> Result<Self, GatewayError> {
        let raw: &RawValue = serde_json::from_slice(bytes)
            .map_err(|e| GatewayError::BadRequest(format!("the body is not JSON: {e}")))?;
        if !raw.get().starts_with('{') {
            return Err(GatewayError::BadRequest(
                "the body must be a JSON object: a System One request is {model, state, \
                 questions}"
                    .into(),
            ));
        }
        serde_json::from_str(raw.get())
            .map(Self)
            .map_err(|e| GatewayError::BadRequest(format!("the body is not JSON: {e}")))
    }

    /// `model`: `None` when the object has none, `Some(Err(sent))` with the
    /// JSON it sent when that is no string.
    pub fn model(&self) -> Option<Result<String, String>> {
        let raw = self.0.get("model")?.get();
        Some(serde_json::from_str(raw).map_err(|_| raw.to_string()))
    }

    /// The object with `model` set to `model`, every other member as sent and
    /// where it was sent.
    pub fn with_model(&self, model: &str) -> Result<Vec<u8>, GatewayError> {
        let to_raw = |s: &str| {
            serde_json::to_string(s)
                .ok()
                .and_then(|s| RawValue::from_string(s).ok())
                .ok_or_else(|| GatewayError::Internal(format!("cannot write the model '{s}'")))
        };
        let mut members = Vec::with_capacity(self.0.len());
        for (k, v) in self.0.iter() {
            let v = if k == "model" {
                to_raw(model)?
            } else {
                v.clone()
            };
            members.push((k.to_string(), v));
        }
        serde_json::to_vec(&Ordered(members))
            .map_err(|e| GatewayError::Internal(format!("cannot write the body: {e}")))
    }
}

/// `POST {base}/systemone` with `body`, `model` rewritten to `model`, under
/// the upstream's bearer and extra headers.
pub fn build_systemone(
    http: &reqwest::Client,
    up: &Upstream,
    model: &str,
    body: &SystemOneBody,
) -> Result<reqwest::RequestBuilder, GatewayError> {
    let rb = http
        .post(format!("{}/systemone", up.base()))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.with_model(model)?);
    Ok(apply_bearer_auth(rb, up))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_model_changes_and_every_member_keeps_its_place_and_bytes() {
        let sent = br#"{"state": {"b": 1, "a": 2.50},
            "model": "decide/laya",
            "questions": {"q": {"type": "choice", "instructions": "Which?",
                "criteria": {"technical": null, "billing": "refunds", "sales": null}}}}"#;
        let body = SystemOneBody::parse(sent).unwrap();
        assert_eq!(body.model(), Some(Ok("decide/laya".to_string())));
        let out = String::from_utf8(body.with_model("laya").unwrap()).unwrap();
        assert_eq!(
            out,
            r#"{"state":{"b": 1, "a": 2.50},"model":"laya","questions":{"q": {"type": "choice", "instructions": "Which?",
                "criteria": {"technical": null, "billing": "refunds", "sales": null}}}}"#
        );
    }

    #[test]
    fn a_body_that_is_not_an_object_or_not_json_is_a_400() {
        for bad in [&b"nope"[..], b"[1, 2]", b"\"model\"", b""] {
            let e = SystemOneBody::parse(bad).unwrap_err();
            assert_eq!(e.http_status(), 400, "{e}");
        }
        assert!(SystemOneBody::parse(b"[1]")
            .unwrap_err()
            .to_string()
            .contains("JSON object"));
        // A `model` that is no string comes back as sent, for the refusal.
        let body = SystemOneBody::parse(br#"{"model": 7}"#).unwrap();
        assert_eq!(body.model(), Some(Err("7".to_string())));
        let body = SystemOneBody::parse(br#"{"state": "s"}"#).unwrap();
        assert_eq!(body.model(), None);
    }
}
