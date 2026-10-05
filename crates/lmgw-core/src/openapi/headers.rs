//! The one `x-lmgw-*` header table (api-docs design §4.9): every header this
//! gateway reads or stamps, in one place, so the OpenAPI description's header
//! parameters and response headers, and `/v1/models`' `lmgw.headers` block,
//! all read from the same list instead of three hand-maintained ones drifting
//! apart.
//!
//! Filled in WP2. `tests/it/openapi_headers.rs` is the drift guard: every
//! `"x-lmgw-*"` string literal anywhere in `src/` names a row here, and
//! nothing else. The prose on the `Client`-audience rows is moved verbatim
//! from the old hand-written `/v1/models` block in `server.rs` — WP2 does not
//! paraphrase it, only widens the reasoning trio's route list to name
//! `/v1/messages/count_tokens` alongside the three routes it already named.

/// Which side of the wire a header travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Request,
    Response,
}

/// Who is meant to read or send a header — `x-lmgw-audience`. `Internal`
/// headers (e.g. `x-lmgw-face`) are lmgw's own, set on requests it proxies
/// and never read from a client; they are in the table for
/// `every_x_lmgw_literal_is_in_the_header_table` (WP2) to find, but carry no
/// parameter and are not in [`headers_block`]'s `/v1/models` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Client,
    Agent,
    Owner,
    Internal,
}

impl Audience {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Client => lmgw_api_types::openapi_ext::audience::CLIENT,
            Self::Agent => lmgw_api_types::openapi_ext::audience::AGENT,
            Self::Owner => lmgw_api_types::openapi_ext::audience::OWNER,
            Self::Internal => "internal",
        }
    }
}

/// Which routes carry a header.
#[derive(Debug, Clone, Copy)]
pub enum Scope {
    /// Exactly these `(method, path)` rows.
    Routes(&'static [(&'static str, &'static str)]),
    /// Every inference-plane route.
    AllInference,
    /// Set by lmgw itself, never advertised as a per-route parameter
    /// (`x-lmgw-face`).
    None,
}

/// One header's declared value shape.
#[derive(Debug, Clone, Copy)]
pub enum HeaderSchema {
    Enum(&'static [&'static str]),
    Integer,
    Text,
}

/// One row of the table.
#[derive(Debug, Clone, Copy)]
pub struct LmgwHeader {
    /// The literal header name, as it appears on the wire and in the
    /// constant it is defined next to.
    pub name: &'static str,
    pub direction: Direction,
    pub audience: Audience,
    pub scope: Scope,
    pub schema: HeaderSchema,
    /// The `/v1/models` `lmgw.headers` prose (§4.9).
    pub description: &'static str,
}

// ---------------------------------------------------------------------------
// Route groups a header's `Scope::Routes` draws on (§4.9's Scope column) —
// named once so the table below reads as which group applies, not eighteen
// repeated tuples. Paths are exactly as `CAPABILITY_TABLE` spells them
// (`{*id}` wildcards kept), since that is what `DocRoute::path` carries and
// [`super::params::header_params`] matches against.
// ---------------------------------------------------------------------------

/// The reasoning trio's routes: every `/v1` request a per-request thinking
/// control is read on. On `/v1/messages/count_tokens` the control shapes what
/// is counted (`proxy/count_messages.rs`).
const REASONING_ROUTES: &[(&str, &str)] = &[
    ("POST", "/v1/chat/completions"),
    ("POST", "/v1/messages"),
    ("POST", "/v1/messages/count_tokens"),
    ("POST", "/v1/responses"),
];

/// Every route `gate::open` can answer on behalf of, so a fallback, a
/// candidate's model, or (were it not narrower, see [`RUNG_ROUTES`]) a rung
/// can show up on its response.
const GATED_ROUTES: &[(&str, &str)] = &[
    ("POST", "/v1/chat/completions"),
    ("POST", "/v1/messages"),
    ("POST", "/v1/messages/count_tokens"),
    ("POST", "/v1/responses"),
    ("POST", "/v1/completions"),
    ("POST", "/v1/embeddings"),
    ("POST", "/v1/rerank"),
    ("POST", "/v1/count_tokens"),
    ("POST", "/tokenize"),
    ("POST", "/v1/audio/speech"),
    ("POST", "/v1/audio/transcriptions"),
    ("POST", "/v1/audio/transcriptions/details"),
    ("POST", "/v1/audio/alignments"),
    ("GET", "/v1/audio/voices"),
    ("POST", "/v1/images/generations"),
    ("POST", "/v1/images/edits"),
    ("POST", "/v1/tasks/run"),
    ("POST", "/v1/tasks/stream"),
];

