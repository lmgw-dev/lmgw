//! A streamed `/completion`, as the phases need it: a client timestamp for
//! every chunk that carries a token, and the server's `timings` from the
//! final chunk (benchmark design §4.3).
//!
//! Both engines stream `data: {…}` lines and end with a `"stop": true` chunk
//! that carries `timings` (verified 2026-09-29, §2.1). They differ in what a
//! token chunk looks like:
//!
//! * official llama.cpp sends `"tokens": [id, …]` on every chunk — usually one
//!   id, several when a speculative draft is accepted, and an empty array on
//!   the final chunk;
//! * ik_llama.cpp sends no `tokens` at all, one chunk per token.
//!
//! So a chunk's token count is its `tokens` array's length when it has one,
//! else 1 for a non-final chunk, else 1 for a final chunk that still carries
//! content.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::ir::Timings;

/// One parsed `data:` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub tokens: u32,
    /// The token ids, where the engine sends them (official's `tokens`).
    pub ids: Vec<u32>,
    pub stop: bool,
    pub content: String,
    pub timings: Option<Timings>,
}

/// Parse one SSE `data:` payload. `Ok(None)` for a payload that is not a
/// chunk (`[DONE]`, a blank keep-alive); `Err` for an `error` object the
/// server streams instead of a chunk (a context overflow mid-stream, say) or
/// for a payload that is not JSON.
pub fn parse_chunk(data: &str) -> Result<Option<Chunk>, String> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }
    let v: Value =
        serde_json::from_str(data).map_err(|e| format!("unreadable stream chunk ({e}): {data}"))?;
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        return Err(format!("the server streamed an error: {msg}"));
    }
    let stop = v.get("stop").and_then(Value::as_bool).unwrap_or(false);
    let content = v
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let listed = v.get("tokens").and_then(Value::as_array);
    let tokens = match listed {
        Some(ids) => ids.len() as u32,
        None if !stop => 1,
        None => u32::from(!content.is_empty()),
    };
    let ids = listed
        .map(|ids| {
            ids.iter()
                .filter_map(|i| i.as_u64().and_then(|i| u32::try_from(i).ok()))
                .collect()
        })
        .unwrap_or_default();
    let timings = if stop {
        crate::egress::openai::parse_timings(v.get("timings"))
    } else {
        None
    };
    Ok(Some(Chunk {
        tokens,
        ids,
        stop,
        content,
        timings,
    }))
}

/// What one streamed request produced, filled in as it arrives — so a
/// stream the mixed phase closes early still leaves its timestamps behind.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamRecord {
    /// Just before the request was sent.
    pub sent: Instant,
    /// Every token-carrying chunk: arrival time and token count.
    pub tokens: Vec<(Instant, u32)>,
    /// The generated token ids in order, where the engine sends them
    /// (official llama.cpp; ik sends none).
    pub ids: Vec<u32>,
    /// Arrival of the final (`stop: true`) chunk; `None` while running or
    /// when the stream was closed early.
    pub end: Option<Instant>,
    pub timings: Option<Timings>,
    pub content: String,
}

impl StreamRecord {
    pub fn new(sent: Instant) -> Self {
        Self {
            sent,
            tokens: Vec::new(),
            ids: Vec::new(),
            end: None,
            timings: None,
            content: String::new(),
        }
    }

    /// Fold one chunk in, arrived at `at`.
    pub fn push(&mut self, at: Instant, chunk: Chunk) {
        if chunk.tokens > 0 {
            self.tokens.push((at, chunk.tokens));
        }
        self.ids.extend_from_slice(&chunk.ids);
        self.content.push_str(&chunk.content);
        if chunk.stop {
            self.end = Some(at);
            self.timings = chunk.timings;
        }
    }

    /// The first token's arrival; the final chunk's when no chunk carried a
    /// token (a server that folds a one-token answer into its final chunk).
    pub fn first_token(&self) -> Option<Instant> {
        self.tokens.first().map(|t| t.0).or(self.end)
    }

    /// **TTFT**: sent → first streamed token.
    pub fn ttft(&self) -> Option<Duration> {
        self.first_token()
            .and_then(|t| t.checked_duration_since(self.sent))
    }

    /// First token → last token, and the tokens that arrived after the first
    /// chunk: the decode window, with the prompt's prefill outside it.
    pub fn decode_window(&self) -> Option<(Instant, Instant, u64)> {
        let (first, first_n) = *self.tokens.first()?;
        let (last, _) = *self.tokens.last()?;
        let total: u64 = self.tokens.iter().map(|(_, n)| *n as u64).sum();
        Some((first, last, total - first_n as u64))
    }

    /// Distinct token ids over generated ones, when the stream carried an id
    /// for every generated token (`predicted_n`): near 1 for prose, low for
    /// a degenerate loop — which `ignore_eos` can run into, and which a
    /// drafter predicts perfectly (§13 decision 58). `None` on engines that
    /// send no ids.
    pub fn distinct_ratio(&self, predicted_n: u64) -> Option<f64> {
        if predicted_n == 0 || self.ids.len() as u64 != predicted_n {
            return None;
        }
        let distinct: std::collections::HashSet<u32> = self.ids.iter().copied().collect();
        Some(distinct.len() as f64 / self.ids.len() as f64)
    }

