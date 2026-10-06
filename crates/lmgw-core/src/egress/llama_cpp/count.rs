//! The token count on a llama.cpp server (llama egress design §3.1, §1.2): the
//! universal counter's `/tokenize {model, content}` plan, the `/tokenize`
//! request the llama.cpp-compatible route forwards, and the request builders
//! and readers of the gate's exact count, `/apply-template` then `/tokenize`.
//! The gate keeps the policy (`crate::gate::count`): what a failure means,
//! what media adds, when to refuse.
//!
//! **The flags of the gate's count.** Measured on this machine's llama-server
//! image: `/tokenize` with `parse_special` left at its default (`true`)
//! equalled a real completion's `usage.prompt_tokens` exactly;
//! `parse_special: false` overcounted by nearly double. So nothing here ever
//! sets it.
//!
//! [`prompt_tokenize_request`] **does** set `add_special: true`, which
//! `/tokenize` defaults to `false`: the completion path itself tokenizes every
//! prompt with `tokenize_input_prompts(vocab, mctx, prompt, /*add_special*/
//! true, /*parse_special*/ true)` (`server-context.cpp` — read in the
//! b062ba735 checkout on this machine, and the same `true, true` arguments in
//! 171e884's own copy), while `/tokenize` reads `json_value(body,
//! "add_special", false)`. So a model whose vocab adds a BOS token gets one on
//! the completion path and must be counted with it. On a vocab that adds
//! nothing the flag changes nothing — which is why the measurement above,
//! taken on such a model, could not tell the two apart. An off-by-one under
//! the pool's arithmetic is exactly the overflow the ledger exists to
//! prevent.

use serde_json::{json, Value};

use crate::config::Upstream;
use crate::egress::{apply_bearer_auth, CountPlan};

/// `POST /tokenize` at a llama-server's root (not under `/v1`), with the
/// upstream's bearer and extra headers, carrying `body` as given.
///
/// One builder for both callers, so the root derivation and the auth cannot
/// drift apart: the universal counter's `{model, content}` count
/// ([`crate::egress::Egress::build_count_tokens`]) and the llama.cpp-compatible
/// `/tokenize` route, which forwards the client's own object with only `model`
/// rewritten (api-docs design §5.3).
pub(crate) fn tokenize_request(
    http: &reqwest::Client,
    up: &Upstream,
    body: &Value,
) -> reqwest::RequestBuilder {
    let base = up.base().trim_end_matches("/v1");
    apply_bearer_auth(http.post(format!("{base}/tokenize")).json(body), up)
}

/// llama-server's count plan: `{model, content}` to its `/tokenize`
/// ([`tokenize_request`]), read back by
/// [`parse_tokenize_count`](crate::egress::openai_wire::parse_tokenize_count).
/// A single-model server ignores `model`, but forwarding it costs nothing and
/// lets a router-mode server pick the backend.
pub(crate) fn tokenize_count_plan(
    http: &reqwest::Client,
    up: &Upstream,
    model: &str,
    text: &str,
) -> CountPlan {
    CountPlan::Request(Box::new(tokenize_request(
        http,
        up,
        &json!({"model": model, "content": text}),
    )))
}

/// The gate's first step: `POST /apply-template` at `server_root` with the
/// exact chat body the egress is about to send, which the server renders
/// with the model's chat template and does not run.
pub(crate) fn apply_template_request(
    http: &reqwest::Client,
    server_root: &str,
    body: &Value,
) -> reqwest::RequestBuilder {
    http.post(format!("{server_root}/apply-template"))
        .json(body)
}

/// The rendered prompt in an `/apply-template` answer.
pub(crate) fn rendered_prompt(answer: &Value) -> Option<&str> {
    answer.get("prompt").and_then(Value::as_str)
}

/// The gate's second step: `POST /tokenize` at `server_root` with the
/// completion path's own flags (module doc): the server's `parse_special`
/// default, and `add_special: true`. `content` is a rendered chat prompt, or
/// one legacy prompt exactly as the completion path reads it — a string, or a
/// mixed array of strings and token ids, which `/tokenize` feeds through the
/// same `tokenize_mixed` (a BOS only where the prompt starts with text).
pub(crate) fn prompt_tokenize_request(
    http: &reqwest::Client,
    server_root: &str,
    content: &Value,
) -> reqwest::RequestBuilder {
    http.post(format!("{server_root}/tokenize"))
        .json(&json!({"content": content, "add_special": true}))
}

/// The number of token ids in a `/tokenize` answer.
pub(crate) fn token_count(answer: &Value) -> Option<u64> {
    answer
        .get("tokens")
        .and_then(Value::as_array)
        .map(|a| a.len() as u64)
}
