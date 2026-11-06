//! A chunk the embedding model refuses as too large, split in halves
//! (review R2, finding 7).
//!
//! Chunks are sized with a token count that is only as good as the tokenizer
//! behind it: a digit-dense table can tokenize denser than the sampled
//! windows of its file suggested, and a guessed tokenizer is a guess. When the
//! embedder still refuses a chunk as larger than one input, the ingest splits
//! that chunk in half — recursively, until the halves are taken or a piece
//! cannot be cut any more — and the file says so in a note. The refusal's
//! error variant is the signal, and for a plain upstream error its own
//! wording ([`is_oversize_error`]); nothing is pre-shrunk.

use crate::error::GatewayError;
use crate::knowledge::store::NewKbChunk;
use crate::quickdoc::query::TiktokenCounter;
use quickdoc_core::embed::TokenCounter;
use quickdoc_core::QuickdocError;
use sha2::{Digest, Sha256};

use super::chunk::embed_text;

/// The refusals of an embedding server that mean "this input is larger than I
/// take", in the words they really use — never a broad phrase such as "too
/// large" or "context length", which any unrelated error can carry (a
/// persistent one would send the ingest halving a chunk down to single bytes):
///
/// - llama-server: "input is too large to process. increase the physical batch
///   size" (error type `exceed_context_size_error` on newer builds, "exceeds
///   the available context size");
/// - OpenAI-shaped: code `context_length_exceeded`, "maximum context length";
/// - Gemini / Voyage: "maximum number of tokens", "max allowed tokens".
const OVERSIZE_PHRASES: [&str; 8] = [
    "input is too large to process",
    "physical batch size",
    "exceeds the available context size",
    "exceed_context_size",
    "context_length_exceeded",
    "maximum context length",
    "maximum number of tokens",
    "max allowed tokens",
];

/// HTTP statuses an embedding server answers an oversize input with: 400 and
/// 422 (OpenAI-shaped, llama-server), 413, and 500 (older llama-server
/// builds). An error that names any other status (401, 404, 429, 5xx …) is not
/// about the input's size whatever its message says.
const OVERSIZE_STATUSES: [u16; 4] = [400, 413, 422, 500];

/// The status in a gateway error rendered as `upstream error (NNN): …`.
fn upstream_status(error: &str) -> Option<u16> {
    let rest = error.split("upstream error (").nth(1)?;
    rest.split(')').next()?.trim().parse().ok()
}

/// Whether an embedding call's error is a refusal of the input as larger than
/// the model takes, decided on its variant: the context refusals — lmgw's
/// own check, llama-server's typed backstop, the OpenAI platform's
/// `context_length_exceeded` in either wording — are; an upstream error the
/// egress left plain (`Upstream`: an older llama-server's wording, another
/// provider's) is when its words say so ([`is_oversize`]); nothing else is —
/// a rate or quota refusal ([`GatewayError::UpstreamLimited`]), a hold, a
/// transport error, whatever its message holds.
pub fn is_oversize_error(e: &GatewayError) -> bool {
    match e {
        GatewayError::ContextExceeded { .. } | GatewayError::ContextExceededAsWorded { .. } => true,
        // By the status the error carries, not the one its text spells: the
        // relayed variant's text has no `upstream error (NNN):` lead.
        GatewayError::Upstream { .. } | GatewayError::UpstreamRelayed(_) => {
            e.upstream_status()
                .is_some_and(|s| OVERSIZE_STATUSES.contains(&s))
                && e.upstream_message().is_some_and(has_oversize_phrase)
        }
        _ => false,
    }
}

/// Whether an embedder's error is [`is_oversize_error`]'s refusal, as the
/// in-process embedder reports it.
pub fn refused_as_oversize(e: &QuickdocError) -> bool {
    matches!(e, QuickdocError::EmbedderOversize(_))
}

