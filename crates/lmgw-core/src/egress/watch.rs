//! A test's view of every request body an egress builds (api-compat design,
//! level 1): the integration suite installs one function that checks each
//! body against the provider's own pinned schema, so every test that sends
//! to a fake upstream checks what lmgw sent without saying so.
//!
//! Nothing is installed outside the tests, and then [`built`] is one atomic
//! load: no copy of the body, no clone of the builder.

use std::sync::OnceLock;

use crate::config::{Protocol, Upstream};

/// One request an egress built, as it goes on the wire.
pub struct Built<'a> {
    /// The upstream's protocol: which provider's schema the body answers to.
    pub protocol: Protocol,
    /// The URL it is posted to (the route within the protocol: chat,
    /// Responses, embeddings, count tokens).
    pub url: &'a str,
    /// The body's bytes.
    pub body: &'a [u8],
    /// The client's own top-level keys (an OpenAI-shaped request's
    /// passthrough; a Messages request's `context_management` on the
    /// Anthropic egress), as it sent them. A body key whose value equals the
    /// client's went out verbatim, and the client answers for it; lmgw
    /// writes into some of these keys (`reasoning`), and then the body's
    /// value differs.
    pub passthrough: &'a serde_json::Map<String, serde_json::Value>,
}

/// On a response lmgw relays as an upstream answered it (the native
/// `/v1/responses` passthrough): what a test's fake upstream wrote, not what
/// lmgw made. An extension, never on the wire.
#[derive(Debug, Clone, Copy)]
pub struct Relayed;

static WATCHER: OnceLock<fn(&Built<'_>)> = OnceLock::new();

/// Installs `f` for every request built from now on in this process. The
/// first call wins; whether it was this one is the answer.
#[doc(hidden)]
pub fn install(f: fn(&Built<'_>)) -> bool {
    WATCHER.set(f).is_ok()
}

/// Shows `rb` to the installed watcher, if any, and hands it back unchanged.
pub(crate) fn built(
    up: &Upstream,
    passthrough: &serde_json::Map<String, serde_json::Value>,
    rb: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    let Some(f) = WATCHER.get() else {
        return rb;
    };
    // A body that is not one buffer (none is today: every egress posts JSON)
    // has nothing to show.
    if let Some(Ok(req)) = rb.try_clone().map(reqwest::RequestBuilder::build) {
        if let Some(body) = req.body().and_then(reqwest::Body::as_bytes) {
            f(&Built {
                protocol: up.protocol,
                url: req.url().as_str(),
                body,
                passthrough,
            });
        }
    }
    rb
}
