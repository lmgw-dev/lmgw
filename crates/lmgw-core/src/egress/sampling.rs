//! Which of the modelled sampling parameters each egress sends (openai egress
//! design §7.1): the one table both readers share — the
//! `x-lmgw-params-ignored` header, which names what a route left out of an
//! API client's request ([`crate::proxy::params_ignored`]), and the Chat,
//! which never sends what its route cannot take
//! (`web/chat_sampling.rs`). Each row is what that egress's body builder
//! writes; a change to a builder's samplers changes its row here.
//!
//! `temperature`, `top_p` and `stop` go out on every egress and are not in
//! the table; `max_tokens` and `reasoning` are not sampling.

use crate::config::Protocol;
use crate::ir::Params;

/// The table's row for one protocol: whether its egress writes each of the
/// parameters not every egress writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Takes {
    pub top_k: bool,
    pub min_p: bool,
    pub repeat_penalty: bool,
    /// `presence_penalty` and `frequency_penalty`.
    pub penalties: bool,
    pub seed: bool,
}

/// The row for `protocol`:
/// - llama.cpp and OpenAI-compatible: everything
///   ([`BodyDialect::COMPATIBLE`](crate::egress::openai_wire::BodyDialect::COMPATIBLE)
///   writes the llama samplers; most compatible servers take them, and one
///   that does not ignores or refuses them itself);
/// - the OpenAI platform: no llama samplers ([`crate::egress::openai::DIALECT`]),
///   which its Chat reference does not have and which it would refuse;
/// - Anthropic: `top_k`, no `min_p`, `repeat_penalty`, penalties or `seed`
///   (`egress::anthropic`'s body);
/// - Gemini: `top_k` and `seed`, no `min_p`, `repeat_penalty` or penalties
///   (`egress::gemini`'s `generationConfig`).
pub fn takes(protocol: Protocol) -> Takes {
    let openai_shaped = |llama: bool| Takes {
        top_k: llama,
        min_p: llama,
        repeat_penalty: llama,
        penalties: true,
        seed: true,
    };
    match protocol {
        Protocol::LlamaCpp | Protocol::OpenaiCompatible => {
            openai_shaped(crate::egress::openai_wire::BodyDialect::COMPATIBLE.llama_samplers)
        }
        Protocol::Openai => openai_shaped(crate::egress::openai::DIALECT.llama_samplers),
        Protocol::Anthropic => Takes {
            top_k: true,
            min_p: false,
            repeat_penalty: false,
            penalties: false,
            seed: false,
        },
        Protocol::Gemini => Takes {
            top_k: true,
            min_p: false,
            repeat_penalty: false,
            penalties: false,
            seed: true,
        },
    }
}

/// `params` without what `protocol`'s egress does not send, and the names of
/// what was set and left out, in a fixed order: `top_k`, `min_p`,
/// `repeat_penalty`, `presence_penalty`, `frequency_penalty`, `seed`. Unset
/// parameters are never named.
pub fn split(params: &Params, protocol: Protocol) -> (Params, Vec<&'static str>) {
    let takes = takes(protocol);
    let mut sent = params.clone();
    let mut dropped = Vec::new();
    let mut drop = |name: &'static str, set: bool, taken: bool| {
        let gone = set && !taken;
        if gone {
            dropped.push(name);
        }
        gone
    };
    if drop("top_k", sent.top_k.is_some(), takes.top_k) {
        sent.top_k = None;
    }
    if drop("min_p", sent.min_p.is_some(), takes.min_p) {
        sent.min_p = None;
    }
    if drop(
        "repeat_penalty",
        sent.repeat_penalty.is_some(),
        takes.repeat_penalty,
    ) {
        sent.repeat_penalty = None;
    }
    if drop(
        "presence_penalty",
        sent.presence_penalty.is_some(),
        takes.penalties,
    ) {
        sent.presence_penalty = None;
    }
    if drop(
        "frequency_penalty",
        sent.frequency_penalty.is_some(),
        takes.penalties,
    ) {
        sent.frequency_penalty = None;
    }
    if drop("seed", sent.seed.is_some(), takes.seed) {
        sent.seed = None;
    }
    (sent, dropped)
}

