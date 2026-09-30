//! Alias routing: `lmgw__upstream_set`, `lmgw__model_set`, and
//! `lmgw__candidate_alias_set` (the latter's schema lives in the
//! sibling [`crate::mcp::selfadmin::candidate_alias`] module; this
//! file only places its single entry in the catalog).

use crate::mcp::selfadmin::candidate_alias;
use crate::mcp::selfadmin::{bool_p, enum_p, int_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__upstream_set",
            writes: true,
            description:
                "Create, update, delete, enable or disable an upstream provider. Update is \
                 partial — pass only the fields to change, everything else is kept. Omit \
                 api_key on update to leave the stored key untouched.",
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
                        "Wire protocol the upstream speaks. Required on create.",
                        &["openai", "anthropic", "gemini"],
                    ),
                ),
                (
                    "kind",
                    enum_p(
                        "Upstream flavour. Default 'generic'.",
                        &["generic", "llama_server", "audio_cpp"],
                    ),
                ),
                (
                    "base_url",
                    str_p("Base URL, e.g. https://api.openai.com/v1. Required on create."),
                ),
                ("api_key", str_p("Provider API key. Write-only.")),
                (
                    "timeout_ms",
                    int_p("Request timeout in ms. Default 120000."),
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
                         /v1/chat/completions. Default false. Leave it off for llama-server \
                         and other local servers, which have no such endpoint.",
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
                        "Owner-set capability facts merged over what lmgw derives for \
                         /v1/models: a JSON object with optional keys capabilities \
                         (deep-merged; e.g. {\"input_modalities\":[\"text\",\"image\"],\
                         \"reasoning\":{\"kind\":\"levels\",\"levels\":[\"low\",\"high\"]}}), \
                         max_output_tokens, notes (appended). Use it only for facts the GGUF or \
                         the provider catalog does not state; the result is published with \
                         source \"owner\".",
                    ),
                ),
                (
                    "clear",
                    str_p(
                        "Field names to reset to unset, comma-separated. Only \
                         'capabilities_override' is clearable here today.",
                    ),
                ),
            ],
            required: &["action"],
        },
        candidate_alias::tool(),
    ]
}
