//! The suite's corpus (benchmark design §4.1).
//!
//! `assets/bench/corpus-v1.txt` is a frozen concatenation of lmgw's own
//! design records as they stood on 2026-09-30, taken in filename (= date) order
//! and joined by one blank line — 437 392 bytes of English prose, tables and
//! code:
//!
//! 1. `2026-06-09-llm-api-gateway-design.md`
//! 2. `2026-06-29-mcp-gateway-design.md`
//! 3. `2026-08-29-quickdoc-design.md`
//! 4. `2026-08-30-per-model-containers-design.md`
//! 5. `2026-09-04-gpu-hold-design.md`
//! 6. `2026-09-17-model-capabilities-design.md`
//! 7. `2026-09-18-agent-catalog-design.md`
//! 8. `2026-09-18-usage-analytics-cost-policy-design.md`
//! 9. `2026-09-19-agent-container-runtime-design.md`
//! 10. `2026-09-21-image-generation-design.md`
//!
//! (all under `docs/design/`; the benchmark's own spec is not in
//! it). The file is never regenerated: the specs keep changing, the corpus
//! must not, and a new corpus is a new suite version.
//!
//! A run tokenizes it once through the bench server's `/tokenize` and slices
//! **token-id arrays** out of that sequence, so a prompt of *P* tokens is
//! exactly *P* tokens on every engine and model.
//!
//! **As text, never as control tokens** (§13 decision 58). The specs quote
//! chat-template markers — `<think>`, `<tool_call>`, `<|channel|>`,
//! `<|python_tag|>`, `[TOOL_CALLS]` — and `/tokenize` parses such strings
//! into a vocabulary's control tokens by default, which invites degenerate
//! continuations under `ignore_eos` (and flatters a drafter). Official
//! llama.cpp takes `parse_special: false`; ik_llama.cpp ignores it (its
//! `/tokenize` forces special parsing). So the text is also sent as pieces
//! split right after the opening bracket of every bracketed marker
//! ([`pieces`]): no piece holds a whole marker, neither engine can match
//! one, and both tokenize the corpus the same way.
//!
//! **The BOS** a vocabulary adds is not added to a token-id prompt by
//! llama-server, so every prompt starts with it here ([`Corpus::prompt`]),
//! counted inside *P*.

/// The corpus text, compiled in.
pub const CORPUS_V1: &str = include_str!("../../assets/bench/corpus-v1.txt");

/// The corpus as the bench server tokenizes it, and the tokens every prompt
/// starts with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Corpus {
    /// What the vocabulary puts before a text (its BOS), empty when it puts
    /// nothing.
    pub prefix: Vec<u32>,
    pub tokens: Vec<u32>,
}

impl Corpus {
    /// Tokens in the corpus itself (the offsets' range).
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// A prompt of exactly `len` tokens: the prefix, then the corpus from
    /// `offset` on ([`slice`]).
    pub fn prompt(&self, offset: usize, len: usize) -> Vec<u32> {
        let head = self.prefix.len().min(len);
        let mut out = self.prefix[..head].to_vec();
        out.extend(slice(&self.tokens, offset, len - head));
        out
    }
}

/// `text` cut right after the opening bracket of every bracketed marker — a
/// `<` or `[` followed, before any whitespace or other bracket, by its
/// closing `>` or `]` (`<think>`, `</tool_call>`, `<|channel|>`,
/// `[TOOL_CALLS]`, and `Vec<String>`'s `<String>` alike). Joined, the pieces
/// are `text`; none holds a whole marker, so none can be parsed as a control
/// token, whatever the engine does with `parse_special`.
pub fn pieces(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let close = match b {
            b'<' => b'>',
            b'[' => b']',
            _ => continue,
        };
        let rest = &bytes[i + 1..];
        let end = rest
            .iter()
            .position(|c| c.is_ascii_whitespace() || matches!(c, b'<' | b'>' | b'[' | b']'));
        if end.is_some_and(|e| e > 0 && rest[e] == close) {
            // The bracket is ASCII, so i + 1 is a char boundary.
            out.push(&text[start..i + 1]);
            start = i + 1;
        }
    }
    out.push(&text[start..]);
    out
}

/// What `with` (a text tokenized with the vocabulary's special tokens added)
/// has before `without` (the same text without them): the BOS, where the
/// vocabulary adds one. Empty when `without` is not inside `with`.
pub fn special_prefix(with: &[u32], without: &[u32]) -> Vec<u32> {
    if without.is_empty() || without.len() > with.len() {
        return Vec::new();
    }
    (0..=with.len() - without.len())
        .find(|&k| with[k..k + without.len()] == *without)
        .map(|k| with[..k].to_vec())
        .unwrap_or_default()
}

/// `len` tokens of `tokens` from `offset` on, wrapping around to the start
/// when the corpus is shorter than the prompt (§4.1) — a 131k-token prompt
/// is longer than the corpus on most tokenizers.
pub fn slice(tokens: &[u32], offset: usize, len: usize) -> Vec<u32> {
    if tokens.is_empty() {
        return Vec::new();
    }
    tokens
        .iter()
        .cycle()
        .skip(offset % tokens.len())
        .take(len)
        .copied()
        .collect()
}

/// Deterministic, well-spread start offsets into a corpus of `len` tokens.
///
/// Every request draws the next one, so repetitions of a point start at
/// different offsets and share no prefix (§4.1). The step is the golden-ratio
/// fraction of the corpus, nudged to be coprime with it, so the first `len`
/// draws are all distinct and consecutive ones land far apart. `salt` gives
/// each phase its own sequence; the same salt over the same corpus tokens
/// gives the same offsets on every run, which keeps runs of one model
/// comparable.
#[derive(Debug, Clone)]
pub struct Offsets {
    len: u64,
    step: u64,
    next: u64,
}

