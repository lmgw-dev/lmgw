//! What one embedding input may hold, and how its tokens are counted.
//!
//! `chunk_tokens` is checked against the **per-input limit** of the embedding
//! model, which is not always its context:
//!
//! - a **local aux** llama-server model: lmgw passes no physical batch for it
//!   (an aux row has no batch field), so llama-server's own default `-ub 512`
//!   is the real limit of a non-causal embedder — unless the row's extra args
//!   carry `-ub` / `-b`, which are then what it was started with. The limit is
//!   `min(context, ubatch, batch)`. A **last-token pooling** model (a causal
//!   embedder) is the exception: llama-server can split its input across
//!   batches, so only its context bounds it.
//! - a **reranker** is the same kind of model (a non-causal one, always):
//!   [`input_limit`] answers for its alias too, and there the limit bounds the
//!   whole query + document pair.
//! - a **cloud** embedding alias: the context its upstream catalog lists;
//!   unknown is said, never guessed.
//!
//! Tokens are counted with the embedding model's own tokenizer where the
//! universal counter ([`crate::proxy::count_tokens_inner`]) reaches it
//! (llama-server `/tokenize`, Gemini `countTokens`, the tiktoken encoding of
//! a real OpenAI model) and with tiktoken `o200k_base` otherwise — which the
//! base's view says, since that count is a guess for a model that is not
//! OpenAI's.

use std::sync::Arc;

use quickdoc_core::embed::TokenCounter;
use serde::Serialize;

use crate::config::{Snapshot, UpstreamKind};
use crate::egress::{for_protocol, CountPlan};
use crate::quickdoc::query::TiktokenCounter;
use crate::state::SharedState;

/// llama-server's default physical batch (`-ub`): what an aux embedder takes
/// per input when nothing else was configured.
pub const LLAMA_DEFAULT_UBATCH: u64 = 512;
/// llama-server's default logical batch (`-b`).
pub const LLAMA_DEFAULT_BATCH: u64 = 2048;
/// Tokens the embedding model adds around an input (BOS/EOS, CLS/SEP) that
/// `/tokenize` does not count; reserved inside `chunk_tokens`.
pub const SPECIAL_TOKENS: usize = 2;
/// The same for a reranker's query + document pair (BOS, query, EOS, SEP,
/// document, EOS).
pub const PAIR_SPECIAL_TOKENS: usize = 4;

/// A search's query is checked against a model's limit with the remembered
/// tokenizer ratio ([`cached_counter`]) — no tokenizer call — and counted
/// exactly, by the model, only when that estimate reaches this percentage of
/// the limit. Below it the estimate has at least a third of the limit as
/// margin, which a different ratio for one short query does not eat; above it
/// the exact count decides whether (and where) the query is cut.
pub const EXACT_ABOVE_PERCENT: u64 = 75;

/// Whether an estimate of `tokens` is near enough to `max` to be counted
/// exactly ([`EXACT_ABOVE_PERCENT`]).
pub fn near_limit(tokens: u64, max: u64) -> bool {
    tokens * 100 >= max * EXACT_ABOVE_PERCENT
}

/// The per-input limit of an embedding model and where it comes from.
#[derive(Debug, Clone, Serialize)]
pub struct InputLimit {
    /// Tokens one embedding input may hold; `None` when unknown.
    pub tokens: Option<u64>,
    /// One sentence: what the number is and where it comes from.
    pub source: String,
    /// `model` (its own tokenizer) | `tiktoken` (exact: an OpenAI model's own
    /// encoding) | `tiktoken_guess` (o200k_base standing in).
    pub tokenizer: &'static str,
}

/// An `-x N` / `--long N` / `--long=N` integer among freeform llama-server
/// args.
fn arg_int(args: &[String], short: &str, long: &str) -> Option<u64> {
    let mut found = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let value = if a == short || a == long {
            i += 1;
            args.get(i).map(String::as_str)
        } else {
            a.strip_prefix(&format!("{long}="))
        };
        if let Some(n) = value.and_then(|v| v.trim().parse::<u64>().ok()) {
            found = Some(n);
        }
        i += 1;
    }
    found
}

