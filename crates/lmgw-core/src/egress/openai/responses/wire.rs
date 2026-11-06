//! Which wire one platform send takes (openai egress design §7.2.1): the
//! Responses wire by default, the Chat wire when the send asks for something
//! only Chat Completions has, so nothing is dropped (the rule "a gateway
//! translates, never refuses or drops"). A send that needs both wires (tools
//! on a model the Chat wire refuses them for, plus `stop`) takes the Chat
//! wire, and OpenAI's own refusal reaches the client (decided, §7.2.5).
//!
//! Checked 2026-10-24 against the create reference
//! (<https://developers.openai.com/api/reference/resources/responses/methods/create.md>):
//! the Responses request has no `stop`, `seed`, `presence_penalty` or
//! `frequency_penalty`, and its input content is `input_text`, `input_image`
//! and `input_file` only, no audio.

use crate::ir::{ChatRequest, ContentPart, Params};

/// The wire a platform send leaves on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// `POST {base}/responses` (§7.2.2).
    Responses,
    /// `POST {base}/chat/completions`, the body every OpenAI-shaped egress
    /// shares (§7.1).
    Chat,
}

/// The passthrough keys that take the Chat wire (§7.2.1), which forwards
/// them verbatim: keys only Chat Completions has, the design's list plus the
/// legacy function-calling keys (`functions`, `function_call`), which Chat
/// Completions still documents as deprecated and the Responses request does
/// not have. `top_logprobs` is a Responses field too (the create
/// reference), but there it only counts beside `include:
/// ["message.output_text.logprobs"]`, where a chat client's goes beside its
/// `logprobs: true`, and its log probabilities come back in the
/// `output_text` part, which no chat-shaped answer reads yet: it keeps the
/// Chat wire with `logprobs`.
pub const CHAT_ONLY: [&str; 10] = [
    "n",
    "logit_bias",
    "logprobs",
    "top_logprobs",
    "audio",
    "modalities",
    "prediction",
    "web_search_options",
    "functions",
    "function_call",
];

