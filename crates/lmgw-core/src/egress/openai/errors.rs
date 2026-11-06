//! OpenAI's error bodies read as lmgw's errors (openai egress design §8, P4):
//! OpenAI's shape only, `error.{message, type, param, code}`, with `code`
//! read exactly. Every body this does not recognise is the generic
//! [`openai_wire::upstream_error`], as before; llama-server's context
//! refusal is not read here (a platform row never fronts llama-server).
//!
//! The codes, as OpenAI documents them (checked 2026-10-24):
//! - the `429`s, <https://developers.openai.com/api/docs/guides/error-codes.md>:
//!   "Rate limit reached for requests" (no code given), `slow_down` (type
//!   `rate_limit_error`), and the billing family `credit_balance_exhausted`,
//!   `organization_spend_limit_exceeded`, `project_spend_limit_exceeded` and
//!   `organization_usage_limit_exceeded` — "The broader `error.type` can
//!   still be `insufficient_quota`", and "Retrying billing, spend, or quota
//!   errors won't restore API access";
//! - flex, <https://developers.openai.com/api/docs/guides/flex-processing.md>:
//!   "a `429 Resource Unavailable` error code. **You will not be charged
//!   when this occurs.**", and the retry it recommends: "set `service_tier`
//!   to `auto` in the retried request". The page gives no body, so the flex
//!   body in the corpus is a stand-in until the live probe (WP6) captures
//!   one; matched by its code `resource_unavailable` or a message saying
//!   "Resource Unavailable";
//! - `context_length_exceeded`: in no current page; the body is the
//!   long-standing one OpenAI sends (`param: messages`, the message "This
//!   model's maximum context length is N tokens. However, …"), also until
//!   WP6 captures it.

use serde_json::Value;

use crate::egress::openai_wire;
use crate::error::{GatewayError, Relayed, UpstreamLimit};

/// The billing family's codes (the error-codes guide, above), and the older
/// `insufficient_quota`.
const QUOTA_CODES: [&str; 5] = [
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
];

/// The platform's `map_error`.
pub(super) fn map_error(status: u16, body: &[u8]) -> GatewayError {
    let v: Value = serde_json::from_slice(body).unwrap_or_default();
    let Some(error) = v.get("error").filter(|e| e.is_object()) else {
        return openai_wire::upstream_error(status, body);
    };
    let text = |k: &str| error.get(k).and_then(Value::as_str);
    let (Some(message), kind, code) = (text("message"), text("type"), text("code")) else {
        return openai_wire::upstream_error(status, body);
    };
    match status {
        400 if code == Some("context_length_exceeded") => context_exceeded(message),
        429 => GatewayError::UpstreamLimited {
            limit: limit_of(kind, code, message),
            code: code.map(Box::from),
            error_type: kind.map(Box::from),
            message: message.to_string(),
            // The send site's, which has the answer's headers
            // (`egress::chat_error`).
            hints: Default::default(),
            note: None,
        },
        // A 4xx lmgw has no word for (an unknown parameter, a deprecated
        // model): OpenAI's own object goes to the client as sent. Not the
        // auth statuses, which are the owner's key, not the client's.
        400..=499 if !matches!(status, 401 | 403) => {
            GatewayError::UpstreamRelayed(Box::new(Relayed {
                status,
                error_type: kind.map(String::from),
                code: code.map(String::from),
                param: text("param").map(String::from),
                message: message.to_string(),
            }))
        }
        _ => openai_wire::upstream_error(status, body),
    }
}

/// Which limit a `429` names: the billing family by its code or its type,
/// flex's capacity by its code or its words, and every other `429` a rate
/// limit (`rate_limit_exceeded`, `slow_down`, a type of `tokens` or
/// `requests`).
fn limit_of(kind: Option<&str>, code: Option<&str>, message: &str) -> UpstreamLimit {
    if kind == Some("insufficient_quota") || code.is_some_and(|c| QUOTA_CODES.contains(&c)) {
        UpstreamLimit::Quota
    } else if code == Some("resource_unavailable")
        || message
            .to_ascii_lowercase()
            .starts_with("resource unavailable")
    {
        UpstreamLimit::FlexCapacity
    } else {
        UpstreamLimit::Rate
    }
}