/// The limit of a local aux row, from its context, its freeform args and its
/// pooling. `last_pooling` models split an input across batches, so nothing
/// but the context bounds them.
fn local_limit(context: Option<u64>, args: &[String], last_pooling: bool) -> (Option<u64>, String) {
    if last_pooling {
        return match context {
            Some(c) => (
                Some(c),
                format!(
                    "the model's context of {c} tokens (last-token pooling: llama-server \
                     splits an input across batches, so the ubatch is no limit)"
                ),
            ),
            None => (
                None,
                "unknown: a last-token pooling model has no ubatch limit and its context is \
                 not known to this gateway"
                    .to_string(),
            ),
        };
    }
    let ub = arg_int(args, "-ub", "--ubatch-size");
    let b = arg_int(args, "-b", "--batch-size");
    let batch = b.unwrap_or(LLAMA_DEFAULT_BATCH);
    let ubatch = ub.unwrap_or(LLAMA_DEFAULT_UBATCH).min(batch);
    let ub_src = match (ub, b) {
        (Some(_), _) => format!("the model row's own -ub {ubatch}"),
        (None, Some(b)) if b < LLAMA_DEFAULT_UBATCH => format!("the model row's own -b {b}"),
        _ => format!("llama-server's ubatch default of {LLAMA_DEFAULT_UBATCH}"),
    };
    match context {
        Some(c) if c < ubatch => (
            Some(c),
            format!("the model's context of {c} tokens (smaller than its ubatch of {ubatch})"),
        ),
        Some(_) | None => (
            Some(ubatch),
            format!(
                "{ub_src}; set a larger batch on the model row (extra args -ub N -b N) to \
                 raise it"
            ),
        ),
    }
}

/// The per-input limit of the embedding model behind `alias`.
pub async fn input_limit(state: &SharedState, alias: &str) -> InputLimit {
    let snap = state.snapshot();
    let context = super::ops::embed_context(state, alias).await;
    let route = snap.resolve(alias).ok();
    let tokenizer = route
        .as_ref()
        .map(|r| tokenizer_kind(state, &snap, r))
        .unwrap_or("tiktoken_guess");
    let aux = route.as_ref().and_then(|r| {
        (r.upstream.kind == UpstreamKind::LlamaServer)
            .then(|| {
                snap.aux_models
                    .iter()
                    .find(|m| m.model_id == r.upstream_model)
            })
            .flatten()
    });
    if let Some(m) = aux {
        let last = pools_last(state, &snap, m).await;
        let (tokens, source) = local_limit(context, &m.args, last);
        return InputLimit {
            tokens,
            source,
            tokenizer,
        };
    }
    match context {
        Some(c) => InputLimit {
            tokens: Some(c),
            source: format!("the context length of {alias} in its upstream's catalog"),
            tokenizer,
        },
        None => InputLimit {
            tokens: None,
            source: format!(
                "unknown: the context length of {alias} is not in any catalog this gateway \
                 reads"
            ),
            tokenizer,
        },
    }
}

/// Whether an aux embedder pools on the last token: the row says so, or
/// leaves it to the model and the GGUF header does. A reranker never does
/// (`--reranking` selects rank pooling).
async fn pools_last(state: &SharedState, snap: &Snapshot, m: &crate::config::AuxModel) -> bool {
    if m.kind != crate::config::AuxKind::Embed {
        return false;
    }
    if let Some(p) = m.pooling.as_deref().filter(|p| !p.is_empty()) {
        return p == "last";
    }
    let path = std::path::Path::new(&snap.settings.aux_router.models_dir).join(&m.gguf_path);
    match state.gguf_cache.summarize_cached(&path).await {
        Ok(s) => s.pooling_type.and_then(crate::gguf::pooling_name) == Some("last"),
        Err(_) => false,
    }
}

fn tokenizer_kind(state: &SharedState, _snap: &Snapshot, r: &crate::config::Route) -> &'static str {
    match for_protocol(r.upstream.protocol).build_count_tokens(
        &state.http,
        &r.upstream,
        &r.upstream_model,
        "",
    ) {
        Ok(CountPlan::Request(_)) => "model",
        Ok(CountPlan::Ready(_)) => "tiktoken",
        _ => "tiktoken_guess",
    }
}