    /// The final chunk's timings, or an error naming what the stream lacked.
    pub fn timings(&self) -> Result<&Timings, String> {
        match (&self.end, &self.timings) {
            (Some(_), Some(t)) => Ok(t),
            (Some(_), None) => Err("the final chunk carried no timings".into()),
            (None, _) => Err("the stream ended without a final (stop) chunk".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFICIAL_TOKEN: &str = r#"{"index":0,"content":" are","tokens":[513],"stop":false,"id_slot":-1,"tokens_predicted":3,"tokens_evaluated":19}"#;
    const OFFICIAL_FINAL: &str = r#"{"index":0,"content":"","tokens":[],"id_slot":1,"stop":true,"tokens_predicted":4,"tokens_evaluated":19,"timings":{"cache_n":0,"prompt_n":10,"prompt_ms":39.367,"prompt_per_token_ms":3.9367,"prompt_per_second":254.02,"predicted_n":4,"predicted_ms":50.04,"predicted_per_token_ms":16.68,"predicted_per_second":59.95}}"#;
    const IK_TOKEN: &str = r#"{"content":"**","stop":false,"id_slot":0,"multimodal":false}"#;
    const IK_FINAL: &str = r#"{"content":"","generated_text":"\n\n**1.","id_slot":0,"stop":true,"tokens_predicted":4,"tokens_evaluated":19,"timings":{"prompt_n":10,"prompt_ms":34.08,"prompt_per_token_ms":3.408,"prompt_per_second":293.43,"predicted_n":4,"predicted_ms":41.288,"predicted_per_token_ms":10.322,"predicted_per_second":96.88,"n_ctx":4096,"n_past":13}}"#;

    #[test]
    fn both_engines_chunks_parse() {
        let c = parse_chunk(OFFICIAL_TOKEN).unwrap().unwrap();
        assert_eq!((c.tokens, c.stop, c.content.as_str()), (1, false, " are"));
        let f = parse_chunk(OFFICIAL_FINAL).unwrap().unwrap();
        assert_eq!((f.tokens, f.stop), (0, true));
        let t = f.timings.unwrap();
        assert_eq!((t.prompt_n, t.predicted_n, t.cache_n), (10, 4, Some(0)));

        let c = parse_chunk(IK_TOKEN).unwrap().unwrap();
        assert_eq!((c.tokens, c.stop), (1, false));
        let f = parse_chunk(IK_FINAL).unwrap().unwrap();
        assert_eq!((f.tokens, f.stop), (0, true));
        let t = f.timings.unwrap();
        assert_eq!((t.prompt_n, t.cache_n), (10, None), "ik sends no cache_n");
        assert!((t.predicted_per_second - 96.88).abs() < 1e-9);
    }

    #[test]
    fn a_draft_burst_counts_every_token_and_errors_surface() {
        let c = parse_chunk(r#"{"content":"abc","tokens":[1,2,3],"stop":false}"#)
            .unwrap()
            .unwrap();
        assert_eq!(c.tokens, 3);
        let c = parse_chunk(r#"{"content":"x","stop":true}"#)
            .unwrap()
            .unwrap();
        assert_eq!(
            c.tokens, 1,
            "a final chunk with content still carries a token"
        );
        assert_eq!(parse_chunk("[DONE]").unwrap(), None);
        assert_eq!(parse_chunk("  ").unwrap(), None);
        let e = parse_chunk(r#"{"error":{"code":400,"message":"context full"}}"#).unwrap_err();
        assert!(e.contains("context full"), "{e}");
        assert!(parse_chunk("{not json").is_err());
    }

    #[test]
    fn the_record_derives_ttft_and_the_decode_window() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut r = StreamRecord::new(t0);
        assert!(r.timings().is_err());
        r.push(at(120), parse_chunk(OFFICIAL_TOKEN).unwrap().unwrap());
        r.push(at(140), parse_chunk(IK_TOKEN).unwrap().unwrap());
        r.push(
            at(160),
            parse_chunk(r#"{"content":"xy","tokens":[5,6],"stop":false}"#)
                .unwrap()
                .unwrap(),
        );
        r.push(at(170), parse_chunk(OFFICIAL_FINAL).unwrap().unwrap());
        assert_eq!(r.ttft(), Some(Duration::from_millis(120)));
        assert_eq!(r.decode_window(), Some((at(120), at(160), 3)));
        assert_eq!(r.end, Some(at(170)));
        assert_eq!(r.content, " are**xy");
        assert_eq!(r.timings().unwrap().prompt_n, 10);
    }

    #[test]
    fn the_distinct_ratio_needs_an_id_per_generated_token() {
        let t0 = Instant::now();
        let mut r = StreamRecord::new(t0);
        for ids in ["[5]", "[6,5]", "[5]"] {
            r.push(
                t0,
                parse_chunk(&format!(r#"{{"content":"x","tokens":{ids},"stop":false}}"#))
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(r.ids, vec![5, 6, 5, 5]);
        assert_eq!(r.distinct_ratio(4), Some(0.5));
        assert_eq!(r.distinct_ratio(5), None, "an id missing");
        // ik: no ids at all.
        let mut ik = StreamRecord::new(t0);
        ik.push(t0, parse_chunk(IK_TOKEN).unwrap().unwrap());
        assert_eq!(ik.distinct_ratio(1), None);
    }

    #[test]
    fn a_one_token_answer_folded_into_the_final_chunk_has_a_ttft() {
        let t0 = Instant::now();
        let mut r = StreamRecord::new(t0);
        r.push(
            t0 + Duration::from_millis(50),
            parse_chunk(IK_FINAL).unwrap().unwrap(),
        );
        assert_eq!(r.ttft(), Some(Duration::from_millis(50)));
        assert_eq!(r.decode_window(), None);
    }
}