/// What makes a send take the Chat wire: the first thing found, for the log
/// line that says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatOnly {
    /// A modelled field the Responses request has no spelling for.
    Field(&'static str),
    /// An `input_audio` part in a message.
    AudioInput,
    /// A passthrough key in [`CHAT_ONLY`].
    Passthrough(&'static str),
    /// An output cap below [`MIN_OUTPUT_TOKENS`], which the Responses API
    /// refuses and Chat Completions takes: the caller's cap goes as it is,
    /// on the Chat wire, never raised to the minimum.
    CapBelowMinimum(u32),
}

/// The smallest `max_output_tokens` the Responses API takes: below it OpenAI
/// answers `400 integer_below_min_value` ("must be >= 16", reported against
/// the Responses API; the create reference names no minimum, checked
/// 2026-10-25), where Chat Completions takes the same cap. A token-limit
/// probe (`max_tokens: 1`) or a short classifier is a cap like that.
pub const MIN_OUTPUT_TOKENS: u32 = 16;

impl ChatOnly {
    /// The words a log line uses for it.
    pub fn describe(self) -> String {
        match self {
            Self::Field(f) => format!("`{f}`, which the Responses API has no spelling for"),
            Self::AudioInput => {
                "an audio input part, which the Responses input does not take".into()
            }
            Self::Passthrough(k) => format!("`{k}`, a key only Chat Completions has"),
            Self::CapBelowMinimum(n) => format!(
                "`max_tokens` {n}, below the {MIN_OUTPUT_TOKENS} the Responses API's \
                 max_output_tokens takes"
            ),
        }
    }
}

/// Why the send of `ir` with its final `params` (every tier merged) needs
/// the Chat wire, `None` when the Responses wire carries all of it.
pub fn chat_only(ir: &ChatRequest, params: &Params) -> Option<ChatOnly> {
    let fields = [
        ("stop", !params.stop.is_empty()),
        ("seed", params.seed.is_some()),
        ("presence_penalty", params.presence_penalty.is_some()),
        ("frequency_penalty", params.frequency_penalty.is_some()),
    ];
    if let Some((f, _)) = fields.iter().find(|(_, set)| *set) {
        return Some(ChatOnly::Field(f));
    }
    let audio = ir
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .any(|p| matches!(p, ContentPart::Audio { .. }));
    if audio {
        return Some(ChatOnly::AudioInput);
    }
    if let Some(k) = CHAT_ONLY.iter().find(|k| ir.passthrough.contains_key(**k)) {
        return Some(ChatOnly::Passthrough(k));
    }
    params
        .max_tokens
        .filter(|n| *n < MIN_OUTPUT_TOKENS)
        .map(ChatOnly::CapBelowMinimum)
}

/// The wire the send of `ir` with `params` takes.
pub fn wire_for(ir: &ChatRequest, params: &Params) -> Wire {
    match chat_only(ir, params) {
        Some(_) => Wire::Chat,
        None => Wire::Responses,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ir::{Message, Role};

    fn ir() -> ChatRequest {
        serde_json::from_value(json!({
            "model_alias": "a",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        }))
        .unwrap()
    }

    #[test]
    fn a_plain_send_takes_the_responses_wire() {
        assert_eq!(wire_for(&ir(), &Params::default()), Wire::Responses);
        // What both wires spell is no reason: the samplers the Responses API
        // has, the cap, reasoning, and a key it shares.
        let mut r = ir();
        r.passthrough.insert("user".into(), json!("u1"));
        r.passthrough.insert("service_tier".into(), json!("flex"));
        let p = Params {
            temperature: Some(0.2),
            top_p: Some(0.9),
            max_tokens: Some(64),
            top_k: Some(20),
            ..Default::default()
        };
        assert_eq!(chat_only(&r, &p), None);
    }

    #[test]
    fn each_modelled_field_without_a_responses_spelling_takes_the_chat_wire() {
        let cases = [
            (
                Params {
                    stop: vec!["END".into()],
                    ..Default::default()
                },
                "stop",
            ),
            (
                Params {
                    seed: Some(7),
                    ..Default::default()
                },
                "seed",
            ),
            (
                Params {
                    presence_penalty: Some(0.5),
                    ..Default::default()
                },
                "presence_penalty",
            ),
            (
                Params {
                    frequency_penalty: Some(0.5),
                    ..Default::default()
                },
                "frequency_penalty",
            ),
        ];
        for (p, field) in cases {
            assert_eq!(chat_only(&ir(), &p), Some(ChatOnly::Field(field)));
            assert_eq!(wire_for(&ir(), &p), Wire::Chat, "{field}");
        }
    }

    #[test]
    fn an_audio_part_takes_the_chat_wire() {
        let mut r = ir();
        r.messages.push(Message {
            role: Role::User,
            content: vec![ContentPart::Audio {
                mime: "audio/wav".into(),
                data: "UklGRg==".into(),
            }],
        });
        assert_eq!(
            chat_only(&r, &Params::default()),
            Some(ChatOnly::AudioInput)
        );
    }

    #[test]
    fn every_chat_only_key_takes_the_chat_wire() {
        for key in CHAT_ONLY {
            let mut r = ir();
            r.passthrough.insert(key.into(), json!(1));
            assert_eq!(
                chat_only(&r, &Params::default()),
                Some(ChatOnly::Passthrough(key)),
                "{key}"
            );
        }
    }

    /// A cap below the Responses minimum takes the Chat wire with its exact
    /// value; the minimum itself stays on the Responses wire.
    #[test]
    fn a_cap_below_the_responses_minimum_takes_the_chat_wire() {
        for n in [1, 5, 15] {
            let p = Params {
                max_tokens: Some(n),
                ..Default::default()
            };
            assert_eq!(chat_only(&ir(), &p), Some(ChatOnly::CapBelowMinimum(n)));
        }
        let p = Params {
            max_tokens: Some(16),
            ..Default::default()
        };
        assert_eq!(wire_for(&ir(), &p), Wire::Responses);
    }

    /// The modelled fields are read from the send's final params, an alias
    /// default included, not from the request's own.
    #[test]
    fn the_final_params_decide_not_the_request_s_own() {
        let mut r = ir();
        r.params.seed = Some(1);
        assert_eq!(wire_for(&r, &Params::default()), Wire::Responses);
    }
}
