//! The two GET handlers (api-docs design §1, §4.11): `admin_json` behind
//! `Cap::Admin` at `GET /api/openapi.json`, `v1_json` behind `Cap::Inference`
//! at `GET /v1/openapi.json`. Neither reads `SharedState` — both documents
//! are pure functions of the crate's own static registries.
//!
//! Each document is serialized once, into a [`Bytes`] every request shares
//! (review R2 #1: the handlers used to clone the whole `Value` tree and
//! re-serialize it per request). The ETag is a digest of those bytes, so a
//! browser revalidating under `no-cache` gets a `304` and no body until the
//! gateway is rebuilt. Both routes carry tower-http's compression layer
//! (`server.rs`, `web/api.rs`) — applied nowhere else in the gateway, and
//! worth it for a JSON document this size; a client that sends no
//! `Accept-Encoding` gets it plain.

use std::sync::OnceLock;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::build::{admin_doc, v1_doc};

/// One served document: its bytes and the validator that names them.
struct Served {
    body: Bytes,
    etag: HeaderValue,
}

impl Served {
    fn of(doc: &Value) -> Self {
        let body = serde_json::to_vec(doc).expect("a serde_json::Value always serializes");
        let digest = Sha256::digest(&body);
        // Weak: the compression layer can hand the same document out gzipped
        // or not, and a strong validator promises byte-identical bodies.
        let etag = format!("W/\"{}\"", hex::encode(&digest[..12]));
        Served {
            body: Bytes::from(body),
            etag: HeaderValue::from_str(&etag).expect("hex digits are a valid header value"),
        }
    }

    fn respond(&self, headers: &HeaderMap) -> Response {
        let mut response = if if_none_match_hits(headers, &self.etag) {
            StatusCode::NOT_MODIFIED.into_response()
        } else {
            let mut r = Body::from(self.body.clone()).into_response();
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            r
        };
        let h = response.headers_mut();
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        h.insert(header::ETAG, self.etag.clone());
        response
    }
}

/// Weak comparison (RFC 9110 §13.1.2): any listed tag equal to `etag` once
/// both drop their `W/` prefix, or `*`.
fn if_none_match_hits(headers: &HeaderMap, etag: &HeaderValue) -> bool {
    let Some(ours) = etag.to_str().ok().map(|t| t.trim_start_matches("W/")) else {
        return false;
    };
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || tag.trim_start_matches("W/") == ours)
}

static ADMIN: OnceLock<Served> = OnceLock::new();
static V1: OnceLock<Served> = OnceLock::new();

pub async fn admin_json(headers: HeaderMap) -> Response {
    ADMIN
        .get_or_init(|| Served::of(admin_doc()))
        .respond(&headers)
}

pub async fn v1_json(headers: HeaderMap) -> Response {
    V1.get_or_init(|| Served::of(v1_doc())).respond(&headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_inm(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn if_none_match_is_a_weak_comparison_over_a_list() {
        let etag = HeaderValue::from_static("W/\"abc\"");
        assert!(if_none_match_hits(&with_inm("W/\"abc\""), &etag));
        assert!(if_none_match_hits(&with_inm("\"abc\""), &etag));
        assert!(if_none_match_hits(&with_inm("\"x\", W/\"abc\""), &etag));
        assert!(if_none_match_hits(&with_inm("*"), &etag));
        assert!(!if_none_match_hits(&with_inm("\"abcd\""), &etag));
        assert!(!if_none_match_hits(&HeaderMap::new(), &etag));
    }
}
