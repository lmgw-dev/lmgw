//! The egress corpus (llama-egress design §9.1, WP0): what the egress layer
//! sends and reads, case by case, captured on the code before the llama.cpp
//! egress existed, so every later step proves itself against it.
//!
//! Each case runs on both abstract upstreams, [`Up::Llama`] and
//! [`Up::Generic`], and writes `tests/fixtures/egress/<case>__<upstream>.json`:
//! the request's method, URL and headers (secrets as `<set>`), the body as its
//! exact serialized string (and parsed beside it, for a readable diff), and
//! where the case is about them, the count plan, `map_error` on recorded error
//! bodies, the parsed completion, and the deltas decoded from recorded SSE.
//! `LMGW_BLESS=1` rewrites the files (`support/golden.rs`).
//!
//! **Built to survive the protocol switch.** No fixture names a protocol:
//! the upstream is `llama` or `generic` in the file name only, and the egress
//! is always the one `for_protocol(up.protocol)` picks. Three seams hold the
//! rest, one place each:
//! - [`LLAMA_SPELLING`] is how a llama.cpp upstream is spelled —
//!   `LlamaCpp` + `LlamaServer` since WP2a (`Openai` + `LlamaServer` when
//!   the corpus was captured). The
//!   route corpus (`route_golden.rs`) stores its external row with it too.
//! - [`counted_body`] is the body the gate counts for a chat send.
//! - [`exceed_context`] is the reader of llama-server's context refusal.
//!
//! WP2a proved the old spelling (the OpenAI egress's kind branches) and the
//! new one (the llama.cpp egress) byte-equal on every case; since WP2c the
//! OpenAI egress has no kind branches, and the old spelling means a generic
//! OpenAI row.

mod chat;
mod reasoning;
mod wire;

use std::collections::BTreeSet;
use std::sync::{Arc, LazyLock};

use lmgw_core::config::{LlamaRoute, Protocol, ToolImages, Upstream, UpstreamKind};
use lmgw_core::egress::for_protocol;
use lmgw_core::egress::llama_cpp::props::LlamaFacts;
use lmgw_core::ir::{ChatRequest, Params};
use serde_json::{json, Value};

use crate::support::golden;

/// The fixtures' directory under `tests/fixtures`.
const DIR: &str = "egress";

/// The model every case asks its upstream for.
pub(crate) const MODEL: &str = "the-model";

// ---------------------------------------------------------------------------
// The upstreams
// ---------------------------------------------------------------------------

/// How a llama.cpp upstream (llama-server, ik_llama.cpp) is spelled in a row
/// — the one line WP2a changed from `(Protocol::Openai,
/// UpstreamKind::LlamaServer)`.
pub(crate) const LLAMA_SPELLING: (Protocol, UpstreamKind) =
    (Protocol::LlamaCpp, UpstreamKind::LlamaServer);

/// The abstract upstream a case runs on: its name in the fixture's file
/// name, and the row it stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Up {
    Llama,
    Generic,
}

impl Up {
    pub(crate) const ALL: [Up; 2] = [Up::Llama, Up::Generic];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Up::Llama => "llama",
            Up::Generic => "generic",
        }
    }

    pub(crate) fn upstream(self) -> Upstream {
        match self {
            Up::Llama => llama_upstream(),
            Up::Generic => generic_upstream(),
        }
    }
}

/// An external llama.cpp row with a key and an extra header, spelled as
/// [`LLAMA_SPELLING`], whose server is known to see: its route carries the
/// facts `vision: true` (no `video`) and the frozen decision that tool images
/// go (llama egress design §3.2, §8.2). Every case that is not about a tool
/// image renders as it did with nothing known, so the corpus proves that
/// known facts with vision and no tool image give today's bytes; the tool
/// image cases show the images that go, and the reasons of those that do not.
pub(crate) fn llama_upstream() -> Upstream {
    let (protocol, kind) = LLAMA_SPELLING;
    Upstream {
        id: 1,
        name: "llama".into(),
        protocol,
        kind,
        base_url: "http://llama.test/v1".into(),
        api_key: Some("sk-llama".into()),
        extra_headers: vec![("x-extra".into(), "one".into())],
        timeout_ms: 30_000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: Some(Arc::new(LlamaRoute {
            facts: Arc::new(LlamaFacts {
                vision: Some(true),
                ..LlamaFacts::default()
            }),
            tool_images: ToolImages::Allowed,
        })),
    }
}

/// A generic OpenAI-compatible provider with a key and an extra header.
pub(crate) fn generic_upstream() -> Upstream {
    Upstream {
        id: 2,
        name: "generic".into(),
        protocol: Protocol::Openai,
        kind: UpstreamKind::Generic,
        base_url: "http://generic.test/v1".into(),
        api_key: Some("sk-generic".into()),
        extra_headers: vec![("x-extra".into(), "one".into())],
        timeout_ms: 30_000,
        enabled: true,
        expose_all: false,
        expose_prefix: String::new(),
        supports_responses: false,
        llama: None,
    }
}

/// `up` with `path` after its host instead of `/v1` (the count root's
/// derivation). Changes only the base URL, so it keeps `up`'s spelling.
pub(crate) fn rebased(up: &Upstream, path: &str) -> Upstream {
    let mut up = up.clone();
    up.base_url = format!("http://{}.test{path}", up.name);
    up
}

