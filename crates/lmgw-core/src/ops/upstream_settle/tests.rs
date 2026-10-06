//! [`settle`]'s rule table, row by row (llama.cpp egress design §5).

use super::*;
use crate::config::Protocol::{Anthropic, Gemini, LlamaCpp, Openai};
use crate::config::UpstreamKind::{AudioCpp, Generic, LlamaServer};

fn row(protocol: Protocol, kind: UpstreamKind) -> UpstreamShape {
    UpstreamShape {
        protocol,
        kind,
        supports_responses: false,
    }
}

/// The protocol and kind stored, and whether anything was said about it.
fn settled(
    protocol: Option<Protocol>,
    kind: Option<UpstreamKind>,
    current: UpstreamShape,
) -> (Protocol, UpstreamKind, bool) {
    let s = settle(protocol, kind, None, current).unwrap();
    (s.shape.protocol, s.shape.kind, !s.notes.is_empty())
}

fn refused(
    protocol: Option<Protocol>,
    kind: Option<UpstreamKind>,
    current: UpstreamShape,
) -> String {
    settle(protocol, kind, None, current).unwrap_err()
}

#[test]
fn llama_cpp_with_no_kind_or_llama_server_is_a_llama_server() {
    for current in [
        row(Openai, Generic),
        row(LlamaCpp, LlamaServer),
        UpstreamShape::NEW_ROW,
    ] {
        let (p, k, _) = settled(Some(LlamaCpp), None, current);
        assert_eq!((p, k), (LlamaCpp, LlamaServer), "{current:?}");
        let (p, k, noted) = settled(Some(LlamaCpp), Some(LlamaServer), current);
        assert_eq!((p, k, noted), (LlamaCpp, LlamaServer, false), "{current:?}");
    }
    // The kind it was not sent is said.
    let s = settle(Some(LlamaCpp), None, None, row(Openai, Generic)).unwrap();
    assert!(s.notes[0].contains("kind set to llama_server"), "{s:?}");
    assert!(!settled(Some(LlamaCpp), None, row(LlamaCpp, LlamaServer)).2);
}

#[test]
fn llama_cpp_with_any_other_kind_is_refused_by_name() {
    for kind in [Generic, AudioCpp] {
        let e = refused(Some(LlamaCpp), Some(kind), UpstreamShape::NEW_ROW);
        assert!(e.contains("llama_cpp") && e.contains(kind.as_str()), "{e}");
    }
}

#[test]
fn openai_with_llama_server_is_the_old_spelling_of_llama_cpp_and_says_so() {
    for current in [
        row(Openai, Generic),
        row(LlamaCpp, LlamaServer),
        UpstreamShape::NEW_ROW,
    ] {
        let s = settle(Some(Openai), Some(LlamaServer), None, current).unwrap();
        assert_eq!((s.shape.protocol, s.shape.kind), (LlamaCpp, LlamaServer));
        assert!(s.notes[0].contains("old spelling"), "{s:?}");
        assert!(s.message("upstream 'x' created").contains("old spelling"));
    }
}

#[test]
fn anthropic_with_llama_server_is_stored_as_sent() {
    for current in [row(Openai, Generic), row(LlamaCpp, LlamaServer)] {
        assert_eq!(
            settled(Some(Anthropic), Some(LlamaServer), current),
            (Anthropic, LlamaServer, false)
        );
    }
}

#[test]
fn gemini_with_llama_server_is_refused() {
    let e = refused(Some(Gemini), Some(LlamaServer), UpstreamShape::NEW_ROW);
    assert!(e.contains("gemini") && e.contains("llama_server"), "{e}");
}

#[test]
fn openai_or_gemini_alone_leaves_a_llama_server_for_generic() {
    for current in [row(LlamaCpp, LlamaServer), row(Anthropic, LlamaServer)] {
        for p in [Openai, Gemini] {
            assert_eq!(
                settled(Some(p), None, current),
                (p, Generic, true),
                "{current:?}"
            );
        }
    }
}

#[test]
fn anthropic_alone_keeps_a_llama_server() {
    assert_eq!(
        settled(Some(Anthropic), None, row(LlamaCpp, LlamaServer)),
        (Anthropic, LlamaServer, false)
    );
}

#[test]
fn another_kind_alone_on_a_llama_cpp_row_is_refused() {
    for kind in [Generic, AudioCpp] {
        let e = refused(None, Some(kind), row(LlamaCpp, LlamaServer));
        assert!(e.contains("protocol"), "{e}");
    }
}