/// Routes whose response can report a reasoning control the route could not
/// express, or a `max_tokens` lmgw filled in or raised — the three chat-style
/// routes, but not `/v1/messages/count_tokens` (no `max_tokens` there) and
/// not `/v1/completions` (no reasoning there either; it can still be
/// *clamped*, see [`RUNG_ROUTES`]).
const REASONING_IGNORED_ROUTES: &[(&str, &str)] = &[
    ("POST", "/v1/chat/completions"),
    ("POST", "/v1/messages"),
    ("POST", "/v1/responses"),
];

/// Routes a ladder rung, or a clamped (as opposed to defaulted/raised)
/// `max_tokens`, can be reported on: the three chat-style routes plus
/// `/v1/completions`, which has no reasoning but does run on ladder rows.
const RUNG_ROUTES: &[(&str, &str)] = &[
    ("POST", "/v1/chat/completions"),
    ("POST", "/v1/messages"),
    ("POST", "/v1/responses"),
    ("POST", "/v1/completions"),
];

/// Routes `x-lmgw-count-approximate` can appear on (§5.1): the two counters
/// that answer a number. `/tokenize` answers the backend's own token ids,
/// which are never approximate.
const COUNT_APPROXIMATE_ROUTES: &[(&str, &str)] = &[
    ("POST", "/v1/count_tokens"),
    ("POST", "/v1/messages/count_tokens"),
];

/// `x-lmgw-speech`'s and `x-lmgw-sample-rate`'s one route.
const SPEECH_ROUTES: &[(&str, &str)] = &[("POST", "/v1/audio/speech")];

/// `x-lmgw-voices-source`'s one route.
const VOICES_ROUTES: &[(&str, &str)] = &[("GET", "/v1/audio/voices")];

