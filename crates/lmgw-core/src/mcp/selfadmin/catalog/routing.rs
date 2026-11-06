//! Alias routing: `lmgw__upstream_set`, `lmgw__model_set`, and
//! `lmgw__candidate_alias_set` (the latter's schema lives in the
//! sibling [`crate::mcp::selfadmin::candidate_alias`] module; this
//! file only places its single entry in the catalog).

use crate::mcp::selfadmin::candidate_alias;
use crate::mcp::selfadmin::{bool_p, enum_p, fallback_p, int_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__upstream_set",
            writes: true,
            description:
                "Create, update, delete, enable or disable an upstream provider. Update is \
                 partial — pass only the fields to change, everything else is kept. Omit \
                 api_key on update to leave the stored key untouched. A call that moves \
                 base_url to another address (update, enable or disable) must pass the \
                 api_key for that address in the same call when the upstream holds one, or \
                 it is refused and nothing changes: a stored key is never sent to a host it \
                 was not given for. An upstream that sends extra headers moves on the \
                 dashboard only.",
            props: vec![
                (
                    "action",
                    enum_p(
                        "What to do.",
                        &["create", "update", "delete", "enable", "disable"],
                    ),
                ),
                (
                    "id",
                    int_p("Upstream id. Required for everything except create."),
                ),
                ("name", str_p("Display name, unique.")),
                (
                    "protocol",
                    enum_p(
                        "Wire protocol the upstream speaks. Required on create. 'openai' is the \
                         OpenAI platform only (api.openai.com or a regional *.api.openai.com \
                         host), always kind 'generic': lmgw sends it OpenAI's own dialect, and \
                         another host is accepted with a note. 'openai_compatible' is every \
                         other OpenAI-shaped provider (OpenRouter, Kilo, Groq, Gemini's \
                         OpenAI-compatible endpoint, vLLM, an external audio.cpp server). \
                         'llama_cpp' is llama.cpp's own server (llama-server, ik_llama.cpp) and \
                         is always kind 'llama_server'; 'openai' or 'openai_compatible' with \
                         kind 'llama_server' is refused, use protocol 'llama_cpp'.",
                        &[
                            "openai",
                            "openai_compatible",
                            "anthropic",
                            "gemini",
                            "llama_cpp",
                        ],
                    ),
                ),
                (
                    "kind",
                    enum_p(
                        "Upstream flavour. Default 'generic'. 'llama_server' goes with protocol \
                         'llama_cpp' (or 'anthropic', for llama-server's /v1/messages) and \
                         marks the upstream local and free; leave it out with 'llama_cpp'. \
                         'audio_cpp' (an external audio.cpp server) goes with \
                         'openai_compatible'. Protocol 'openai' is always 'generic'. Switching \
                         the protocol to openai, openai_compatible or gemini alone makes a \
                         llama_server 'generic', and switching it to openai alone makes an \
                         audio_cpp row 'generic'.",
                        &["generic", "llama_server", "audio_cpp"],
                    ),
                ),
                (
                    "base_url",
                    str_p(
                        "Base URL, e.g. https://api.openai.com/v1. Required on create. \
                         Moving it needs api_key in the same call when the upstream holds \
                         a key.",
                    ),
                ),
                ("api_key", str_p("Provider API key. Write-only.")),
                (
                    "timeout_ms",
                    int_p(
                        "Timeout in ms of every request to it: its sends and its catalog \
                         (models-list) reads wait that long for an answer, and a streamed \
                         answer may go that long without an event before lmgw ends it \
                         with an error (keep-alives count only before the answer shows \
                         and while the model thinks out of sight). 0 means no bound. \
                         Default 120000.",
                    ),
                ),
                ("enabled", bool_p("Whether the upstream is usable.")),
                (
                    "expose_all",
                    bool_p("Pass the upstream's whole catalog through without per-model aliases."),
                ),
                (
                    "expose_prefix",
                    str_p("Namespace for passthrough models, e.g. 'groq' -> groq/<model>."),
                ),
                (
                    "supports_responses",
                    bool_p(
                        "The upstream implements OpenAI's /v1/responses itself, so the gateway \
                         forwards that route verbatim instead of synthesizing it from \
                         /v1/chat/completions. 'openai_compatible' only; default false. \
                         Implied on 'openai' (the OpenAI platform forwards it without the \
                         flag), and refused on every other protocol: lmgw synthesizes \
                         /v1/responses there. Most local servers have no such endpoint.",
                    ),
                ),
                (
                    "catalog_all_modalities",
                    bool_p(
                        "Ask the upstream's catalog for every output modality \
                         (GET …/models?output_modalities=all), so OpenRouter lists its decision, \
                         embedding, rerank, speech, transcription, image and video models as \
                         well as its chat models. Default true; only the catalog request of an \
                         'openai_compatible' upstream of kind 'generic' carries it. Some \
                         endpoints refuse \
                         the parameter: Gemini's OpenAI-compatible one answers 400, so with \
                         this on its catalog fails, and the error names this option. Set it \
                         false for such an upstream.",
                    ),
                ),
            ],
            required: &["action"],
        },
        Builtin {
            name: "lmgw__model_set",
            writes: true,
            description:
                "Create, update, delete, enable or disable a model alias — the friendly name \
                 clients request, mapped onto an upstream and its real model id. Identify an \
                 existing alias by id or by its current alias name. This maps a name onto a \
                 REMOTE upstream; to configure a local llama.cpp GGUF use \
                 lmgw__local_model_set instead.",
            props: vec![
                (
                    "action",
                    enum_p(
                        "What to do.",
                        &["create", "update", "delete", "enable", "disable"],
                    ),
                ),
                ("id", int_p("Alias row id.")),
                (
                    "alias",
                    str_p(
                        "The client-facing model name. On create this is the new alias; \
                         otherwise it selects the row (pass id as well to rename).",
                    ),
                ),
                (
                    "upstream",
                    str_p("Upstream to route to, by name or numeric id."),
                ),
                (
                    "upstream_model",
                    str_p("The real model id at the upstream, e.g. gpt-4o-mini."),
                ),
                ("enabled", bool_p("Whether the alias resolves.")),
                (
                    "capabilities_override",
                    str_p(
                        "Capability facts set on the row, merged over what lmgw derives for \
                         /v1/models: a JSON object with optional keys capabilities \
                         (deep-merged; e.g. {\"input_modalities\":[\"text\",\"image\"],\
                         \"reasoning\":{\"kind\":\"levels\",\"levels\":[\"low\",\"high\"]}}), \
                         max_output_tokens, notes (appended). Use it only for facts the GGUF or \
                         the provider catalog does not state; the result is published with \
                         source \"owner\".",
                    ),
                ),
                (
                    "fallback",
                    fallback_p(
                        "what this alias's upstream model is: a chat model for a chat model, \
                         an embedder for an embedder",
                    ),
                ),
                (
                    "clear",
                    str_p(
                        "Field names to reset to unset, comma-separated: \
                         'capabilities_override', 'fallback'.",
                    ),
                ),
            ],
            required: &["action"],
        },
        candidate_alias::tool(),
        Builtin {
            name: "lmgw__upstream_quota_clear",
            writes: true,
            description:
                "Forget that an upstream is out of quota, so the next request goes to it again \
                 (Check now on the Upstreams page). lmgw remembers an upstream that answered \
                 insufficient_quota (lmgw__upstreams shows it as quota): until its next check, \
                 models and aliases on it that have a fallback answer with the fallback. Answers \
                 what it did, also when nothing was remembered.",
            props: vec![("id", int_p("Upstream id, from lmgw__upstreams."))],
            required: &["id"],
        },
    ]
}
