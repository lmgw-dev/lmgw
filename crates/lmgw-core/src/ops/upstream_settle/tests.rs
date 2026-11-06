//! [`settle`]'s rule table, row by row (llama.cpp egress design §5, openai
//! egress design §4.3).

use super::*;
use crate::config::Protocol::{Anthropic, Gemini, LlamaCpp, Openai, OpenaiCompatible};
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
fn a_create_starts_from_a_compatible_generic_row() {
    assert_eq!(UpstreamShape::NEW_ROW, row(OpenaiCompatible, Generic));
}

#[test]
fn llama_cpp_with_no_kind_or_llama_server_is_a_llama_server() {
    for current in [
        row(OpenaiCompatible, Generic),
        row(LlamaCpp, LlamaServer),
        UpstreamShape::NEW_ROW,
    ] {
        let (p, k, _) = settled(Some(LlamaCpp), None, current);
        assert_eq!((p, k), (LlamaCpp, LlamaServer), "{current:?}");
        let (p, k, noted) = settled(Some(LlamaCpp), Some(LlamaServer), current);
        assert_eq!((p, k, noted), (LlamaCpp, LlamaServer, false), "{current:?}");
    }
    // The kind it was not sent is said.
    let s = settle(Some(LlamaCpp), None, None, row(OpenaiCompatible, Generic)).unwrap();
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
fn an_openai_shaped_protocol_with_llama_server_is_refused_by_name() {
    for p in [Openai, OpenaiCompatible] {
        for current in [
            row(OpenaiCompatible, Generic),
            row(LlamaCpp, LlamaServer),
            UpstreamShape::NEW_ROW,
        ] {
            let e = refused(Some(p), Some(LlamaServer), current);
            assert!(e.contains("use protocol llama_cpp"), "{p:?} {e}");
            assert!(e.contains(p.as_str()), "{p:?} {e}");
        }
    }
}

/// Decision 7: the OpenAI platform is always generic.
#[test]
fn openai_is_always_generic() {
    for current in [row(OpenaiCompatible, Generic), UpstreamShape::NEW_ROW] {
        assert_eq!(
            settled(Some(Openai), None, current),
            (Openai, Generic, false)
        );
        assert_eq!(
            settled(Some(Openai), Some(Generic), current),
            (Openai, Generic, false)
        );
    }
    let e = refused(Some(Openai), Some(AudioCpp), UpstreamShape::NEW_ROW);
    assert!(
        e.contains("an audio.cpp server is openai_compatible"),
        "{e}"
    );
    // A row of another kind moving to openai alone becomes generic, said.
    for current in [row(OpenaiCompatible, AudioCpp), row(LlamaCpp, LlamaServer)] {
        let s = settle(Some(Openai), None, None, current).unwrap();
        assert_eq!((s.shape.protocol, s.shape.kind), (Openai, Generic));
        assert!(s.notes[0].contains("kind set to generic"), "{s:?}");
    }
    // An audio.cpp kind alone on an openai row needs the protocol too.
    let e = refused(None, Some(AudioCpp), row(Openai, Generic));
    assert!(e.contains("send protocol openai_compatible as well"), "{e}");
}

#[test]
fn anthropic_with_llama_server_is_stored_as_sent() {
    for current in [row(OpenaiCompatible, Generic), row(LlamaCpp, LlamaServer)] {
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
fn openai_compatible_or_gemini_alone_leaves_a_llama_server_for_generic() {
    for current in [row(LlamaCpp, LlamaServer), row(Anthropic, LlamaServer)] {
        for p in [OpenaiCompatible, Gemini] {
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
    // openai and openai_compatible: refused, by name.
    for p in [Openai, OpenaiCompatible] {
        let e = refused(None, Some(LlamaServer), row(p, Generic));
        assert!(e.contains("use protocol llama_cpp"), "{e}");
    }
    // gemini: refused.
    refused(None, Some(LlamaServer), row(Gemini, Generic));
    // A create without a protocol is an openai_compatible one.
    refused(None, Some(LlamaServer), UpstreamShape::NEW_ROW);
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
        (
            None,
            None,
            row(OpenaiCompatible, Generic),
            (OpenaiCompatible, Generic),
        ),
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
            row(OpenaiCompatible, Generic),
            (Anthropic, Generic),
        ),
        (
            Some(OpenaiCompatible),
            Some(AudioCpp),
            row(OpenaiCompatible, Generic),
            (OpenaiCompatible, AudioCpp),
        ),
        (
            None,
            Some(AudioCpp),
            row(OpenaiCompatible, Generic),
            (OpenaiCompatible, AudioCpp),
        ),
        (
            Some(OpenaiCompatible),
            None,
            row(Openai, Generic),
            (OpenaiCompatible, Generic),
        ),
        (
            Some(Openai),
            None,
            row(OpenaiCompatible, Generic),
            (Openai, Generic),
        ),
        (
            None,
            Some(Generic),
            row(Anthropic, LlamaServer),
            (Anthropic, Generic),
        ),
        // The dashboard's switch away from llama_cpp: it resets the kind.
        (
            Some(OpenaiCompatible),
            Some(Generic),
            row(LlamaCpp, LlamaServer),
            (OpenaiCompatible, Generic),
        ),
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

/// Decision 8: native `/v1/responses` is a choice on `openai_compatible` rows
/// only (and llama decision 19 before it).
#[test]
fn supports_responses_stands_on_openai_compatible_only() {
    let on = UpstreamShape {
        supports_responses: true,
        ..row(OpenaiCompatible, Generic)
    };
    // Sent on anywhere else: refused by name, for the protocol's reason.
    for (p, why) in [
        (LlamaCpp, "llama.cpp egress"),
        (Openai, "has no flag for it"),
        (Anthropic, "neither Anthropic's nor Gemini's API"),
        (Gemini, "neither Anthropic's nor Gemini's API"),
    ] {
        let e = settle(Some(p), None, Some(true), on).unwrap_err();
        assert!(
            e.contains("supports_responses") && e.contains(why),
            "{p:?}: {e}"
        );
        // Carried over from the row: switched off and said.
        let s = settle(Some(p), None, None, on).unwrap();
        assert!(!s.shape.supports_responses, "{p:?}");
        assert!(
            s.notes.iter().any(|n| n.contains("/v1/responses")),
            "{p:?}: {s:?}"
        );
        // Sent off: off, nothing to say about it.
        let s = settle(Some(p), None, Some(false), on).unwrap();
        assert!(!s.shape.supports_responses);
        assert!(
            !s.notes.iter().any(|n| n.contains("/v1/responses")),
            "{p:?}: {s:?}"
        );
    }
    let e = settle(Some(LlamaCpp), Some(LlamaServer), Some(true), on).unwrap_err();
    assert!(e.contains("supports_responses"), "{e}");
    // openai_compatible keeps what it was sent, or had.
    let s = settle(None, None, None, on).unwrap();
    assert!(s.shape.supports_responses);
    let s = settle(Some(OpenaiCompatible), None, Some(false), on).unwrap();
    assert!(!s.shape.supports_responses);
    let s = settle(None, None, Some(true), UpstreamShape::NEW_ROW).unwrap();
    assert!(s.shape.supports_responses);
}

/// Decision 13: an `openai` row on a host that is not OpenAI's is accepted
/// and said; the platform's own hosts and every other protocol are not.
#[test]
fn the_host_note_is_for_openai_rows_off_openai_hosts_only() {
    let openai = || settle(Some(Openai), None, None, UpstreamShape::NEW_ROW).unwrap();
    for host in [
        "https://api.openai.com/v1",
        "https://API.openai.com/v1/",
        "https://eu.api.openai.com/v1",
        "https://us.api.openai.com./v1",
    ] {
        assert!(openai().with_host_note(host).notes.is_empty(), "{host}");
    }
    for host in [
        "http://127.0.0.1:9/v1",
        "https://proxy.example/v1",
        "https://api.openai.com.example/v1",
        "https://notapi.openai.com/v1",
        "not a url",
    ] {
        let s = openai().with_host_note(host);
        assert_eq!(s.notes.len(), 1, "{host}");
        assert!(s.notes[0].contains("is not api.openai.com"), "{host}");
        assert!(s.message("saved").contains("openai_compatible"), "{host}");
    }
    let other = settle(Some(OpenaiCompatible), None, None, UpstreamShape::NEW_ROW).unwrap();
    assert!(other
        .with_host_note("https://proxy.example/v1")
        .notes
        .is_empty());
}

#[test]
fn a_blank_protocol_or_kind_is_not_sent_and_sd_cpp_is_refused() {
    assert_eq!(sent_protocol(None), Ok(None));
    assert_eq!(sent_protocol(Some("  ")), Ok(None));
    assert_eq!(sent_protocol(Some("llama_cpp")), Ok(Some(LlamaCpp)));
    assert_eq!(
        sent_protocol(Some("openai_compatible")),
        Ok(Some(OpenaiCompatible))
    );
    let e = sent_protocol(Some("llama")).unwrap_err();
    assert!(
        e.contains("llama_cpp") && e.contains("openai_compatible"),
        "{e}"
    );
    assert_eq!(sent_kind(Some("")), Ok(None));
    assert_eq!(sent_kind(Some("llama_server")), Ok(Some(LlamaServer)));
    assert!(sent_kind(Some("sd_cpp"))
        .unwrap_err()
        .contains("image class"));
}