/// The refusal for a chunk size above the limit, or `None` when it fits.
pub fn refusal(limit: &InputLimit, alias: &str, chunk_tokens: i64) -> Option<String> {
    let max = limit.tokens?;
    (chunk_tokens as u64 > max).then(|| {
        format!(
            "chunk_tokens {chunk_tokens} is larger than what the embedding model '{alias}' \
             takes per input: at most {max} tokens ({}). A chunk is embedded with its heading \
             path, so pick at most {max}.",
            limit.source
        )
    })
}

/// A sentence for the base's view when the limit or the tokenizer is weak.
pub fn notes(limit: &InputLimit, alias: &str) -> Vec<String> {
    let mut out = Vec::new();
    if limit.tokens.is_none() {
        out.push(format!(
            "the per-input limit of '{alias}' is unknown to this gateway, so chunk_tokens could \
             not be checked against it — an oversized chunk shows up as an embedding error on \
             its file"
        ));
    }
    if limit.tokenizer == "tiktoken_guess" {
        out.push(format!(
            "'{alias}' has no tokenizer this gateway can reach, so chunks are sized with \
             tiktoken o200k_base — an approximation of its tokens"
        ));
    }
    out
}

/// The chunker's counter: tiktoken scaled by how many more (or fewer) tokens
/// the embedding model's own tokenizer makes of this file, plus the special
/// tokens the model wraps an input in.
pub struct ScaledCounter {
    ratio: f64,
    name: String,
    /// The ratio was measured with the model's own tokenizer (not the
    /// tiktoken stand-in of a failed or unreachable one).
    measured: bool,
}

impl ScaledCounter {
    pub fn plain() -> Self {
        Self {
            ratio: 1.0,
            name: TiktokenCounter.name(),
            measured: false,
        }
    }

    /// A counter with a ratio measured earlier (see [`cached_counter`]).
    pub fn with_ratio(alias: &str, ratio: f64) -> Self {
        Self {
            ratio,
            name: format!("{alias} tokenizer"),
            measured: true,
        }
    }

    /// Model tokens per tiktoken token.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Whether the model's own tokenizer measured the ratio.
    pub fn measured(&self) -> bool {
        self.measured
    }

    /// The model's tokens for text tiktoken counts as `tiktoken` — what
    /// [`count`](TokenCounter::count) makes of it, without the text. Chunks
    /// store their tiktoken count, so a stored chunk is measured with this.
    pub fn estimate(&self, tiktoken: usize) -> usize {
        (tiktoken as f64 * self.ratio).ceil() as usize + SPECIAL_TOKENS
    }
}

impl TokenCounter for ScaledCounter {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn count(&self, text: &str) -> usize {
        self.estimate(TiktokenCounter.count(text))
    }
}

/// How many windows a text is measured in. Each window is counted by the
/// model and by tiktoken; the worst ratio wins, so a dense stretch of the
/// text (digits, code, another script) is not averaged away.
const WINDOWS: usize = 16;
/// A window is not made smaller than this, so a short text (a query) is one
/// tokenizer call rather than sixteen.
const MIN_WINDOW_BYTES: usize = 2048;

/// The byte ranges [`calibrate`] measures: up to [`WINDOWS`] contiguous
/// windows over `text`, each cut on a char boundary.
fn windows(text: &str) -> Vec<(usize, usize)> {
    let n = text.len();
    if n == 0 {
        return Vec::new();
    }
    let count = WINDOWS.min(n.div_ceil(MIN_WINDOW_BYTES)).max(1);
    let per = n.div_ceil(count);
    let mut out = Vec::with_capacity(count);
    let mut start = 0;
    while start < n {
        let mut end = (start + per).min(n);
        while !text.is_char_boundary(end) {
            end += 1;
        }
        out.push((start, end));
        start = end;
    }
    out
}