/// [`split`]'s names alone: what `protocol`'s egress leaves out of `params`.
pub fn dropped(params: &Params, protocol: Protocol) -> Vec<&'static str> {
    split(params, protocol).1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Params {
        Params {
            temperature: Some(0.5),
            top_p: Some(0.9),
            top_k: Some(40),
            min_p: Some(0.05),
            repeat_penalty: Some(1.1),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            seed: Some(7),
            stop: vec!["END".into()],
            ..Default::default()
        }
    }

    #[test]
    fn llama_and_compatible_rows_take_everything() {
        for p in [Protocol::LlamaCpp, Protocol::OpenaiCompatible] {
            let (sent, dropped) = split(&all(), p);
            assert_eq!(sent, all(), "{p:?}");
            assert!(dropped.is_empty(), "{p:?}");
        }
    }

    #[test]
    fn the_platform_drops_the_llama_samplers_only() {
        let (sent, dropped) = split(&all(), Protocol::Openai);
        assert_eq!(dropped, ["top_k", "min_p", "repeat_penalty"]);
        assert_eq!(sent.presence_penalty, Some(0.1));
        assert_eq!(sent.seed, Some(7));
        assert_eq!(sent.stop, ["END"]);
    }

    #[test]
    fn anthropic_takes_top_k_and_stop_but_no_penalties_or_seed() {
        let (sent, dropped) = split(&all(), Protocol::Anthropic);
        assert_eq!(
            dropped,
            [
                "min_p",
                "repeat_penalty",
                "presence_penalty",
                "frequency_penalty",
                "seed"
            ]
        );
        assert_eq!(sent.top_k, Some(40));
        assert_eq!(sent.top_p, Some(0.9));
        assert_eq!(sent.stop, ["END"]);
    }

    #[test]
    fn gemini_takes_seed_and_top_k() {
        let (sent, dropped) = split(&all(), Protocol::Gemini);
        assert_eq!(
            dropped,
            [
                "min_p",
                "repeat_penalty",
                "presence_penalty",
                "frequency_penalty"
            ]
        );
        assert_eq!(sent.seed, Some(7));
        assert_eq!(sent.top_k, Some(40));
    }

    #[test]
    fn unset_params_are_never_named() {
        for p in [Protocol::Anthropic, Protocol::Gemini, Protocol::Openai] {
            assert!(dropped(&Params::default(), p).is_empty(), "{p:?}");
        }
    }

    /// Each row is what the body builder really writes: every parameter the
    /// table says a protocol takes is in its chat body, every one it drops is
    /// not.
    #[test]
    fn the_table_matches_the_body_builders() {
        use crate::config::{Upstream, UpstreamKind};
        use crate::ir::{ChatRequest, Message, Role};
        let ir = ChatRequest {
            model_alias: "m".into(),
            messages: vec![Message::text(Role::User, "hi")],
            params: Params::default(),
            tools: vec![],
            tool_choice: None,
            stream: false,
            passthrough: Default::default(),
            response_format: None,
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
            context_management: None,
            compaction: None,
            thinking_display: None,
        };
        let http = reqwest::Client::new();
        for p in [
            Protocol::LlamaCpp,
            Protocol::OpenaiCompatible,
            Protocol::Openai,
            Protocol::Anthropic,
            Protocol::Gemini,
        ] {
            let up = Upstream {
                id: 1,
                name: "up".into(),
                protocol: p,
                kind: UpstreamKind::Generic,
                base_url: "http://upstream.example/v1".into(),
                api_key: Some("sk".into()),
                extra_headers: vec![],
                timeout_ms: 1000,
                enabled: true,
                expose_all: false,
                expose_prefix: String::new(),
                supports_responses: false,
                llama: None,
            };
            let req = crate::egress::for_protocol(p)
                .build_chat(&http, &up, "tgt", &ir, &all(), false)
                .unwrap()
                .build()
                .unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
            let text = body.to_string();
            let dropped = dropped(&all(), p);
            for (name, wire) in [
                ("top_k", ["\"top_k\"", "\"topK\""]),
                ("min_p", ["\"min_p\"", "\"min_p\""]),
                (
                    "repeat_penalty",
                    ["\"repeat_penalty\"", "\"repeat_penalty\""],
                ),
                (
                    "presence_penalty",
                    ["\"presence_penalty\"", "\"presencePenalty\""],
                ),
                (
                    "frequency_penalty",
                    ["\"frequency_penalty\"", "\"frequencyPenalty\""],
                ),
                ("seed", ["\"seed\"", "\"seed\""]),
            ] {
                let sent = wire.iter().any(|w| text.contains(w));
                assert_eq!(sent, !dropped.contains(&name), "{p:?} {name}: {text}");
            }
        }
    }
}