/// Whether an error's words say the input was larger than the model takes:
/// one of [`OVERSIZE_PHRASES`], and — where the error carries the upstream's
/// status — one of [`OVERSIZE_STATUSES`]. For an error only its text is left
/// of: a plain upstream error ([`is_oversize_error`]), a job's stored error.
pub fn is_oversize(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    if upstream_status(&e).is_some_and(|s| !OVERSIZE_STATUSES.contains(&s)) {
        return false;
    }
    has_oversize_phrase(&e)
}

/// Whether `text` holds one of [`OVERSIZE_PHRASES`].
fn has_oversize_phrase(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    OVERSIZE_PHRASES.iter().any(|needle| text.contains(needle))
}

/// The fewest tokens a piece is still split below: an oversize refusal of a
/// piece this small is not about its size, so the file fails with the
/// upstream's own error instead of halving on. It is a fraction of the
/// model's limit — `limit / MIN_PIECE_DIVISOR` — but never under
/// [`MIN_PIECE_TOKENS`]. The worst case for an input the model refuses
/// whatever its size is a handful of calls: 8 pieces of 1/8 of the limit,
/// then the failure.
pub const MIN_PIECE_DIVISOR: u64 = 8;
/// The absolute floor of [`min_piece_tokens`].
pub const MIN_PIECE_TOKENS: u64 = 32;

/// Pieces of at most this many tokens (tiktoken, as chunks store them) are not
/// split any further, for a model taking `limit` tokens per input.
pub fn min_piece_tokens(limit: u64) -> u64 {
    (limit / MIN_PIECE_DIVISOR).max(MIN_PIECE_TOKENS)
}

/// The id of a piece of a split chunk: derived from its parent's, so it is as
/// stable as the parent is.
fn child_id(parent: &str, half: u8) -> String {
    let mut h = Sha256::new();
    h.update(parent.as_bytes());
    h.update([0u8, half]);
    hex::encode(h.finalize())
}

/// Cut `payload` in two near its middle, at the whitespace nearest to it when
/// there is one in the middle half (so words stay whole), else at the nearest
/// char boundary. `None` when it cannot be cut into two non-empty pieces.
fn cut_point(payload: &str) -> Option<usize> {
    let len = payload.len();
    if len < 2 {
        return None;
    }
    let mut mid = len / 2;
    while !payload.is_char_boundary(mid) {
        mid += 1;
    }
    if mid == 0 || mid >= len {
        return None;
    }
    let (lo, hi) = (len / 4, len - len / 4);
    let before = payload[..mid]
        .char_indices()
        .rev()
        .find(|(i, c)| *i >= lo && c.is_whitespace())
        .map(|(i, _)| i);
    let after = payload[mid..]
        .char_indices()
        .find(|(i, c)| mid + *i < hi && c.is_whitespace())
        .map(|(i, _)| mid + i);
    let cut = match (before, after) {
        (Some(b), Some(a)) => {
            if mid - b <= a - mid {
                b
            } else {
                a
            }
        }
        (Some(x), None) | (None, Some(x)) => x,
        (None, None) => mid,
    };
    (cut > 0 && cut < len).then_some(cut)
}

