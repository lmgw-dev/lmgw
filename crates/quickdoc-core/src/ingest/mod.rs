//! Ingestion (§8) — the half that needs no gateway.
//!
//! The pipeline is *code-fenced*: code fetches, code slices, code validates; the
//! model only ever proposes boundaries and metadata. Everything in this module
//! is that code, and it is pure — no HTTP, no model, no job. lmgw-core's
//! `quickdoc::ingest` job executor supplies the two impure halves (the fetcher
//! and the tool loop) and drives these steps in order:
//!
//! ```text
//! fetch ─→ sniff kind ─→ to_document_text ─→ [content_hash gate]
//!       ─→ window by the ingest model's real context
//!       ─→ LLM emits spans (emit_extraction, schema-constrained)
//!       ─→ validate: slice the ORIGINAL text, check the anchors
//!       ─→ chunk + embed
//! ```
//!
//! - [`source`] — the four source kinds, the sniffing order, the domain fence.
//! - [`extract`] — the verbatim contract: schema, line index, validation.
//! - [`prompt`] — the versioned instructions a corpus pins.
//! - [`html`] — the last-resort HTML→text reduction.
//! - [`url`] — join/split/host, so link-following stays inside the fence.

pub mod extract;
pub mod html;
pub mod prompt;
pub mod source;
pub mod url;

pub use extract::{
    is_verbatim, validate, AcceptedSection, Extraction, LineIndex, ProposedSection, Rejection,
    Verdict, EMIT_TOOL,
};
pub use source::{linked_urls, sniff, to_document_text, Fence, SourceKind};

/// The text a chunk is embedded as: its payload plus the derived labels.
///
/// §8 embeds derived text for recall while never serving it as payload, so the
/// two differ — and **one** definition of the difference, because ingest and
/// re-embed producing vectors from different compositions would degrade a
/// corpus with no symptom anyone could see.
pub fn embed_text(
    heading_path: &str,
    derived_title: &str,
    derived_summary: &str,
    payload: &str,
) -> String {
    [heading_path, derived_title, derived_summary, payload]
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A real token counter, asked over the network in production (llama.cpp
/// `/tokenize`) and locally in tests.
///
/// Async, and therefore not [`crate::embed::TokenCounter`]: window planning has
/// to know the *actual* count, which for a local model means asking the model's
/// own tokenizer. An estimate would put a guessed safety margin back into the
/// one place this design refuses to have one.
#[async_trait::async_trait]
pub trait AsyncTokenCounter: Send + Sync {
    async fn count(&self, text: &str) -> crate::Result<usize>;
}

/// Split a document into the windows one extraction call each can hold.
///
/// `budget_tokens` is the room a single call has for document text — derived by
/// the caller from the ingest model's **real** context length minus what the
/// prompt and the reply actually take, never from a constant. `measure` is the
/// caller's real token counter; it is consulted once for the whole document to
/// learn this document's own tokens-per-character density, and then once per
/// candidate window to *verify* the split rather than trust the estimate.
///
/// Returns absolute, inclusive `(first_line, last_line)` pairs covering every
/// line. A single line that does not fit on its own is still emitted as its own
/// window: truncating documentation to fit a budget is the one thing this must
/// not do, so the overrun surfaces at the upstream instead.
pub async fn plan_windows(
    text: &str,
    idx: &LineIndex,
    budget_tokens: usize,
    measure: &dyn AsyncTokenCounter,
) -> crate::Result<Vec<(usize, usize)>> {
    let lines = idx.len();
    if lines == 0 || budget_tokens == 0 {
        return Ok(vec![(1, lines.max(1))]);
    }
    let total_tokens = measure.count(text).await?;
    if total_tokens <= budget_tokens {
        return Ok(vec![(1, lines)]);
    }
    // Measured on this document with this tokenizer — not a guessed ratio.
    let chars = text.chars().count().max(1);
    let per_char = total_tokens as f64 / chars as f64;

    let mut windows = Vec::new();
    let mut start = 1usize;
    while start <= lines {
        let mut end = start;
        let mut est = 0f64;
        while end <= lines {
            let line_chars = idx.line(text, end).map_or(0, |l| l.chars().count() + 1);
            let next = est + line_chars as f64 * per_char;
            if next > budget_tokens as f64 && end > start {
                break;
            }
            est = next;
            end += 1;
        }
        end -= 1;
        // Verify the estimate. Shrinking by the measured overrun converges in a
        // couple of rounds; each round is one real count, which is what makes
        // this honest rather than a guess with a safety margin bolted on.
        for _ in 0..4 {
            if end <= start {
                break;
            }
            let (a, b) = match idx.span(start, end) {
                Some(s) => s,
                None => break,
            };
            let real = measure.count(&text[a..b]).await?;
            if real <= budget_tokens {
                break;
            }
            let keep = ((end - start + 1) as f64 * budget_tokens as f64 / real as f64) as usize;
            end = start + keep.max(1).min(end - start) - 1;
        }
        windows.push((start, end));
        start = end + 1;
    }
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whitespace-separated words: deterministic, and close enough to a real
    /// tokenizer's behaviour that the density estimate is exercised.
    struct Words;

    #[async_trait::async_trait]
    impl AsyncTokenCounter for Words {
        async fn count(&self, text: &str) -> crate::Result<usize> {
            Ok(words(text))
        }
    }

    fn words(s: &str) -> usize {
        s.split_whitespace().count()
    }

    #[tokio::test]
    async fn a_document_inside_the_budget_is_one_window() {
        let text = "a b c\nd e f\n";
        let idx = LineIndex::new(text);
        assert_eq!(
            plan_windows(text, &idx, 100, &Words).await.unwrap(),
            vec![(1, 2)]
        );
    }

    #[tokio::test]
    async fn windows_cover_every_line_exactly_once_and_respect_the_budget() {
        let text: String = (1..=40).map(|n| format!("line {n} of the doc\n")).collect();
        let idx = LineIndex::new(&text);
        let windows = plan_windows(&text, &idx, 20, &Words).await.unwrap();
        assert!(windows.len() > 1, "40 lines × 5 words must not fit in 20");
        assert_eq!(windows[0].0, 1);
        assert_eq!(windows.last().unwrap().1, idx.len());
        for pair in windows.windows(2) {
            assert_eq!(pair[1].0, pair[0].1 + 1, "gap or overlap: {pair:?}");
        }
        for (a, b) in &windows {
            let (s, e) = idx.span(*a, *b).unwrap();
            assert!(words(&text[s..e]) <= 20, "window {a}–{b} overruns");
        }
    }

    /// A single line bigger than the whole budget goes out whole. Cutting it to
    /// fit would put a truncated code sample in the corpus, which is exactly the
    /// corruption this pipeline exists to prevent.
    #[tokio::test]
    async fn an_oversized_line_is_never_split() {
        let text = format!("{}\nshort\n", "w ".repeat(50));
        let idx = LineIndex::new(&text);
        let windows = plan_windows(&text, &idx, 10, &Words).await.unwrap();
        assert_eq!(windows[0], (1, 1));
        assert_eq!(windows.last().unwrap().1, idx.len());
    }
}