/// The table (§4.9). In spec order: the reasoning trio, then the two
/// literal-only headers (`x-lmgw-run`, `x-lmgw-admin-token`), the internal
/// `x-lmgw-face`, the gated-response trio, the three "route could not
/// comply" headers, the ladder pair, and the new counter-approximation
/// header.
pub const LMGW_HEADERS: &[LmgwHeader] = &[
    LmgwHeader {
        name: "x-lmgw-reasoning",
        direction: Direction::Request,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_ROUTES),
        schema: HeaderSchema::Enum(&["on", "off"]),
        description: "on|off — per-request thinking switch; wins over body fields. On a cloud \
            model, off goes out in the form the model takes: no reasoning control where it has \
            none, its lowest level where it cannot stop (x-lmgw-reasoning-ignored then says \
            enabled). An off never ends in an error: a refused one is retried with what the \
            refusal names and, failing that, with no reasoning control, so a model that cannot \
            run without reasoning answers with its default reasoning. Accepted on \
            /v1/chat/completions, /v1/messages, /v1/messages/count_tokens and /v1/responses.",
    },
    LmgwHeader {
        name: "x-lmgw-reasoning-effort",
        direction: Direction::Request,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_ROUTES),
        schema: HeaderSchema::Text,
        description: "an effort level (see capabilities.reasoning.levels of the model); 'none' = \
            off. Accepted on /v1/chat/completions, /v1/messages, /v1/messages/count_tokens and \
            /v1/responses.",
    },
    LmgwHeader {
        name: "x-lmgw-reasoning-budget",
        direction: Direction::Request,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_ROUTES),
        schema: HeaderSchema::Integer,
        description: "integer thinking-token budget; 0 = off. Expressible on llama.cpp, Gemini \
            and budget-style Anthropic models only. Accepted on /v1/chat/completions, \
            /v1/messages, /v1/messages/count_tokens and /v1/responses.",
    },
    LmgwHeader {
        name: "x-lmgw-run",
        direction: Direction::Request,
        audience: Audience::Agent,
        scope: Scope::AllInference,
        schema: HeaderSchema::Integer,
        description: "which live agent run to attribute this request's cost, tokens and call \
            count to, by job id (container-runtime §3.1); only an agent token may attribute, and \
            only to its own runs. An id naming no live run, a foreign run, or presented by a \
            caller with no agent token is ignored with a log line, never a refusal.",
    },
    LmgwHeader {
        name: "x-lmgw-admin-token",
        direction: Direction::Request,
        audience: Audience::Owner,
        // `Scope::None`, not `Routes([("POST", "/mcp/admin")])`: the route
        // already carries this credential as the `adminToken` security
        // scheme (`build.rs`'s `security_schemes()`), so it belongs on the
        // operation once, not twice over as a header parameter too.
        scope: Scope::None,
        schema: HeaderSchema::Text,
        description: "an extra bearer spelling accepted only on POST /mcp/admin, alongside the \
            ordinary bearer and x-api-key; the MCP client configs that dial the self-admin plane \
            send it this way. Inert on every other route.",
    },
    LmgwHeader {
        name: "x-lmgw-face",
        direction: Direction::Request,
        audience: Audience::Internal,
        scope: Scope::None,
        schema: HeaderSchema::Text,
        description: "set by lmgw itself on a request it proxies to an agent app's container, \
            naming which face — the public mount or the Admin-gated one — the inbound request \
            arrived on. Never read from a client-supplied value, which is stripped before the \
            proxy hop; the route family it travels on is lmgw's own plumbing, not part of this \
            API (§4.2).",
    },
    LmgwHeader {
        name: "x-lmgw-fallback",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(GATED_ROUTES),
        schema: HeaderSchema::Text,
        description: "RESPONSE: the alias that actually answered instead of the requested local \
            model; the body's model field still names the one requested.",
    },
    LmgwHeader {
        name: "x-lmgw-fallback-reason",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(GATED_ROUTES),
        schema: HeaderSchema::Enum(&[
            "hold",
            "external_vram",
            "background",
            "unavailable",
            "benchmark",
        ]),
        description: "RESPONSE: why x-lmgw-fallback answered, always next to it: hold (the \
            owner's GPU hold pauses local models), benchmark (a benchmark run has the GPU to \
            itself until it ends), external_vram (GPU memory used outside lmgw \
            — another program — left too little room to load the model, so the fallback \
            answered at once instead of waiting), background (a background job's model could \
            not load without disturbing the owner's), unavailable (a candidate alias's primary \
            cannot be used at all — missing, disabled or lacking a capability the alias enables \
            — and none of its other models is loaded).",
    },
    LmgwHeader {
        name: "x-lmgw-candidate",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(GATED_ROUTES),
        schema: HeaderSchema::Text,
        description: "RESPONSE: on a candidate alias, the id of the local model that answered \
            (or refused) the request; never next to x-lmgw-fallback. The body's model field \
            still names the alias requested.",
    },
    LmgwHeader {
        name: "x-lmgw-reasoning-ignored",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_IGNORED_ROUTES),
        schema: HeaderSchema::Enum(&["enabled", "effort", "budget"]),
        description: "RESPONSE: comma-separated controls (enabled, effort, budget) the route \
            could not express — enabled also when an off went out as no control or as the \
            model's lowest level, because the model cannot take the off itself, or the \
            provider refused every form of off and the model answered with its default \
            reasoning; absent when everything was applied.",
    },
    LmgwHeader {
        name: "x-lmgw-max-tokens-defaulted",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_IGNORED_ROUTES),
        schema: HeaderSchema::Integer,
        description: "RESPONSE: the max_tokens lmgw chose because the request set none and the \
            route (an Anthropic upstream) requires one: the catalog's published maximum when \
            streaming, 4096 when not streaming (the provider refuses non-streamed requests that \
            could run too long).",
    },
    LmgwHeader {
        name: "x-lmgw-max-tokens-raised",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_IGNORED_ROUTES),
        schema: HeaderSchema::Integer,
        description: "RESPONSE: the max_tokens lmgw raised the request's cap to, because the \
            requested thinking budget would not fit underneath it.",
    },
    LmgwHeader {
        name: "x-lmgw-max-tokens-clamped",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(RUNG_ROUTES),
        schema: HeaderSchema::Integer,
        description: "RESPONSE: the max_tokens lmgw lowered the request's cap to, because a \
            ladder rung or a guarded shared-KV-pool model's n_predict is the ceiling every \
            request on it must respect. Absent when nothing was lowered — a missing max_tokens \
            is filled, not clamped, and a value already under the ceiling is kept exactly.",
    },
    LmgwHeader {
        name: "x-lmgw-rung",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(RUNG_ROUTES),
        schema: HeaderSchema::Text,
        description: "RESPONSE: on a ladder model, the rung that answered (or refused) the \
            request: '<k>/<n>; ctx=<per-slot context>; gguf=<weights file>', counted from 1 (the \
            gguf part is left off when the file name cannot be a header value). A request whose \
            prompt plus max output does not fit the running rung makes the model climb to the \
            smallest rung that fits before it is answered; on /v1/responses it names the rung \
            running when the run opened (a unary answer one of whose turns the fallback answered \
            carries x-lmgw-fallback instead; a stream's headers leave before its turns run). \
            Absent on models without a ladder and on responses a fallback answered.",
    },
    LmgwHeader {
        name: "x-lmgw-count-approximate",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(COUNT_APPROXIMATE_ROUTES),
        schema: HeaderSchema::Enum(&[
            "flattened",
            "tokenizer_guess",
            "media_bound",
            "media_omitted",
            "message_framing",
        ]),
        description: "RESPONSE: on the token counters, why the number is not exactly what the \
            backend would count for this request, comma-separated: flattened (the request's \
            structure was counted as plain text — chat template, tool-definition and \
            per-message overhead are not included, so the real prompt is larger), \
            tokenizer_guess (the backend's tokenizer is unknown; counted with tiktoken \
            o200k_base), media_bound (images counted at the model's per-image upper bound), \
            media_omitted (image or audio parts are not in the number), message_framing \
            (/v1/count_tokens: the backend counts messages, so the text was counted as one user \
            message, framing included). Absent when the count is exact.",
    },
    LmgwHeader {
        name: "x-lmgw-speech",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(SPEECH_ROUTES),
        schema: HeaderSchema::Text,
        description: "RESPONSE: on POST /v1/audio/speech, what lmgw changed in the request \
            so the model understands it, '; '-separated: voice=options.voice_id (a voice the \
            model ships, which this family reads from options.voice_id — MagpieTTS), \
            voice=preset->options.voice_id (the same for a preset's voice id), voice=<name> (a \
            shipped voice sent in the model's own spelling), language=<asked>-><sent> (the \
            language in the model's own vocabulary, e.g. de->german for Qwen3-TTS, en->en-us \
            for Kokoro), instructions=options.instruct (moved where the family reads them), \
            instructions=also:options.instruct or instructions=also:options.instruction (the \
            model's default request options describe the voice under that other key, which \
            the engine would merge in beside the request's and refuse as conflicting — the \
            request's text went there too, so it replaces the default), instructions=dropped \
            (the model reads none — capabilities.speech.instructions is none), tags=mapped:<n>,stripped:<n> (inline tags in input rewritten into the \
            model's spelling, or removed so they are never read out). The voice and language \
            parts apply to local audio models only. Absent when the request went as it came; \
            input changes only for its inline tags.",
    },
    LmgwHeader {
        name: "x-lmgw-sample-rate",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(SPEECH_ROUTES),
        schema: HeaderSchema::Integer,
        description: "RESPONSE: on a streamed POST /v1/audio/speech (stream_format sse or \
            audio) to a local audio model, the sample rate of its PCM16 in Hz — the stream has \
            no header of its own. Learned from the model's last WAV answer through lmgw (also \
            capabilities.speech.sample_rate); absent until it has answered one since lmgw \
            started.",
    },
    LmgwHeader {
        name: "x-lmgw-voices-source",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(VOICES_ROUTES),
        schema: HeaderSchema::Enum(&["config", "engine"]),
        description: "RESPONSE: on GET /v1/audio/voices, where the list came from: config (a \
            local audio model answered from lmgw's own catalog — its presets, the voices its \
            package ships, its embeddings and the voice library — without starting it) or \
            engine (the model's own server: a remote upstream, or a local container with \
            ?probe=engine, which starts it if it is down).",
    },
];