/// Measure the embedding model's tokenizer against tiktoken on this text.
/// Returns the counter and a sentence naming what was counted with — a
/// failed measurement falls back to tiktoken and says so.
pub async fn calibrate(
    state: &SharedState,
    alias: &str,
    text: &str,
) -> (Arc<ScaledCounter>, String) {
    let limit = input_limit(state, alias).await;
    if limit.tokenizer == "tiktoken_guess" {
        return (
            Arc::new(ScaledCounter::plain()),
            "tiktoken o200k_base (the model has no reachable tokenizer)".into(),
        );
    }
    if text.is_empty() {
        return (
            Arc::new(ScaledCounter::plain()),
            "tiktoken o200k_base".into(),
        );
    }
    let mut ratio: f64 = 0.0;
    for (start, end) in windows(text) {
        let slice = &text[start..end];
        let tik = TiktokenCounter.count(slice);
        if tik == 0 {
            continue;
        }
        match crate::proxy::count_tokens_inner(state, alias, slice, true).await {
            Ok((n, headers)) if headers.fallback().is_none() => {
                ratio = ratio.max(n as f64 / tik as f64);
            }
            Ok(_) => {
                return (
                    Arc::new(ScaledCounter::plain()),
                    format!(
                        "tiktoken o200k_base ({alias} was counted by a fallback model, whose \
                         tokenizer is not the embedder's)"
                    ),
                )
            }
            Err((_, e)) => {
                return (
                    Arc::new(ScaledCounter::plain()),
                    format!("tiktoken o200k_base (counting with {alias} failed: {e})"),
                )
            }
        }
    }
    if ratio <= 0.0 {
        ratio = 1.0;
    }
    (
        Arc::new(ScaledCounter::with_ratio(alias, ratio)),
        format!("{alias}'s own tokenizer (worst window {ratio:.2}x tiktoken)"),
    )
}

/// What a model's tokenizer ratio is remembered under: the upstream and model
/// an alias resolves to, so a remapped alias (or another model) is a new
/// entry and a stale one is never used for it. `None` when the alias does not
/// resolve.
fn ratio_key(state: &SharedState, alias: &str) -> Option<String> {
    let r = state.snapshot().resolve(alias).ok()?;
    Some(format!(
        "{}|{}|{}",
        r.upstream.id, r.upstream.base_url, r.upstream_model
    ))
}

/// Whether a ratio is remembered for `alias`'s model ([`cached_counter`]).
pub fn ratio_known(state: &SharedState, alias: &str) -> bool {
    ratio_key(state, alias).is_some_and(|k| state.knowledge.ratio(&k).is_some())
}

/// Remember the ratio `counter` measured for `alias`'s model. Only a ratio the
/// model's own tokenizer measured is kept — a tiktoken stand-in (an
/// unreachable tokenizer, a fallback answer) is not a fact about the model.
pub fn remember_ratio(state: &SharedState, alias: &str, counter: &ScaledCounter) {
    if !counter.measured() {
        return;
    }
    if let Some(key) = ratio_key(state, alias) {
        state.knowledge.set_ratio(&key, counter.ratio());
    }
}

/// The counter for `alias`'s tokenizer, with its ratio to tiktoken **cached in
/// memory per model identity** ([`ratio_key`]): measured once over `sample`
/// ([`calibrate`]: up to [`WINDOWS`] tokenizer calls), then every later call
/// costs none. The entry lives until the process ends or the alias resolves to
/// another upstream/model — a model file swapped under an unchanged name is
/// only noticed by a fresh measurement ([`fit::measure`](super::fit) and
/// [`remember_ratio`] replace the entry), which is why the base's re-embed
/// stage measures again. The sentence says which it was.
pub async fn cached_counter(
    state: &SharedState,
    alias: &str,
    sample: &str,
) -> (Arc<ScaledCounter>, String) {
    if input_limit(state, alias).await.tokenizer == "tiktoken_guess" {
        return calibrate(state, alias, sample).await;
    }
    let key = ratio_key(state, alias);
    if let Some(r) = key.as_deref().and_then(|k| state.knowledge.ratio(k)) {
        return (
            Arc::new(ScaledCounter::with_ratio(alias, r)),
            format!("{alias}'s own tokenizer ({r:.2}x tiktoken, measured earlier and remembered)"),
        );
    }
    let (counter, why) = calibrate(state, alias, sample).await;
    remember_ratio(state, alias, &counter);
    (counter, why)
}

