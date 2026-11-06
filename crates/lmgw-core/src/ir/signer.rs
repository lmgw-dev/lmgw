//! Which provider an Anthropic signature, verbatim block or citation lmgw
//! read from an answer goes back to (gateway design §6.1, "Where a
//! signature goes", changed 2026-11-06): the scope of the row that
//! answered, its base URL normalized: the origin (scheme, host and port,
//! lower-case, the scheme's own port dropped) and the path, without
//! trailing slashes or the `/v1` the Anthropic egress adds itself. No
//! query, user or password, key, header or model is part of it.
//!
//! Anthropic binds a thinking block's `signature` (and a `redacted_thinking`
//! block's `data`, a web search citation's `encrypted_index`) to Anthropic
//! itself, not to the key, the model or a header: its signatures work
//! across the Claude API, Bedrock and Vertex, and a block the model being
//! asked cannot read (another model's, another account's on Claude Sonnet
//! 5.5 and Claude Haiku 5.5) is dropped by the API, unbilled, and the
//! request succeeds. Anthropic asks that every block goes back unchanged,
//! after a model switch too, as stripping one can itself earn a 400. So a
//! row at the same scope gets the block whatever its key, its headers or
//! the model; a row elsewhere (an Anthropic-compatible provider of another
//! company, a proxy) is not sent it, as its server cannot read it, and a
//! block another server signed is refused by Anthropic with 400 "Invalid
//! `signature` in `thinking` block".
//!
//! The path is part of the scope because a gateway in front of several
//! providers tells them apart by path at one origin: Cloudflare's AI Gateway
//! (`https://gateway.ai.cloudflare.com/v1/<account>/<gateway>/anthropic`),
//! a LiteLLM proxy on `localhost:4000` with one path per provider. Two rows
//! at `https://api.anthropic.com` (with or without `/v1`, any key or model)
//! share one. lmgw cannot tell two scopes that reach the same provider (a
//! proxy in front of Anthropic) yet, so a block never crosses scopes.
//!
//! The scope is no secret (it is the row's address, without any user or
//! password the URL might hold and without its query) and a record keeps it
//! in clear.

use serde::{Deserialize, Serialize};

use crate::config::Upstream;

/// The provider a signed block goes back to (module doc). The default, an
/// empty scope, is a decoder no send site made (`Egress::new_decoder` in a
/// test) and matches no row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Signer(String);

impl Signer {
    /// The signer `scope` names, as [`Self::of`] would read it from a row's
    /// base URL.
    pub fn new(scope: impl Into<String>) -> Self {
        Self(scope.into())
    }

    /// The signer of `up`'s answers: its base URL normalized (module doc):
    /// `https://api.anthropic.com` for `https://API.anthropic.com:443/v1/`,
    /// `https://gateway.ai.cloudflare.com/v1/acct/gw/anthropic` for a
    /// Cloudflare AI Gateway row. The `/v1` is trimmed as the Anthropic
    /// egress trims it before it adds `/v1/messages`, so two rows that send
    /// to one URL share a scope. A base URL that does not parse is its own
    /// scope, as written.
    pub fn of(up: &Upstream) -> Self {
        let base = up.base();
        match reqwest::Url::parse(base) {
            Ok(u) if u.origin().is_tuple() => {
                let path = u
                    .path()
                    .trim_end_matches('/')
                    .trim_end_matches("/v1")
                    .trim_end_matches('/');
                Self(format!("{}{path}", u.origin().ascii_serialization()))
            }
            _ => Self(base.to_string()),
        }
    }

    /// The scope, as a log line names it.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether a block this signer's row answered with goes to `row`.
    pub fn reaches(&self, row: &Signer) -> bool {
        !self.0.is_empty() && self == row
    }
}

/// Whether a block that carries `signer` goes to `row`: a client's own block
/// (no signer) goes to every Anthropic-protocol row as the client wrote it,
/// one lmgw read from an answer only to a row at its scope (module doc).
pub fn goes_to(signer: Option<&Signer>, row: &Signer) -> bool {
    signer.is_none_or(|s| s.reaches(row))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(base: &str, key: &str) -> Upstream {
        Upstream {
            id: 4,
            name: "anthropic".into(),
            protocol: crate::config::Protocol::Anthropic,
            kind: crate::config::UpstreamKind::Generic,
            base_url: base.into(),
            api_key: Some(key.into()),
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            llama: None,
        }
    }

    /// The key and a header are no part of it; the host, the port and the
    /// path are, without trailing slashes, the `/v1`, a query or a user.
    #[test]
    fn the_signer_is_the_rows_normalized_base_url() {
        let one = Signer::of(&row("https://api.anthropic.com/v1/", "sk-a"));
        assert_eq!(one.as_str(), "https://api.anthropic.com");
        let other_key = Signer::of(&Upstream {
            extra_headers: vec![("anthropic-beta".into(), "x".into())],
            ..row("https://API.anthropic.com:443", "sk-b")
        });
        assert!(one.reaches(&other_key));
        assert!(!one.reaches(&Signer::of(&row(
            "https://api.example.com/anthropic",
            "sk-a"
        ))));
        assert!(!one.reaches(&Signer::of(&row("https://api.anthropic.com:8443", "sk-a"))));
        assert_eq!(
            Signer::of(&row("https://user:pw@api.anthropic.com/?k=1", "k")).as_str(),
            "https://api.anthropic.com",
            "no user, password or query"
        );
        // One origin, two providers behind a gateway's paths.
        let cf = |p: &str| {
            Signer::of(&row(
                &format!("https://gateway.ai.cloudflare.com/v1/acct/gw/{p}/"),
                "k",
            ))
        };
        assert_eq!(
            cf("anthropic").as_str(),
            "https://gateway.ai.cloudflare.com/v1/acct/gw/anthropic"
        );
        assert!(!cf("anthropic").reaches(&cf("other")));
        let litellm = |p: &str| Signer::of(&row(&format!("http://localhost:4000{p}"), "k"));
        assert!(!litellm("/anthropic").reaches(&litellm("/claude-proxy")));
        assert!(litellm("/anthropic").reaches(&litellm("/anthropic/v1")));
        assert!(!Signer::default().reaches(&Signer::default()), "no row");
        assert!(goes_to(None, &Signer::default()), "a client's block");
    }
}
