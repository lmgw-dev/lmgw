//! audio.cpp's request options as its engines see them, for the fake
//! containers of [`super::gpu_world`] and [`super::audio_world`] (R2).
//!
//! audio.cpp makes a request's `instructions` the engine's
//! `options.instruction` (`build_speech_request`), fills in the row's
//! `default_request_options` from its mounted `server.json` under every key
//! the request does not set itself (`apply_default_request_options`), and
//! the families that read both synonyms through `find_option_match` refuse
//! a request whose `instruction` and `instruct` both say something and
//! differ. The exception reaches its HTTP layer uncaught, which answers 500
//! `server_error` with the engine's message. The fakes answer the same, so
//! a request lmgw shapes into that conflict fails in a test as it would on
//! a real container, instead of hiding behind a fake that speaks anything.
//!
//! Supertonic's engine fails the same way on a character its package's
//! `unicode_indexer` has no entry for, and on an empty text
//! ([`supertonic_refusal`]).

#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::OnceLock;

use serde_json::{json, Map, Value};
use unicode_normalization::char::decompose_compatible;
use wiremock::ResponseTemplate;

use super::audiocpp_gguf::SUPERTONIC_INDEXER;

/// The families whose engines read `instruction` and `instruct` together,
/// with the key order each passes — the order the engine's message names
/// them in.
const BOTH_KEYS: [(&str, [&str; 2]); 5] = [
    ("qwen3_tts", ["instruction", "instruct"]),
    ("dots_tts", ["instruction", "instruct"]),
    ("moss_voicegen", ["instruct", "instruction"]),
    ("moss_tts_v15", ["instruct", "instruction"]),
    ("moss_ttsd", ["instruct", "instruction"]),
];

/// The model a container serves, as its `server.json` says.
#[derive(Debug, Clone, Default)]
pub struct Served {
    pub family: String,
    pub defaults: Map<String, Value>,
}

/// What a `podman run` serves, read from the `server.json` mounted at
/// `/config`; `None` for a container without one (llama-server, sd-server).
pub fn served(args: &[String]) -> Option<Served> {
    let dir = args.iter().find_map(|a| a.strip_suffix(":/config:ro"))?;
    let raw = std::fs::read(std::path::Path::new(dir).join("server.json")).ok()?;
    let cfg: Value = serde_json::from_slice(&raw).ok()?;
    let model = cfg["models"].get(0)?;
    Some(Served {
        family: model["family"].as_str().unwrap_or_default().to_string(),
        defaults: model
            .get("default_request_options")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
    })
}

/// The options the engine of `served` sees for the speech request `body`.
pub fn engine_options(served: &Served, body: &Value) -> Map<String, Value> {
    let mut options = served.defaults.clone();
    if let Some(o) = body.get("options").and_then(Value::as_object) {
        options.extend(o.clone());
    }
    if let Some(i) = body.get("instructions") {
        options.insert("instruction".into(), i.clone());
    }
    options
}

/// audio.cpp's answer to `body` when the engine of `served` refuses it —
/// as conflicting, or for a character it cannot say; `None` when it speaks.
pub fn refusal(served: &Served, body: &Value) -> Option<ResponseTemplate> {
    conflict(served, body).or_else(|| supertonic_refusal(served, body))
}

/// The characters Supertonic's tokenizer rewrites before its lookup
/// (`supertonic/tokenizer_text.cpp` `preprocess`, audio.cpp 75d0294): its
/// replacement table, the symbols it removes, and what `\s` matches.
const SUPERTONIC_REWRITES: &str =
    "\u{2013}\u{2011}\u{2014}_\u{201C}\u{201D}\u{2018}\u{2019}\u{00B4}`[]|/#\u{2192}\u{2190}@\
     \u{2665}\u{2606}\u{2661}\u{00A9}\\\t\n\u{0B}\u{0C}\r ";

/// Supertonic's engine on `body`'s `input`: an empty one is refused
/// (`SupertonicSession::validate_request`, "Supertonic requires --text
/// input"); otherwise (`SupertonicTextTokenizer::encode`), after its
/// rewrites, every codepoint of the NFKD-normalised text is looked up in the
/// package's `unicode_indexer` — here the real one, trimmed
/// ([`SUPERTONIC_INDEXER`]) — and the first without an entry fails the
/// request with a 500.
fn supertonic_refusal(served: &Served, body: &Value) -> Option<ResponseTemplate> {
    if served.family != "supertonic" {
        return None;
    }
    if body.get("input").and_then(Value::as_str) == Some("") {
        return Some(ResponseTemplate::new(500).set_body_json(json!({"error": {
            "message": "Supertonic requires --text input",
            "type": "server_error",
        }})));
    }
    static INDEXER: OnceLock<HashSet<u32>> = OnceLock::new();
    let indexer = INDEXER.get_or_init(|| {
        let table: Map<String, Value> = serde_json::from_str(SUPERTONIC_INDEXER).unwrap();
        table.keys().map(|k| k.parse().unwrap()).collect()
    });
    let input = body.get("input").and_then(Value::as_str)?;
    let mut missing = None;
    for c in input.chars().filter(|c| !SUPERTONIC_REWRITES.contains(*c)) {
        decompose_compatible(c, |d| {
            if missing.is_none() && !indexer.contains(&u32::from(d)) {
                missing = Some(u32::from(d));
            }
        });
    }
    missing.map(|cp| {
        ResponseTemplate::new(500).set_body_json(json!({"error": {
            "message": format!("Supertonic unicode indexer has no entry for codepoint {cp}"),
            "type": "server_error",
        }}))
    })
}

/// The conflicting-options refusal of the families in [`BOTH_KEYS`].
fn conflict(served: &Served, body: &Value) -> Option<ResponseTemplate> {
    let (_, [first, second]) = BOTH_KEYS.iter().find(|(f, _)| *f == served.family)?;
    let options = engine_options(served, body);
    // Every option is a string to the engine; empty is unset.
    let said = |k: &str| {
        options
            .get(k)
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
            .filter(|t| !t.is_empty())
    };
    let (a, b) = (said(first)?, said(second)?);
    (a != b).then(|| {
        ResponseTemplate::new(500).set_body_json(json!({"error": {
            "message": format!("conflicting option values for {first} and {second}"),
            "type": "server_error",
        }}))
    })
}