/// `context_length_exceeded` as [`GatewayError::ContextExceeded`] with the
/// numbers its message states, or, when the message is not in a shape read
/// whole here, as [`GatewayError::ContextExceededAsWorded`] with OpenAI's
/// words: no number is guessed. `model` is left empty for the send site's
/// `gate::attribute`, as llama-server's backstop leaves it.
///
/// The shapes read: "This model's maximum context length is L tokens.
/// However, your messages resulted in T tokens." or "… you requested T
/// tokens", either optionally followed by a breakdown of `N in the X` parts —
/// "(7000 in the messages, 1500 in the completion)", "(7900 in the messages,
/// 315 in the functions)", "(7000 in the messages, 500 in the functions, and
/// 1500 in the completion)". `N in the completion` is the max output; the
/// prompt is the total minus it (the total itself without a completion
/// part), so the functions count towards the prompt. A breakdown whose parts
/// do not add up to the total is not understood.
fn context_exceeded(message: &str) -> GatewayError {
    match context_numbers(message) {
        Some((limit, prompt_tokens, max_output)) => GatewayError::ContextExceeded {
            model: String::new(),
            prompt_tokens,
            max_output,
            limit,
            top_rung: None,
        },
        None => GatewayError::ContextExceededAsWorded {
            model: String::new(),
            message: message.to_string(),
        },
    }
}

/// [`context_exceeded`]'s reading: `(limit, prompt, max_output)`, `None` for
/// any shape it does not understand whole.
fn context_numbers(message: &str) -> Option<(u64, u64, Option<u64>)> {
    let (limit, _) = number_then(message, "maximum context length is ")?;
    let (total, rest) =
        number_then(message, "resulted in ").or_else(|| number_then(message, "you requested "))?;
    let rest = rest.strip_prefix(" tokens")?;
    let Some(inner) = rest.strip_prefix(" (") else {
        // No breakdown: the sentence has to end here.
        return rest.starts_with('.').then_some((limit, total, None));
    };
    let inner = &inner[..inner.find(')')?];
    let mut sum = 0u64;
    let mut completion = None;
    for part in inner.split(", ") {
        let part = part.strip_prefix("and ").unwrap_or(part);
        let (n, what) = part.split_once(" in the ")?;
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n: u64 = n.parse().ok()?;
        match what {
            "messages" | "functions" => {}
            "completion" if completion.is_none() => completion = Some(n),
            _ => return None,
        }
        sum = sum.checked_add(n)?;
    }
    if sum != total {
        return None;
    }
    let prompt = total.checked_sub(completion.unwrap_or(0))?;
    Some((limit, prompt, completion))
}

