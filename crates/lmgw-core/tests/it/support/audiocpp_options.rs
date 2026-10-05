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

#![allow(dead_code)]

use serde_json::{json, Map, Value};
use wiremock::ResponseTemplate;

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

/// audio.cpp's answer to `body` when the engine of `served` refuses it as
/// conflicting; `None` when it speaks.
pub fn refusal(served: &Served, body: &Value) -> Option<ResponseTemplate> {
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
