//! Building and sending the tester's own request (api-docs design §6.7).
//!
//! This never goes through `crate::api`: that module locks the whole
//! dashboard on a `401 session_required` (`lmgw-ui/src/api.rs:45-52`), which
//! is exactly what a deliberate "send with no credential" click is trying to
//! see happen. `gloo_net` is used directly, with `credentials` set per
//! identity — `SameOrigin` for the session cookie, `Omit` for a pasted key or
//! no credential at all, so the browser never attaches the dashboard's own
//! cookie behind a client key's back.

use gloo_net::http::{Method, RequestBuilder};
use serde_json::Value;

use super::identity::Identity;

/// One part of a multipart body (§6.6): a plain field, or a picked file —
/// only the file's *name* and its `web_sys::File` handle are ever held here.
#[derive(Clone)]
pub enum MultipartPart {
    Field(String, String),
    File(String, web_sys::File),
}

/// A request body shape the tester can hold. Sent verbatim — secret
/// redaction (§4.4 `x-lmgw-secret`) is a `curl.rs`-only concern: the real
/// send has to carry the real value, or the request would not do what the
/// tester showed.
#[derive(Clone)]
pub enum Body {
    None,
    Json {
        text: String,
    },
    Multipart {
        parts: Vec<MultipartPart>,
    },
    /// A raw (`Req::Raw`) body: one picked file, sent as-is.
    Raw {
        file: web_sys::File,
        content_type: String,
    },
}

/// Everything needed to send the request, or to describe it for "Copy as
/// curl" (`curl.rs`) without sending it.
#[derive(Clone)]
pub struct Prepared {
    pub method: String,
    /// Path plus query string, already percent-encoded — no origin.
    pub path: String,
    /// Extra headers only; `Authorization` is derived from `identity`.
    pub headers: Vec<(String, String)>,
    pub identity: Identity,
    /// The pasted key text — only ever used for `Identity::Key`, and only to
    /// build the real `Authorization` header for the actual send. `curl.rs`
    /// never sees it (§6.9: the pasted key is never in the curl).
    pub key: String,
    pub body: Body,
}

/// What came back, classified by `Content-Type` (§6.7) — the shape
/// `response_view.rs` renders.
pub enum Outcome {
    /// Headers arrived; the raw stream is handed back unread so the caller
    /// can drive `stream::SseSplitter` and grow `draft.sse_frames` live
    /// (§6.7) — this module only classifies, it doesn't itself know about
    /// per-operation state.
    Sse(web_sys::ReadableStream),
    Json(Value),
    /// JSON parse failed, or a non-JSON `text/*` (not csv).
    Text(String),
    /// No body at all (a `204`, a `HEAD`): nothing to play or download.
    Empty,
    /// Anything else, or `Content-Disposition: attachment`: bytes behind a
    /// Blob URL, revoked on replacement or cleanup (`revoke_blob`).
    Blob {
        url: String,
        content_type: String,
        filename: String,
        size: usize,
    },
    TransportError(String),
}

pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub outcome: Outcome,
}

fn header_map(resp: &gloo_net::http::Response) -> Vec<(String, String)> {
    resp.headers().entries().collect()
}

fn content_type(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// `Content-Disposition`'s `filename=`, quoted or not, ending at its `;`.
fn attachment_filename(headers: &[(String, String)], fallback: &str) -> String {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-disposition"))
        .and_then(|(_, v)| disposition_filename(v))
        .unwrap_or_else(|| fallback.to_string())
}

fn disposition_filename(v: &str) -> Option<String> {
    v.split(';')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("filename="))
        .map(|f| f.trim().trim_matches('"').to_string())
        .filter(|f| !f.is_empty())
}

/// Why `Headers.set(name, value)` would throw, if it would: gloo-net
/// `unwrap_throw`s that, which unwinds the wasm module mid-task. A name is
/// an RFC 9110 token; a value a Latin-1 byte string with no CR, LF or NUL.
pub fn header_problem(name: &str, value: &str) -> Option<String> {
    let token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
    if name.is_empty() || !name.chars().all(token) {
        return Some(format!("'{name}' is not a valid header name"));
    }
    if value
        .chars()
        .any(|c| c as u32 > 0xFF || matches!(c, '\r' | '\n' | '\0'))
    {
        return Some(format!(
            "the value of '{name}' has characters a header cannot carry"
        ));
    }
    None
}

fn method(m: &str) -> Method {
    match m.to_ascii_uppercase().as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "PATCH" => Method::PATCH,
        "DELETE" => Method::DELETE,
        "HEAD" => Method::HEAD,
        "OPTIONS" => Method::OPTIONS,
        other => other.parse().unwrap_or(Method::GET),
    }
}