/// The whole number right after the first `marker` in `text`, and the text
/// after it.
fn number_then<'a>(text: &'a str, marker: &str) -> Option<(u64, &'a str)> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    Some((rest[..end].parse().ok()?, &rest[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(status_type: &str, code: Option<&str>, message: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"error": {
            "message": message, "type": status_type, "param": null, "code": code,
        }}))
        .unwrap()
    }

    #[test]
    fn the_context_numbers_are_read_where_the_message_states_them() {
        let read = |m: &str| match map_error(
            400,
            &body("invalid_request_error", Some("context_length_exceeded"), m),
        ) {
            GatewayError::ContextExceeded {
                prompt_tokens,
                max_output,
                limit,
                ..
            } => (prompt_tokens, max_output, limit),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            read(
                "This model's maximum context length is 128000 tokens. However, your messages \
                 resulted in 130021 tokens. Please reduce the length of the messages."
            ),
            (130_021, None, 128_000)
        );
        assert_eq!(
            read(
                "This model's maximum context length is 8192 tokens. However, you requested \
                 8500 tokens (7000 in the messages, 1500 in the completion). Please reduce the \
                 length of the messages or completion."
            ),
            (7000, Some(1500), 8192)
        );
        // The functions count towards the prompt.
        assert_eq!(
            read(
                "This model's maximum context length is 8192 tokens. However, your messages \
                 resulted in 8215 tokens (7900 in the messages, 315 in the functions). Please \
                 reduce the length of the messages or functions."
            ),
            (8215, None, 8192)
        );
        assert_eq!(
            read(
                "This model's maximum context length is 8192 tokens. However, you requested \
                 9000 tokens (7000 in the messages, 500 in the functions, and 1500 in the \
                 completion). Please reduce the length of the messages, functions, or \
                 completion."
            ),
            (7500, Some(1500), 8192)
        );
    }

    /// A wording without both numbers keeps OpenAI's words under the same
    /// code, and no number is made up.
    #[test]
    fn a_context_refusal_without_numbers_keeps_openais_words() {
        for message in [
            "Your input exceeds the context window of this model.",
            "This model's maximum context length is 8192 tokens.",
            // A part not read here, a breakdown that does not add up, a total
            // without its unit.
            "This model's maximum context length is 8192 tokens. However, you requested 9000 \
             tokens (7000 in the messages, 2000 in the images).",
            "This model's maximum context length is 8192 tokens. However, you requested 9000 \
             tokens (7000 in the messages, 1500 in the completion).",
            "This model's maximum context length is 8192 tokens. However, your messages \
             resulted in 9000",
        ] {
            let e = map_error(
                400,
                &body(
                    "invalid_request_error",
                    Some("context_length_exceeded"),
                    message,
                ),
            );
            assert!(
                matches!(e, GatewayError::ContextExceededAsWorded { .. }),
                "{e:?}"
            );
            assert_eq!(e.kind(), "context_length_exceeded");
            assert_eq!(e.http_status().as_u16(), 400);
            assert_eq!(e.openai_type(), "invalid_request_error");
            assert_eq!(e.to_string(), format!("'': {message}"));
        }
    }

    #[test]
    fn each_429_names_its_limit() {
        let limit =
            |kind: &str, code: Option<&str>, m: &str| match map_error(429, &body(kind, code, m)) {
                GatewayError::UpstreamLimited { limit, .. } => limit,
                other => panic!("{other:?}"),
            };
        for code in QUOTA_CODES {
            assert_eq!(
                limit("insufficient_quota", Some(code), "m"),
                UpstreamLimit::Quota
            );
        }
        assert_eq!(
            limit(
                "insufficient_quota",
                None,
                "You exceeded your current quota"
            ),
            UpstreamLimit::Quota
        );
        assert_eq!(
            limit("tokens", Some("rate_limit_exceeded"), "Rate limit reached"),
            UpstreamLimit::Rate
        );
        assert_eq!(
            limit("rate_limit_error", Some("slow_down"), "Slow down"),
            UpstreamLimit::Rate
        );
        assert_eq!(
            limit("invalid_request_error", None, "Resource Unavailable"),
            UpstreamLimit::FlexCapacity
        );
        assert_eq!(
            limit("invalid_request_error", Some("resource_unavailable"), "x"),
            UpstreamLimit::FlexCapacity
        );
    }

    #[test]
    fn the_quota_tells_the_client_not_to_retry() {
        let e = map_error(
            429,
            &body(
                "insufficient_quota",
                Some("credit_balance_exhausted"),
                "No credits.",
            ),
        );
        assert_eq!(e.http_status().as_u16(), 429);
        assert_eq!(e.kind(), "insufficient_quota");
        assert_eq!(e.openai_type(), "insufficient_quota");
        assert_eq!(e.anthropic_type(), "billing_error");
        // Anthropic's own status for billing, which its SDKs do not retry.
        assert_eq!(e.anthropic_status().as_u16(), 402);
        let m = e.to_string();
        assert!(m.contains("credit_balance_exhausted"), "{m}");
        assert!(m.contains("No credits."), "{m}");
        assert!(m.contains("retrying will not help"), "{m}");
    }

    /// The OpenAI and Responses wires send OpenAI's own code and type back;
    /// the family word is the log's kind, and the wire's only where OpenAI
    /// sent none.
    #[test]
    fn the_wire_keeps_openais_code_and_type() {
        let wire = |b: Vec<u8>| {
            let e = map_error(429, &b);
            let chat = e.to_openai_json()["error"].clone();
            assert_eq!(chat, e.to_responses_json()["error"]);
            (
                e.kind(),
                chat["code"].as_str().unwrap().to_string(),
                chat["type"].as_str().unwrap().to_string(),
            )
        };
        assert_eq!(
            wire(body(
                "insufficient_quota",
                Some("project_spend_limit_exceeded"),
                "m"
            )),
            (
                "insufficient_quota",
                "project_spend_limit_exceeded".into(),
                "insufficient_quota".into()
            )
        );
        assert_eq!(
            wire(body("tokens", Some("rate_limit_exceeded"), "m")),
            (
                "rate_limit_exceeded",
                "rate_limit_exceeded".into(),
                "tokens".into()
            )
        );
        assert_eq!(
            wire(body("requests", None, "Rate limit reached for requests")),
            (
                "rate_limit_exceeded",
                "rate_limit_exceeded".into(),
                "requests".into()
            )
        );
        // Neither sent: the family's words.
        let bare = serde_json::to_vec(&serde_json::json!({"error": {
            "message": "You exceeded your current quota", "code": "insufficient_quota",
        }}))
        .unwrap();
        assert_eq!(
            wire(bare),
            (
                "insufficient_quota",
                "insufficient_quota".into(),
                "insufficient_quota".into()
            )
        );
        let bare = serde_json::to_vec(&serde_json::json!({"error": {
            "message": "Resource Unavailable",
        }}))
        .unwrap();
        assert_eq!(
            wire(bare),
            (
                "resource_unavailable",
                "resource_unavailable".into(),
                "rate_limit_error".into()
            )
        );
    }

    /// The answer's rate-limit headers ride on the error, and nothing else
    /// of its headers does.
    #[test]
    fn the_rate_hints_are_the_retry_and_ratelimit_headers() {
        let mut h = reqwest::header::HeaderMap::new();
        for (k, v) in [
            ("retry-after", "2"),
            ("retry-after-ms", "1500"),
            ("x-ratelimit-remaining-tokens", "0"),
            ("x-ratelimit-reset-requests", "1s"),
            ("x-request-id", "req_1"),
            ("content-type", "application/json"),
        ] {
            h.insert(k, v.parse().unwrap());
        }
        let e =
            map_error(429, &body("tokens", Some("rate_limit_exceeded"), "m")).with_rate_hints(&h);
        let mut resp = axum::response::IntoResponse::into_response(());
        e.stamp_rate_hints(&mut resp);
        let got: Vec<_> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str(), v.to_str().unwrap()))
            .collect();
        assert_eq!(
            got,
            [
                ("retry-after", "2"),
                ("retry-after-ms", "1500"),
                ("x-ratelimit-remaining-tokens", "0"),
                ("x-ratelimit-reset-requests", "1s"),
            ]
        );
        // Any other error keeps none.
        let other = map_error(400, b"Bad Request").with_rate_hints(&h);
        assert!(other.rate_hints().is_none());
    }

    /// Not OpenAI's shape, or not one of these codes: the generic mapping.
    #[test]
    fn anything_else_is_the_generic_upstream_error() {
        for (status, b) in [
            (429, br#"{"error":"rate limited"}"#.to_vec()),
            (400, b"Bad Request".to_vec()),
            (
                401,
                body("invalid_request_error", None, "Incorrect API key"),
            ),
            (403, body("invalid_request_error", None, "Not allowed")),
            (500, body("server_error", None, "The server had an error")),
        ] {
            assert!(
                matches!(map_error(status, &b), GatewayError::Upstream { .. }),
                "{}",
                String::from_utf8_lossy(&b)
            );
        }
    }

    /// A 4xx in OpenAI's shape with no word of lmgw's is OpenAI's own object,
    /// relayed with its status.
    #[test]
    fn an_unclassified_4xx_is_relayed_whole() {
        let b = br#"{"error":{"message":"Unknown parameter: 'foo_bar'.","type":"invalid_request_error","param":"foo_bar","code":"unknown_parameter"}}"#;
        let e = map_error(400, b);
        assert_eq!(e.http_status().as_u16(), 400);
        assert_eq!(e.to_openai_json()["error"]["code"], "unknown_parameter");
        assert_eq!(e.to_openai_json()["error"]["param"], "foo_bar");
        assert_eq!(
            e.to_anthropic_json()["error"]["type"],
            "invalid_request_error"
        );
        let e = map_error(
            404,
            &body("invalid_request_error", Some("model_not_found"), "gone"),
        );
        assert_eq!(e.http_status().as_u16(), 404);
        assert_eq!(e.to_anthropic_json()["error"]["type"], "not_found_error");
        assert_eq!(e.kind(), "upstream");
    }
}
