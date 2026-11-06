//! (c): generated IR conversations ([`super::conversations`]) through every
//! egress, streamed and not, each body checked against its provider's
//! schema ([`crate::common::api_compat::egress::check`]).

use lmgw_core::config::{Protocol, Upstream, UpstreamKind};
use lmgw_core::egress::for_protocol;
use lmgw_core::egress::watch::Built;
use lmgw_core::ir::ChatRequest;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestRng, TestRunner};

use super::conversations::conversation;
use crate::common::api_compat::{egress, ledger};
use crate::egress_golden::{generic_upstream, llama_upstream, platform_upstream};

fn cloud(protocol: Protocol, base: &str) -> Upstream {
    Upstream {
        id: 9,
        name: format!("{protocol:?}"),
        protocol,
        kind: UpstreamKind::Generic,
        base_url: base.into(),
        api_key: Some("sk-test".into()),
        extra_headers: vec![],
        timeout_ms: 30_000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

/// Every egress, on the upstream rows the golden corpus speaks to: the
/// OpenAI platform, a compatible provider, llama-server (whose tool images
/// go), Anthropic and Gemini.
fn upstreams() -> Vec<Upstream> {
    vec![
        platform_upstream(),
        generic_upstream(),
        llama_upstream(),
        cloud(Protocol::Anthropic, "https://anthropic.test"),
        cloud(Protocol::Gemini, "https://gemini.test"),
    ]
}

/// The refusals an egress makes on purpose, a visible error to the client
/// rather than a body: (protocol, what the error says). Every other refusal
/// fails the property, since every generated conversation is one an ingress
/// can produce.
const KNOWN_REFUSALS: &[(Protocol, &str)] = &[
    // Gemini's inline data takes bytes; lmgw fetches no image URL for it yet.
    (Protocol::Gemini, "image URLs for gemini upstreams"),
];

/// The violations of `ir` sent to `up`, one line each, the known ones
/// (`known::KNOWN`) left out; an egress that refused to build the request
/// is one too, unless the refusal is in [`KNOWN_REFUSALS`].
fn violations(up: &Upstream, ir: &ChatRequest, stream: bool) -> Vec<String> {
    let built = ledger::quietly(|| {
        for_protocol(up.protocol).build_chat(
            &reqwest::Client::new(),
            up,
            "m",
            ir,
            &ir.params,
            stream,
        )
    });
    let req = match built.map(reqwest::RequestBuilder::build) {
        Ok(Ok(req)) => req,
        Ok(Err(e)) => return vec![format!("the request does not build: {e}")],
        Err(e) => {
            let text = e.to_string();
            if KNOWN_REFUSALS
                .iter()
                .any(|(p, why)| *p == up.protocol && text.contains(why))
            {
                return vec![];
            }
            return vec![format!("the egress refused the conversation: {text}")];
        }
    };
    let body = req
        .body()
        .and_then(reqwest::Body::as_bytes)
        .expect("an egress posts a buffered JSON body");
    let out = egress::check(&Built {
        protocol: up.protocol,
        url: req.url().as_str(),
        body,
        passthrough: &ir.passthrough,
    });
    assert!(
        out.entry.is_some(),
        "{}: the check covers no route at {}",
        up.name,
        req.url()
    );
    out.violations.iter().map(ToString::to_string).collect()
}

/// The property: whatever the conversation, every egress sends what its
/// provider's schema takes.
#[test]
fn every_egress_sends_what_its_provider_takes_for_any_conversation() {
    let ups = upstreams();
    // Deterministic in the gate: a fixed seed, so a red run is a real
    // regression and reproduces. `LMGW_PROPTEST_SEED` (a number) explores
    // another seed, `LMGW_PROPTEST_CASES` runs more cases; a failing seed is
    // printed with the failure.
    let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
    let seed = env("LMGW_PROPTEST_SEED").unwrap_or(0x1A57_C0DE);
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    let mut runner = TestRunner::new_with_rng(
        Config {
            cases: env("LMGW_PROPTEST_CASES").map_or(256, |c| c as u32),
            failure_persistence: None,
            ..Config::default()
        },
        TestRng::from_seed(RngAlgorithm::ChaCha, &bytes),
    );
    let result = runner.run(&(conversation(), any::<bool>()), |(ir, stream)| {
        for up in &ups {
            let bad = violations(up, &ir, stream);
            if !bad.is_empty() {
                return Err(TestCaseError::fail(format!(
                    "{} ({:?}, stream {stream}):\n  {}",
                    up.name,
                    up.protocol,
                    bad.join("\n  ")
                )));
            }
        }
        Ok(())
    });
    if let Err(e) = result {
        panic!("{e}\n(LMGW_PROPTEST_SEED={seed})");
    }
}