/// The tail of `text` that fits `max` tokens by `counter`, cut on a char
/// boundary (whitespace preferred), and whether anything was cut. The tail
/// is what is kept because the end of a query is its newest part.
pub fn fit_tail<'a>(text: &'a str, max: usize, counter: &dyn TokenCounter) -> (&'a str, bool) {
    if counter.count(text) <= max {
        return (text, false);
    }
    // Smallest start whose tail fits: a tail's count only shrinks as its
    // start moves right, so bisect over char boundaries.
    let (mut lo, mut hi) = (0usize, text.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let mut m = mid;
        while !text.is_char_boundary(m) {
            m += 1;
        }
        if counter.count(&text[m..]) <= max {
            hi = mid;
        } else {
            lo = m + 1;
        }
    }
    let mut start = lo.min(text.len());
    while !text.is_char_boundary(start) {
        start += 1;
    }
    // Opening mid-word: move on to the next whitespace instead, unless that
    // leaves nothing.
    let mid_word = text[..start]
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace());
    if mid_word {
        if let Some(off) = text[start..].find(char::is_whitespace) {
            let rest = text[start + off..].trim_start();
            if !rest.is_empty() {
                return (rest, true);
            }
        }
    }
    (&text[start..], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn a_local_embedder_takes_the_ubatch_default_unless_the_row_raises_it() {
        let (t, why) = local_limit(Some(8192), &[], false);
        assert_eq!(t, Some(512));
        assert!(why.contains("ubatch default of 512"), "{why}");
        let (t, _) = local_limit(Some(8192), &s(&["-ub", "2048", "-b", "2048"]), false);
        assert_eq!(t, Some(2048));
        let (t, _) = local_limit(Some(8192), &s(&["--ubatch-size=4096"]), false);
        assert_eq!(t, Some(2048), "the logical batch default caps it");
        let (t, _) = local_limit(Some(256), &s(&["-ub", "2048", "-b", "4096"]), false);
        assert_eq!(t, Some(256), "never above the context");
        let (t, _) = local_limit(None, &s(&["-b", "128"]), false);
        assert_eq!(t, Some(128));
    }

    #[test]
    fn a_last_pooling_embedder_is_bounded_by_its_context_not_the_ubatch() {
        let (t, why) = local_limit(Some(8192), &[], true);
        assert_eq!(t, Some(8192));
        assert!(why.contains("splits an input"), "{why}");
        assert_eq!(local_limit(None, &[], true).0, None);
    }

    #[test]
    fn windows_cover_the_text_on_char_boundaries() {
        assert!(windows("").is_empty());
        assert_eq!(windows("short").len(), 1);
        let text = "aé漢😀".repeat(20_000);
        let w = windows(&text);
        assert_eq!(w.len(), WINDOWS);
        assert_eq!(w[0].0, 0);
        assert_eq!(w.last().unwrap().1, text.len());
        for pair in w.windows(2) {
            assert_eq!(pair[0].1, pair[1].0, "contiguous");
        }
        for (a, b) in &w {
            assert!(text.is_char_boundary(*a) && text.is_char_boundary(*b));
        }
    }

    struct WordCount;
    impl TokenCounter for WordCount {
        fn name(&self) -> String {
            "words".into()
        }
        fn count(&self, text: &str) -> usize {
            text.split_whitespace().count()
        }
    }

    #[test]
    fn the_tail_that_fits_keeps_the_newest_words() {
        let (t, cut) = fit_tail("one two three four five six", 3, &WordCount);
        assert!(cut);
        assert_eq!(t, "four five six");
        let (t, cut) = fit_tail("one two", 3, &WordCount);
        assert!(!cut);
        assert_eq!(t, "one two");
        // One long word over the limit is still cut on a char boundary.
        let long = "é".repeat(50);
        let (t, _) = fit_tail(&long, 1, &WordCount);
        assert!(t.chars().all(|c| c == 'é'));
    }

    #[test]
    fn the_refusal_names_the_limit_and_its_source() {
        let l = InputLimit {
            tokens: Some(512),
            source: "llama-server's ubatch default of 512".into(),
            tokenizer: "model",
        };
        let m = refusal(&l, "embed/x", 600).unwrap();
        assert!(
            m.contains("at most 512") && m.contains("ubatch default"),
            "{m}"
        );
        assert!(refusal(&l, "embed/x", 512).is_none());
        let unknown = InputLimit {
            tokens: None,
            source: "unknown".into(),
            tokenizer: "tiktoken_guess",
        };
        assert!(refusal(&unknown, "x", 10_000).is_none());
        assert_eq!(notes(&unknown, "x").len(), 2);
    }
}