#[test]
fn llama_server_alone_reads_the_rows_protocol() {
    // openai: the old spelling.
    let s = settle(None, Some(LlamaServer), None, row(Openai, Generic)).unwrap();
    assert_eq!((s.shape.protocol, s.shape.kind), (LlamaCpp, LlamaServer));
    assert!(s.notes[0].contains("old spelling"), "{s:?}");
    // gemini: refused.
    refused(None, Some(LlamaServer), row(Gemini, Generic));
    // A create without a protocol is an openai one.
    let (p, k, _) = settled(None, Some(LlamaServer), UpstreamShape::NEW_ROW);
    assert_eq!((p, k), (LlamaCpp, LlamaServer));
    // anthropic and llama_cpp: as sent.
    assert_eq!(
        settled(None, Some(LlamaServer), row(Anthropic, Generic)),
        (Anthropic, LlamaServer, false)
    );
    assert_eq!(
        settled(None, Some(LlamaServer), row(LlamaCpp, LlamaServer)),
        (LlamaCpp, LlamaServer, false)
    );
}

#[test]
fn everything_else_is_stored_as_sent() {
    let cases = [
        (None, None, row(Openai, Generic), (Openai, Generic)),
        (
            None,
            None,
            row(LlamaCpp, LlamaServer),
            (LlamaCpp, LlamaServer),
        ),
        (
            None,
            None,
            row(Anthropic, LlamaServer),
            (Anthropic, LlamaServer),
        ),
        (
            Some(Anthropic),
            None,
            row(Openai, Generic),
            (Anthropic, Generic),
        ),
        (
            Some(Openai),
            Some(AudioCpp),
            row(Openai, Generic),
            (Openai, AudioCpp),
        ),
        (
            None,
            Some(AudioCpp),
            row(Openai, Generic),
            (Openai, AudioCpp),
        ),
        (
            None,
            Some(Generic),
            row(Anthropic, LlamaServer),
            (Anthropic, Generic),
        ),
        // The dashboard's switch away from llama_cpp: it resets the kind.
        (
            Some(Openai),
            Some(Generic),
            row(LlamaCpp, LlamaServer),
            (Openai, Generic),
        ),
        (
            Some(Gemini),
            Some(Generic),
            row(LlamaCpp, LlamaServer),
            (Gemini, Generic),
        ),
    ];
    for (p, k, current, want) in cases {
        let (sp, sk, noted) = settled(p, k, current);
        assert_eq!((sp, sk), want, "{p:?} {k:?} on {current:?}");
        assert!(!noted, "{p:?} {k:?} on {current:?}");
    }
}

/// Decision 19: no native `/v1/responses` on a `llama_cpp` row.
#[test]
fn supports_responses_is_never_on_for_llama_cpp() {
    let on = UpstreamShape {
        supports_responses: true,
        ..row(Openai, Generic)
    };
    // Sent on: refused by name.
    let e = settle(Some(LlamaCpp), None, Some(true), on).unwrap_err();
    assert!(e.contains("supports_responses"), "{e}");
    let e = settle(Some(Openai), Some(LlamaServer), Some(true), on).unwrap_err();
    assert!(e.contains("supports_responses"), "{e}");
    // Carried over from the row: switched off and said.
    let s = settle(Some(LlamaCpp), None, None, on).unwrap();
    assert!(!s.shape.supports_responses);
    assert!(s.notes.iter().any(|n| n.contains("/v1/responses")), "{s:?}");
    // Sent off: off, nothing to say about it.
    let s = settle(Some(LlamaCpp), Some(LlamaServer), Some(false), on).unwrap();
    assert!(!s.shape.supports_responses);
    assert!(
        !s.notes.iter().any(|n| n.contains("/v1/responses")),
        "{s:?}"
    );
    // Every other protocol keeps what it was sent, or had.
    let s = settle(None, None, None, on).unwrap();
    assert!(s.shape.supports_responses);
    let s = settle(Some(Openai), None, Some(false), on).unwrap();
    assert!(!s.shape.supports_responses);
    let s = settle(None, None, Some(true), row(Anthropic, LlamaServer)).unwrap();
    assert!(s.shape.supports_responses);
}

#[test]
fn a_blank_protocol_or_kind_is_not_sent_and_sd_cpp_is_refused() {
    assert_eq!(sent_protocol(None), Ok(None));
    assert_eq!(sent_protocol(Some("  ")), Ok(None));
    assert_eq!(sent_protocol(Some("llama_cpp")), Ok(Some(LlamaCpp)));
    assert!(sent_protocol(Some("llama"))
        .unwrap_err()
        .contains("llama_cpp"));
    assert_eq!(sent_kind(Some("")), Ok(None));
    assert_eq!(sent_kind(Some("llama_server")), Ok(Some(LlamaServer)));
    assert!(sent_kind(Some("sd_cpp"))
        .unwrap_err()
        .contains("image class"));
}