/// Build the `gloo_net` request from `p` and send it, classifying the answer
/// by content type (§6.7). `fallback_name` names a download whose response
/// says no `filename` (`<operationId>.bin`).
pub async fn send(p: &Prepared, signal: &web_sys::AbortSignal, fallback_name: &str) -> Answer {
    // The editor already refuses to send these; this is the backstop, since
    // gloo-net's `header()` throws on them instead of returning an error.
    let auth = format!("Bearer {}", p.key);
    let auth_header = (p.identity == Identity::Key && !p.key.is_empty())
        .then(|| ("Authorization".to_string(), auth.clone()));
    let raw_type = match &p.body {
        Body::Raw { content_type, .. } => Some(("Content-Type".to_string(), content_type.clone())),
        _ => None,
    };
    let bad = p
        .headers
        .iter()
        .chain(auth_header.iter())
        .chain(raw_type.iter())
        .find_map(|(n, v)| header_problem(n, v));
    if let Some(problem) = bad {
        return transport_error(&problem);
    }
    let mut builder = RequestBuilder::new(&p.path)
        .method(method(&p.method))
        .abort_signal(Some(signal))
        .credentials(match p.identity {
            Identity::Session => web_sys::RequestCredentials::SameOrigin,
            Identity::Key | Identity::None => web_sys::RequestCredentials::Omit,
        });
    if p.identity == Identity::Key && !p.key.is_empty() {
        builder = builder.header("Authorization", &auth);
    }
    for (name, value) in &p.headers {
        builder = builder.header(name, value);
    }

    let built = match &p.body {
        Body::None => builder.build(),
        Body::Json { text, .. } => builder
            .header("Content-Type", "application/json")
            .body(text.clone()),
        Body::Multipart { parts, .. } => match web_sys::FormData::new() {
            Ok(form) => {
                for part in parts {
                    match part {
                        MultipartPart::Field(name, value) => {
                            let _ = form.append_with_str(name, value);
                        }
                        MultipartPart::File(name, file) => {
                            let _ = form.append_with_blob_and_filename(name, file, &file.name());
                        }
                    }
                }
                builder.body(form)
            }
            Err(_) => return transport_error("FormData is not available"),
        },
        Body::Raw { file, content_type } => builder
            .header("Content-Type", content_type)
            .body(file.clone()),
    };

    let request = match built {
        Ok(r) => r,
        Err(e) => return transport_error(&e.to_string()),
    };

    match request.send().await {
        Ok(resp) => {
            let status = resp.status();
            let headers = header_map(&resp);
            let ctype = content_type(&headers);
            let outcome = classify(resp, &headers, &ctype, fallback_name).await;
            Answer {
                status,
                headers,
                outcome,
            }
        }
        Err(e) => transport_error(&e.to_string()),
    }
}

fn transport_error(msg: &str) -> Answer {
    Answer {
        status: 0,
        headers: Vec::new(),
        outcome: Outcome::TransportError(msg.to_string()),
    }
}

async fn classify(
    resp: gloo_net::http::Response,
    headers: &[(String, String)],
    ctype: &str,
    fallback_name: &str,
) -> Outcome {
    let is_attachment = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("content-disposition")
            && v.to_ascii_lowercase().contains("attachment")
    });
    let base = ctype
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if !is_attachment && base == "text/event-stream" {
        return match resp.body() {
            Some(stream) => Outcome::Sse(stream),
            None => Outcome::Text(String::new()),
        };
    }
    if !is_attachment && (base == "application/json" || base.ends_with("+json")) {
        return match resp.text().await {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) => Outcome::Json(v),
                Err(_) => Outcome::Text(text),
            },
            Err(e) => Outcome::TransportError(e.to_string()),
        };
    }
    if !is_attachment && base.starts_with("text/") && base != "text/csv" {
        return match resp.text().await {
            Ok(text) => Outcome::Text(text),
            Err(e) => Outcome::TransportError(e.to_string()),
        };
    }
    match resp.binary().await {
        Ok(bytes) if bytes.is_empty() => Outcome::Empty,
        Ok(bytes) => match blob_url(&bytes, ctype) {
            Some(url) => Outcome::Blob {
                url,
                content_type: ctype.to_string(),
                filename: attachment_filename(headers, fallback_name),
                size: bytes.len(),
            },
            None => {
                Outcome::TransportError("could not build a Blob URL for the response".to_string())
            }
        },
        Err(e) => Outcome::TransportError(e.to_string()),
    }
}

/// A Blob URL over `bytes` — the same construction `pages::audio_lab` uses.
fn blob_url(bytes: &[u8], mime: &str) -> Option<String> {
    let arr = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&arr);
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

/// Revoke a Blob URL — on replacement (a new send) and on page cleanup
/// (§6.7 "Blob URLs are revoked").
pub fn revoke_blob(url: &str) {
    let _ = web_sys::Url::revoke_object_url(url);
}

/// A fresh `AbortController` for one in-flight request.
pub fn new_abort() -> web_sys::AbortController {
    web_sys::AbortController::new().expect("AbortController is always constructible")
}

pub fn now_ms() -> f64 {
    js_sys::Date::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_names_must_be_tokens_and_values_latin1_without_line_breaks() {
        assert!(header_problem("x-lmgw-reasoning", "off").is_none());
        assert!(header_problem("X-Custom_1", "café").is_none());
        assert!(header_problem("Bad Header", "v").is_some());
        assert!(header_problem("", "v").is_some());
        assert!(header_problem("x-note", "日本").is_some());
        assert!(header_problem("x-note", "a\rb").is_some());
    }

    #[test]
    fn filename_is_read_up_to_its_parameter_end() {
        assert_eq!(
            disposition_filename("attachment; filename=\"usage.csv\"; size=12"),
            Some("usage.csv".to_string())
        );
        assert_eq!(
            disposition_filename("attachment;filename=corpus.sqlite"),
            Some("corpus.sqlite".to_string())
        );
        assert_eq!(disposition_filename("attachment"), None);
    }
}