/// The `{name: description}` map `/v1/models`' `lmgw.headers` publishes
/// (§4.9): every `Audience::Client` row, request or response alike — the
/// eleven headers the old hand-written block carried plus
/// `x-lmgw-count-approximate` (§5.1), `x-lmgw-speech`,
/// `x-lmgw-sample-rate` and `x-lmgw-voices-source`. `Agent`, `Owner` and
/// `Internal` rows (`x-lmgw-run`, `x-lmgw-admin-token`, `x-lmgw-face`) are in
/// the table for the drift guard to find, not for a client reading
/// `/v1/models` to see.
pub fn headers_block() -> std::collections::BTreeMap<&'static str, &'static str> {
    LMGW_HEADERS
        .iter()
        .filter(|h| h.audience == Audience::Client)
        .map(|h| (h.name, h.description))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_block_has_exactly_the_client_rows() {
        let block = headers_block();
        assert_eq!(block.len(), 15, "{block:#?}");
        assert!(block.contains_key("x-lmgw-count-approximate"));
        assert!(block.contains_key("x-lmgw-reasoning"));
    }

    #[test]
    fn agent_owner_and_internal_rows_are_excluded_from_the_block() {
        let block = headers_block();
        for name in ["x-lmgw-run", "x-lmgw-admin-token", "x-lmgw-face"] {
            assert!(
                !block.contains_key(name),
                "{name} must not be client-facing"
            );
        }
    }

    #[test]
    fn every_row_name_starts_with_x_lmgw() {
        for h in LMGW_HEADERS {
            assert!(h.name.starts_with("x-lmgw-"), "{}", h.name);
        }
    }
}