impl Offsets {
    pub fn new(len: usize, salt: u64) -> Self {
        let len = len.max(1) as u64;
        let mut step = ((len as f64) * 0.618_033_988_749_895) as u64 % len;
        step = step.max(1);
        while gcd(step, len) != 1 {
            step = step % len + 1;
        }
        let start = salt.wrapping_mul(7919) % len;
        Self {
            len,
            step,
            next: start,
        }
    }
}

impl Iterator for Offsets {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        let out = self.next;
        self.next = ((self.next as u128 + self.step as u128) % self.len as u128) as u64;
        Some(out as usize)
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_corpus_is_the_frozen_file() {
        assert_eq!(CORPUS_V1.len(), 437_392, "corpus-v1 must never change");
        assert!(CORPUS_V1.starts_with("# "));
        assert!(!CORPUS_V1.contains("Benchmark for models and llama-server builds"));
    }

    /// Review finding 6: the markers the specs quote are cut so no piece
    /// holds one; joined, the pieces are the text.
    #[test]
    fn markers_are_cut_after_their_opening_bracket() {
        let t = "a <think> b </tool_call> <|channel|>x [TOOL_CALLS] Vec<String> 1 < 2 > 0 [ok";
        let p = pieces(t);
        assert_eq!(p.concat(), t);
        assert_eq!(
            p,
            vec![
                "a <",
                "think> b <",
                "/tool_call> <",
                "|channel|>x [",
                "TOOL_CALLS] Vec<",
                "String> 1 < 2 > 0 [ok"
            ]
        );
        for piece in &p {
            for m in ["<think>", "</tool_call>", "<|channel|>", "[TOOL_CALLS]"] {
                assert!(!piece.contains(m), "{piece:?} holds {m}");
            }
        }
        // Nested and multibyte text: cut at the inner marker only.
        assert_eq!(pieces("Option<Vec<ü8>>"), vec!["Option<Vec<", "ü8>>"]);
        assert_eq!(pieces("<>"), vec!["<>"]);
        assert_eq!(pieces(""), vec![""]);
        // The real corpus: every marker is cut, and nothing is lost.
        let all = pieces(CORPUS_V1);
        assert_eq!(all.concat(), CORPUS_V1);
        for m in [
            "<think>",
            "<tool_call>",
            "<|python_tag|>",
            "<|channel|>",
            "[TOOL_CALLS]",
        ] {
            assert!(CORPUS_V1.contains(m), "{m}");
            assert!(all.iter().all(|p| !p.contains(m)), "{m}");
        }
    }

    #[test]
    fn a_prompt_starts_with_the_prefix_counted_inside_its_length() {
        let c = Corpus {
            prefix: vec![1],
            tokens: vec![10, 11, 12, 13],
        };
        assert_eq!(c.prompt(2, 3), vec![1, 12, 13]);
        assert_eq!(c.prompt(0, 1), vec![1]);
        assert_eq!(c.prompt(0, 0), Vec::<u32>::new());
        let bare = Corpus {
            prefix: vec![],
            ..c.clone()
        };
        assert_eq!(bare.prompt(2, 3), vec![12, 13, 10]);
        assert_eq!(c.len(), 4);
    }

    #[test]
    fn the_prefix_is_what_special_tokens_put_before_the_text() {
        assert_eq!(special_prefix(&[1, 50, 51], &[50, 51]), vec![1]);
        assert_eq!(special_prefix(&[50, 51], &[50, 51]), Vec::<u32>::new());
        // An EOS after the text is not a prefix.
        assert_eq!(special_prefix(&[1, 50, 51, 2], &[50, 51]), vec![1]);
        assert_eq!(special_prefix(&[50, 51, 2], &[50, 51]), Vec::<u32>::new());
        assert_eq!(special_prefix(&[1], &[]), Vec::<u32>::new());
        assert_eq!(special_prefix(&[7, 8], &[9]), Vec::<u32>::new());
    }

    #[test]
    fn slices_wrap_around() {
        let t = [1, 2, 3, 4];
        assert_eq!(slice(&t, 1, 2), vec![2, 3]);
        assert_eq!(slice(&t, 3, 6), vec![4, 1, 2, 3, 4, 1]);
        assert_eq!(slice(&t, 9, 3), vec![2, 3, 4]);
        assert!(slice(&[], 0, 5).is_empty());
    }

    #[test]
    fn offsets_are_deterministic_distinct_and_spread() {
        let a: Vec<usize> = Offsets::new(1000, 3).take(20).collect();
        let b: Vec<usize> = Offsets::new(1000, 3).take(20).collect();
        assert_eq!(a, b);
        let mut sorted = a.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 20, "{a:?}");
        // Consecutive draws are far apart, not neighbours sharing a prefix.
        for w in a.windows(2) {
            let d = w[0].abs_diff(w[1]);
            assert!(d.min(1000 - d) > 100, "{w:?}");
        }
        assert_ne!(Offsets::new(1000, 4).next(), Offsets::new(1000, 3).next());
        // Every offset over a whole period is visited exactly once.
        let all: std::collections::BTreeSet<usize> = Offsets::new(97, 1).take(97).collect();
        assert_eq!(all.len(), 97);
        // Degenerate corpora do not panic.
        assert_eq!(
            Offsets::new(1, 5).take(3).collect::<Vec<_>>(),
            vec![0, 0, 0]
        );
        assert_eq!(Offsets::new(0, 5).next(), Some(0));
    }
}
