//! Each upstream's refusals, as captured (design §9.5), read by its egress's
//! classifier: the code each fixture expects, the provider's message kept
//! verbatim, and nothing classified for a request that asked for no JSON.

use std::path::PathBuf;

use lmgw_core::config::Protocol;
use lmgw_core::egress::{chat_error, for_protocol};
use lmgw_core::ir::FormatAsk;
use serde_json::Value;

struct Fixture {
    name: String,
    protocol: Protocol,
    status: u16,
    body: Vec<u8>,
    message: Option<String>,
    expect: Option<String>,
}

fn fixtures() -> Vec<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/structured_output");
    let mut out: Vec<Fixture> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
            let protocol = match v["upstream"].as_str().unwrap() {
                "openai" => Protocol::Openai,
                "openai_compatible" => Protocol::OpenaiCompatible,
                "anthropic" => Protocol::Anthropic,
                "gemini" => Protocol::Gemini,
                "llama_cpp" => Protocol::LlamaCpp,
                other => panic!("{p:?}: unknown upstream {other}"),
            };
            assert!(
                v["captured"].as_str().is_some_and(|c| !c.is_empty()),
                "{p:?} says where it came from"
            );
            Fixture {
                name: p.file_stem().unwrap().to_string_lossy().into_owned(),
                protocol,
                status: v["status"].as_u64().unwrap() as u16,
                body: serde_json::to_vec(&v["body"]).unwrap(),
                message: v["body"]["error"]["message"].as_str().map(String::from),
                expect: v["expect"].as_str().map(String::from),
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    assert!(
        out.len() >= 10,
        "the captured corpus is there: {}",
        out.len()
    );
    out
}

/// Every fixture under its own upstream's egress — and llama.cpp's under a
/// generic OpenAI-compatible row too, as llama-swap or a remote box in front
/// of llama-server serves them — and OpenAI's own under a compatible row too,
/// as an aggregator in front of OpenAI relays them (openai egress design §8).
#[test]
fn every_captured_refusal_reads_as_its_code_with_the_message_kept() {
    for f in fixtures() {
        let mut protocols = vec![f.protocol];
        if matches!(f.protocol, Protocol::LlamaCpp | Protocol::Openai) {
            protocols.push(Protocol::OpenaiCompatible);
        }
        for p in protocols {
            let e = chat_error(
                for_protocol(p),
                f.status,
                &Default::default(),
                &f.body,
                FormatAsk::Json,
            );
            match &f.expect {
                Some(code) => {
                    assert_eq!(e.kind(), code, "{} on {p:?}: {e:?}", f.name);
                    assert_eq!(
                        Some(e.to_string()),
                        f.message,
                        "{}: the provider's message, verbatim",
                        f.name
                    );
                    assert_eq!(e.http_status().as_u16(), 400, "{}", f.name);
                    assert_eq!(e.to_openai_json()["error"]["param"], "response_format");
                    assert_eq!(e.to_responses_json()["error"]["param"], "text.format");
                    assert_eq!(
                        e.to_anthropic_json()["error"]["type"],
                        "invalid_request_error"
                    );
                }
                None => assert!(
                    !e.about_format(),
                    "{}: not a structured-output refusal, kept generic: {e:?}",
                    f.name
                ),
            }
        }
    }
}

/// Without a JSON format in the request no body is read as one (§11.8):
/// llama.cpp's converter text also covers tool schemas.
#[test]
fn a_request_without_a_json_format_keeps_every_refusal_generic() {
    for f in fixtures() {
        let e = chat_error(
            for_protocol(f.protocol),
            f.status,
            &Default::default(),
            &f.body,
            FormatAsk::Nothing,
        );
        assert!(!e.about_format(), "{}: {e:?}", f.name);
    }
}

/// A type lmgw does not model (`{"type": "regex"}`) lets a refusal of the
/// type itself through, never a schema refusal (§11.8): those texts also
/// cover tool-parameter schemas.
#[test]
fn an_unmodelled_type_reads_only_the_type_refusals() {
    for f in fixtures() {
        let e = chat_error(
            for_protocol(f.protocol),
            f.status,
            &Default::default(),
            &f.body,
            FormatAsk::Type,
        );
        match f.expect.as_deref() {
            Some("response_format_unsupported") => {
                assert_eq!(e.kind(), "response_format_unsupported", "{}: {e:?}", f.name)
            }
            _ => assert!(!e.about_format(), "{}: {e:?}", f.name),
        }
    }
}

/// A classifier never reads another upstream's texts as its own: OpenAI's
/// body on the Anthropic egress, Anthropic's on Gemini's, and so on, stay
/// generic unless the texts are shared: the compatible and llama.cpp
/// egresses read every OpenAI-shaped text (OpenAI's, an aggregator's,
/// llama.cpp's), the OpenAI platform OpenAI's own shape only (openai egress
/// design §8) — so llama.cpp's and an aggregator's bodies stay generic there.
#[test]
fn a_classifier_reads_only_its_own_upstreams_texts() {
    let others = [
        Protocol::Anthropic,
        Protocol::Gemini,
        Protocol::Openai,
        Protocol::OpenaiCompatible,
        Protocol::LlamaCpp,
    ];
    let tolerant = |p: Protocol| matches!(p, Protocol::OpenaiCompatible | Protocol::LlamaCpp);
    let openai_shaped = |p: Protocol| {
        matches!(
            p,
            Protocol::Openai | Protocol::OpenaiCompatible | Protocol::LlamaCpp
        )
    };
    for f in fixtures().into_iter().filter(|f| f.expect.is_some()) {
        for p in others {
            let own = p == f.protocol || (tolerant(p) && openai_shaped(f.protocol));
            if own {
                continue;
            }
            let e = chat_error(
                for_protocol(p),
                f.status,
                &Default::default(),
                &f.body,
                FormatAsk::Json,
            );
            assert!(!e.about_format(), "{} read by {p:?}: {e:?}", f.name);
        }
    }
}