/// The two halves of `c`. `text` is the file's assembled text: a payload that
/// is a verbatim region of it splits its span exactly; one that carries a
/// repeated table header or a re-opened fence keeps the whole span on both
/// halves (the highlight covers a little more, never less).
pub fn halves(c: &NewKbChunk, text: &str) -> Option<(NewKbChunk, NewKbChunk)> {
    let cut = cut_point(&c.payload)?;
    let (a, b) = c.payload.split_at(cut);
    let verbatim = text
        .get(c.span_start as usize..c.span_end as usize)
        .is_some_and(|region| region == c.payload);
    let make = |payload: &str, half: u8, span: (i64, i64)| NewKbChunk {
        id: child_id(&c.id, half),
        seq: c.seq,
        page: c.page,
        heading_path: c.heading_path.clone(),
        span_start: span.0,
        span_end: span.1,
        payload: payload.to_string(),
        tokens: TiktokenCounter.count(&embed_text(&c.heading_path, payload)) as i64,
        embedding: None,
    };
    let (sa, sb) = if verbatim {
        (
            (c.span_start, c.span_start + cut as i64),
            (c.span_start + cut as i64, c.span_end),
        )
    } else {
        ((c.span_start, c.span_end), (c.span_start, c.span_end))
    };
    Some((make(a, 0, sa), make(b, 1, sb)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(payload: &str, start: i64) -> NewKbChunk {
        NewKbChunk {
            id: "parent".into(),
            seq: 3,
            page: Some(2),
            heading_path: "A > B".into(),
            span_start: start,
            span_end: start + payload.len() as i64,
            payload: payload.into(),
            tokens: 9,
            embedding: None,
        }
    }

    #[test]
    fn the_refusals_of_llama_server_and_openai_are_recognised() {
        assert!(is_oversize(
            "input is too large to process. increase the physical batch size"
        ));
        assert!(is_oversize(
            "the request exceeds the available context size"
        ));
        assert!(is_oversize(
            "This model's maximum context length is 8192 tokens"
        ));
        assert!(!is_oversize("connection refused"));
        assert!(!is_oversize("GPU hold is on"));
    }

    #[test]
    fn a_broad_phrase_or_another_status_is_not_an_oversize_refusal() {
        // Unrelated errors that merely mention size or context.
        assert!(!is_oversize(
            "upstream error (400): the file is too large to upload"
        ));
        assert!(!is_oversize(
            "upstream error (400): invalid context length parameter"
        ));
        assert!(!is_oversize(
            "embedding failed: context size unsupported by this build"
        ));
        // The right words under a status that says otherwise.
        assert!(!is_oversize(
            "upstream error (401): input is too large to process"
        ));
        assert!(!is_oversize(
            "upstream error (429): maximum context length rate exceeded"
        ));
        // The real shapes, with their status.
        assert!(is_oversize(
            "embed/x: upstream error (400): input (900 tokens) is too large to process. \
             increase the physical batch size (current batch size: 512)"
        ));
        assert!(is_oversize(
            "upstream error (400): {\"code\":\"context_length_exceeded\"}"
        ));
    }

    /// The variant decides: every context refusal splits whatever its words
    /// (the OpenAI platform's carry none of the phrases), a plain upstream
    /// error by its words and status, and nothing else — not a 429 whose
    /// message happens to hold a phrase.
    #[test]
    fn the_error_variant_decides_and_only_plain_upstream_errors_are_read() {
        let platform = |status: u16, body: serde_json::Value| {
            crate::egress::for_protocol(crate::config::Protocol::Openai)
                .map_error(status, body.to_string().as_bytes())
        };
        let worded = platform(
            400,
            serde_json::json!({"error": {"message": "Your input exceeds the context window.",
                "type": "invalid_request_error", "param": "input",
                "code": "context_length_exceeded"}}),
        );
        assert!(matches!(
            worded,
            GatewayError::ContextExceededAsWorded { .. }
        ));
        assert!(is_oversize_error(&worded));
        let numbered = platform(
            400,
            serde_json::json!({"error": {"message": "This model's maximum context length is \
                8192 tokens. However, your messages resulted in 9000 tokens.",
                "type": "invalid_request_error", "param": "input",
                "code": "context_length_exceeded"}}),
        );
        assert!(matches!(numbered, GatewayError::ContextExceeded { .. }));
        assert!(is_oversize_error(&numbered));
        let limited = platform(
            429,
            serde_json::json!({"error": {"message": "maximum number of tokens per minute",
                "type": "tokens", "param": null, "code": "rate_limit_exceeded"}}),
        );
        assert!(matches!(limited, GatewayError::UpstreamLimited { .. }));
        assert!(!is_oversize_error(&limited));

        // llama-server's typed backstop, and an older build's plain words.
        let llama = |status: u16, body: &str| {
            crate::egress::for_protocol(crate::config::Protocol::LlamaCpp)
                .map_error(status, body.as_bytes())
        };
        assert!(is_oversize_error(&llama(
            400,
            r#"{"error":{"code":400,"message":"input (8010 tokens) is too large to process. increase the physical batch size","type":"exceed_context_size_error","n_prompt_tokens":8010,"n_ctx":4096}}"#
        )));
        let plain = llama(
            500,
            r#"{"error":{"code":500,"message":"input is too large to process. increase the physical batch size","type":"server_error"}}"#,
        );
        assert!(matches!(plain, GatewayError::Upstream { .. }));
        assert!(is_oversize_error(&plain));
        assert!(!is_oversize_error(&llama(
            401,
            r#"{"error":{"message":"input is too large to process","type":"auth"}}"#
        )));
        assert!(!is_oversize_error(&GatewayError::Transport(
            "input is too large to process".into()
        )));
        // The platform's relayed 4xx goes by its status like a plain one: a
        // 400 naming the context is oversize, a 404 or 409 that merely
        // mentions it is not.
        let relayed = |status: u16, message: &str| {
            GatewayError::UpstreamRelayed(Box::new(crate::error::Relayed {
                status,
                error_type: Some("invalid_request_error".into()),
                code: None,
                param: None,
                message: message.into(),
            }))
        };
        let words = "This model's maximum context length is 8192 tokens";
        assert!(is_oversize_error(&relayed(400, words)));
        assert!(!is_oversize_error(&relayed(404, words)));
        assert!(!is_oversize_error(&relayed(409, words)));
    }

    #[test]
    fn the_smallest_piece_scales_with_the_limit_and_has_a_floor() {
        assert_eq!(min_piece_tokens(512), 64);
        assert_eq!(min_piece_tokens(8192), 1024);
        assert_eq!(min_piece_tokens(100), MIN_PIECE_TOKENS);
        assert_eq!(min_piece_tokens(0), MIN_PIECE_TOKENS);
    }

    #[test]
    fn halves_are_verbatim_regions_that_tile_the_chunk() {
        let text = "xx alpha beta gamma delta epsilon zeta eta theta yy";
        let payload = &text[3..text.len() - 3];
        let c = chunk(payload, 3);
        let (a, b) = halves(&c, text).unwrap();
        assert_eq!(format!("{}{}", a.payload, b.payload), payload);
        assert_eq!(a.span_start, 3);
        assert_eq!(a.span_end, b.span_start);
        assert_eq!(b.span_end, c.span_end);
        assert_eq!(&text[a.span_start as usize..a.span_end as usize], a.payload);
        assert_eq!(&text[b.span_start as usize..b.span_end as usize], b.payload);
        assert_ne!(a.id, b.id);
        assert_eq!(
            (a.seq, a.page, a.heading_path.as_str()),
            (3, Some(2), "A > B")
        );
        // Cut at a word boundary, not through a word.
        assert!(b.payload.starts_with(' '), "{:?}", b.payload);
    }

    #[test]
    fn a_wrapped_payload_keeps_the_whole_span_on_both_halves() {
        let c = chunk("| h |\n| - |\n| 1 | 2 | 3 | 4 |", 100);
        let (a, b) = halves(&c, "short text").unwrap();
        assert_eq!((a.span_start, a.span_end), (c.span_start, c.span_end));
        assert_eq!((b.span_start, b.span_end), (c.span_start, c.span_end));
    }

    #[test]
    fn splitting_stops_where_a_piece_cannot_be_cut() {
        assert!(halves(&chunk("x", 0), "x").is_none());
        let mut c = chunk("漢字漢字", 0);
        let mut depth = 0;
        while let Some((a, _)) = halves(&c, "漢字漢字") {
            c = a;
            depth += 1;
            assert!(depth < 10);
        }
        assert!(depth >= 1);
    }
}