/// `up` without its key and extra header, as a managed row is. Keeps `up`'s
/// spelling.
pub(crate) fn keyless(up: &Upstream) -> Upstream {
    let mut up = up.clone();
    up.api_key = None;
    up.extra_headers.clear();
    up
}

// ---------------------------------------------------------------------------
// The seams later work packages move
// ---------------------------------------------------------------------------

/// The chat body the gate counts for a send on `up` (`gate/fit.rs`,
/// `gate/send.rs`, `proxy/count_messages.rs`): the llama.cpp egress's
/// `chat_body` — the gate counts only llama.cpp rows. A generic row is never
/// counted; its case compares the OpenAI egress's own body. Every chat case
/// asserts it is byte for byte the posted body.
pub(crate) fn counted_body(
    ir: &ChatRequest,
    model: &str,
    params: &Params,
    stream: bool,
    up: &Upstream,
) -> Value {
    match up.protocol {
        Protocol::LlamaCpp => {
            lmgw_core::egress::llama_cpp::chat_body(ir, model, params, stream, up)
        }
        _ => lmgw_core::egress::openai::chat_body(ir, model, params, stream),
    }
}

/// llama-server's context refusal read out of an error body, as the local
/// sends read it (`gate/send.rs`), or `null` for any other body.
pub(crate) fn exceed_context(body: &[u8]) -> Value {
    match lmgw_core::egress::llama_cpp::parse_exceed_context(body) {
        Some(e) => json!({"n_prompt_tokens": e.n_prompt_tokens, "n_ctx": e.n_ctx}),
        None => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Running a case
// ---------------------------------------------------------------------------

/// One case: a name and what it records on a given upstream.
pub(crate) struct Case {
    pub name: &'static str,
    pub run: fn(&Upstream) -> Value,
}

/// Every case of the corpus.
pub(crate) fn corpus() -> Vec<Case> {
    let mut out = chat::cases();
    out.extend(reasoning::cases());
    out.extend(wire::cases());
    out
}

/// What `case` records on `up`, as the fixture's bytes.
pub(crate) fn render(case: &Case, up: &Upstream) -> String {
    golden::to_fixture(&(case.run)(up))
}

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

pub(crate) fn http() -> &'static reqwest::Client {
    &HTTP
}

/// A built request as the fixture records it.
pub(crate) fn request_json(rb: reqwest::RequestBuilder) -> Value {
    let req = rb.build().expect("every egress request builds");
    let (body, body_json) = match req.body().and_then(|b| b.as_bytes()) {
        Some(bytes) => golden::body(bytes),
        None => (Value::Null, Value::Null),
    };
    json!({
        "method": req.method().as_str(),
        "url": req.url().as_str(),
        "headers": golden::headers(
            req.headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("<binary>").to_string())),
            &[],
        ),
        "body": body,
        "body_json": body_json,
    })
}

/// A chat request: what `build_chat` posts, after checking that the gate's
/// count would render exactly these bytes.
pub(crate) fn chat_request(
    up: &Upstream,
    ir: &ChatRequest,
    params: &Params,
    stream: bool,
) -> Value {
    let rb = for_protocol(up.protocol)
        .build_chat(http(), up, MODEL, ir, params, stream)
        .expect("every chat case builds");
    let recorded = request_json(rb);
    let counted = serde_json::to_string(&counted_body(ir, MODEL, params, stream, up)).unwrap();
    assert_eq!(
        recorded["body"].as_str(),
        Some(counted.as_str()),
        "the counted body is not the posted body"
    );
    json!({ "request": recorded })
}

/// An IR request of `messages` (IR-shaped JSON), with no tools and no
/// passthrough.
pub(crate) fn ir(messages: Value) -> ChatRequest {
    serde_json::from_value(json!({"model_alias": "alias", "messages": messages}))
        .expect("an IR-shaped request")
}

/// `Params` from their own serde shape.
pub(crate) fn params(v: Value) -> Params {
    serde_json::from_value(v).expect("Params-shaped JSON")
}

/// Text content of one IR message.
pub(crate) fn msg(role: &str, text: &str) -> Value {
    json!({"role": role, "content": [{"type": "text", "text": text}]})
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

#[test]
fn the_egress_corpus_matches_its_fixtures() {
    let mut written = BTreeSet::new();
    let mut failures = Vec::new();
    for case in corpus() {
        for up in Up::ALL {
            let name = format!("{}__{}", case.name, up.name());
            assert!(written.insert(name.clone()), "two cases named {name}");
            if let Err(e) = golden::check(DIR, &name, &render(&case, &up.upstream())) {
                failures.push(e);
            }
        }
    }
    failures.extend(golden::stale(DIR, &written));
    golden::assert_all("egress", written.len(), failures);
}

/// The corpus survives the protocol switch only if no fixture names a
/// protocol: the abstract upstream names are all a file may carry.
#[test]
fn no_egress_fixture_names_a_protocol() {
    for case in corpus() {
        for up in Up::ALL {
            let got = render(&case, &up.upstream());
            for word in ["openai", "llama_cpp", "llama_server", "anthropic", "gemini"] {
                assert!(
                    !got.to_ascii_lowercase().contains(word),
                    "{}__{} names '{word}'",
                    case.name,
                    up.name()
                );
            }
        }
    }
}
