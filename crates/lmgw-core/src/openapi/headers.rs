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

/// `x-lmgw-sample-rate`'s one route.
const SPEECH_ROUTES: &[(&str, &str)] = &[("POST", "/v1/audio/speech")];

/// `x-lmgw-speech`'s routes: speech, and the task routes for their text's
/// characters.
const SHAPED_ROUTES: &[(&str, &str)] = &[
    ("POST", "/v1/audio/speech"),
    ("POST", "/v1/tasks/run"),
    ("POST", "/v1/tasks/stream"),
];

/// `x-lmgw-voices-source`'s one route.
const VOICES_ROUTES: &[(&str, &str)] = &[("GET", "/v1/audio/voices")];

/// The table (§4.9). In spec order: the reasoning trio, then the two
/// literal-only headers (`x-lmgw-run`, `x-lmgw-admin-token`), the internal
/// `x-lmgw-face`, the gated-response trio and its images count, the three "route could not
/// comply" headers, the ladder pair, and the new counter-approximation
/// header.
pub const LMGW_HEADERS: &[LmgwHeader] = &[
    LmgwHeader {
        name: "x-lmgw-reasoning",
        direction: Direction::Request,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_ROUTES),
        schema: HeaderSchema::Enum(&["on", "off"]),
        description: "on|off: per-request thinking switch; it wins over body fields. On a cloud \
            model, off goes out in the form the model takes: no reasoning control where the \
            model has none, its lowest level where it cannot stop (x-lmgw-reasoning-ignored \
            then lists enabled). An off never ends in an error: a refused one is retried with \
            what the refusal names and, failing that, with no reasoning control, so a model \
            that cannot run without reasoning answers with its default reasoning. Accepted on \
            /v1/chat/completions, /v1/messages, /v1/messages/count_tokens and /v1/responses.",
    },
    LmgwHeader {
        name: "x-lmgw-reasoning-effort",
        direction: Direction::Request,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_ROUTES),
        schema: HeaderSchema::Text,
        description: "An effort level (see capabilities.reasoning.levels of the model); 'none' \
            turns reasoning off. Accepted on /v1/chat/completions, /v1/messages, \
            /v1/messages/count_tokens and /v1/responses.",
    },
    LmgwHeader {
        name: "x-lmgw-reasoning-budget",
        direction: Direction::Request,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_ROUTES),
        schema: HeaderSchema::Integer,
        description: "Integer thinking-token budget; 0 turns reasoning off. Expressible on \
            llama.cpp, Gemini and budget-style Anthropic models only. Accepted on \
            /v1/chat/completions, /v1/messages, /v1/messages/count_tokens and /v1/responses.",
    },
    LmgwHeader {
        name: "x-lmgw-run",
        direction: Direction::Request,
        audience: Audience::Agent,
        scope: Scope::AllInference,
        schema: HeaderSchema::Integer,
        description: "The job id of the live agent run to attribute this request's cost, tokens \
            and call count to. Only an agent token may attribute, and only to its own runs. An \
            id that names no live run, a run that belongs to another agent, or a caller without \
            an agent token is ignored with a log line and never refused.",
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
        description: "An extra bearer spelling accepted only on POST /mcp/admin, alongside the \
            ordinary bearer and x-api-key; MCP client configs that dial the self-admin plane \
            send it this way. Inert on every other route.",
    },
    LmgwHeader {
        name: "x-lmgw-face",
        direction: Direction::Request,
        audience: Audience::Internal,
        scope: Scope::None,
        schema: HeaderSchema::Text,
        description: "Set by lmgw on a request it proxies to an agent app's container; it names \
            which face, the public mount or the admin-gated one, the inbound request arrived \
            on. A client-supplied value is stripped before the proxy hop and never read. The \
            route family it travels on is lmgw's internal plumbing, not part of this API.",
    },
    LmgwHeader {
        name: "x-lmgw-fallback",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(GATED_ROUTES),
        schema: HeaderSchema::Text,
        description: "The alias that answered instead of the requested local model; the body's \
            model field still names the requested one. On the chat routes, a fallback whose \
            capabilities say it cannot see receives the request's images as text placeholders, \
            and x-lmgw-images-omitted gives the count.",
    },
    LmgwHeader {
        name: "x-lmgw-images-omitted",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_IGNORED_ROUTES),
        schema: HeaderSchema::Integer,
        description: "Sent next to x-lmgw-fallback: the number of the request's images that \
            went to the answering fallback as text placeholders because its capabilities say it \
            cannot see ('[image/png image, N base64 bytes — omitted: the answering model cannot \
            see images]'). The model reads the placeholder; the client reads this count. On \
            /v1/responses it counts the images the run opened with. Absent when every image was \
            sent, and on a route the client named itself, which always gets its images.",
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
        description: "Why x-lmgw-fallback answered; always sent next to it. hold: the GPU hold \
            pauses local models. benchmark: a benchmark run has the GPU to itself until it \
            ends. external_vram: GPU memory used outside lmgw by another program left too \
            little room to load the model, so the fallback answered at once instead of waiting. \
            background: a background job's model could not load without disturbing \
            foreground work (any request that is not a background candidate alias's). \
            unavailable: a candidate alias's primary cannot be used at all \
            (missing, disabled, or lacking a capability the alias enables) and none of its \
            other models is loaded.",
    },
    LmgwHeader {
        name: "x-lmgw-candidate",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(GATED_ROUTES),
        schema: HeaderSchema::Text,
        description: "On a candidate alias, the id of the local model that answered (or \
            refused) the request. Never sent next to x-lmgw-fallback. The body's model field \
            still names the requested alias.",
    },
    LmgwHeader {
        name: "x-lmgw-reasoning-ignored",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_IGNORED_ROUTES),
        schema: HeaderSchema::Enum(&["enabled", "effort", "budget"]),
        description: "Comma-separated list of the reasoning controls (enabled, effort, budget) \
            the route could not express. enabled is also listed when an off went out as no \
            control or as the model's lowest level, either because the model cannot take the \
            off itself or because the provider refused every form of off and the model answered \
            with its default reasoning. Absent when every control was applied.",
    },
    LmgwHeader {
        name: "x-lmgw-max-tokens-defaulted",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_IGNORED_ROUTES),
        schema: HeaderSchema::Integer,
        description: "The max_tokens value lmgw chose because the request set none and the \
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
        description: "The max_tokens value lmgw raised the request's cap to, because the \
            requested thinking budget would not fit underneath it.",
    },
    LmgwHeader {
        name: "x-lmgw-max-tokens-clamped",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(RUNG_ROUTES),
        schema: HeaderSchema::Integer,
        description: "The max_tokens value lmgw lowered the request's cap to, because the \
            n_predict of a ladder rung, or of a guarded shared-KV-pool model, is a ceiling \
            every request on it must respect. Absent when nothing was lowered: a missing \
            max_tokens is filled, not clamped, and a value already under the ceiling is kept \
            exactly.",
    },
    LmgwHeader {
        name: "x-lmgw-rung",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(RUNG_ROUTES),
        schema: HeaderSchema::Text,
        description: "On a ladder model, the rung that answered (or refused) the request, as \
            '<k>/<n>; ctx=<per-slot context>; gguf=<weights file>', counted from 1 (the gguf \
            part is left off when the file name cannot be a header value). A request whose \
            prompt plus max output does not fit the running rung makes the model climb to the \
            smallest rung that fits before it answers. On /v1/responses it names the rung \
            running when the run opened; a unary answer one of whose turns the fallback \
            answered carries x-lmgw-fallback instead, and a stream's headers are sent before \
            its turns run. Absent on models without a ladder and on responses a fallback \
            answered.",
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
        description: "On the token counters, why the number is not exactly what the backend \
            would count for this request; a comma-separated list. flattened: the request's \
            structure was counted as plain text, so chat template, tool-definition and \
            per-message overhead are not included and the real prompt is larger. \
            tokenizer_guess: the backend's tokenizer is unknown, so the count uses tiktoken \
            o200k_base. media_bound: images are counted at the model's per-image upper bound. \
            media_omitted: image or audio parts are not in the number. message_framing: on \
            /v1/count_tokens, the backend counts messages, so the text was counted as one user \
            message, framing included. Absent when the count is exact.",
    },
    LmgwHeader {
        name: "x-lmgw-speech",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(SHAPED_ROUTES),
        schema: HeaderSchema::Text,
        description: "On POST /v1/audio/speech (and on /v1/tasks/run and /v1/tasks/stream, \
            for their chars part), what lmgw changed in the request so the model understands \
            it; a '; '-separated list. voice=options.voice_id: a voice the model \
            ships, which this family (MagpieTTS) reads from options.voice_id. \
            voice=preset->options.voice_id: the same for a preset's voice id. voice=<name>: a \
            shipped voice sent in the model's own spelling. language=<asked>-><sent>: the \
            language in the model's own vocabulary, e.g. de->german for Qwen3-TTS, en->en-us \
            for Kokoro. instructions=options.instruct: instructions moved to where the family \
            reads them. instructions=also:options.instruct or \
            instructions=also:options.instruction: the model's default request options describe \
            the voice under that other key, which the engine would merge in beside the \
            request's and refuse as conflicting, so the request's text went there too and \
            replaces the default. instructions=dropped: the model reads none \
            (capabilities.speech.instructions is none). tags=mapped:<n>,stripped:<n>: inline \
            tags in input were rewritten into the model's spelling, or removed so they are \
            never read out. chars=replaced:<U+XXXX/...>,dropped:<U+XXXX/...>: characters of \
            input the model's engine has no entry for in its package's vocabulary (Supertonic \
            refuses the whole request over one), replaced by an equivalent it has (a \
            typographic quote by a plain one, a dash by -) or removed (an emoji). Each list \
            names at most 64 codepoints and then ends in +<n> more; lmgw's log has the full \
            list. An input left with nothing to say is 400 empty_input, which carries the \
            header too. The voice, language and chars parts apply to local audio models only. \
            Absent when the request went as it came; input changes only for its inline tags \
            and those characters.",
    },
    LmgwHeader {
        name: "x-lmgw-sample-rate",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(SPEECH_ROUTES),
        schema: HeaderSchema::Integer,
        description: "On a streamed POST /v1/audio/speech (stream_format sse or audio) to a \
            local audio model, the sample rate of its PCM16 in Hz; the stream has no header of \
            its own. lmgw learns it from the model's last WAV answer (it is also \
            capabilities.speech.sample_rate) and omits it until the model has answered one \
            since lmgw started.",
    },
    LmgwHeader {
        name: "x-lmgw-voices-source",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(VOICES_ROUTES),
        schema: HeaderSchema::Enum(&["config", "engine"]),
        description: "On GET /v1/audio/voices, where the list came from. config: a local audio \
            model answered from lmgw's own catalog (its presets, the voices its package ships, \
            its embeddings and the voice library) without being started. engine: the model's \
            own server answered, either a remote upstream or a local container with \
            ?probe=engine, which starts the container if it is down.",
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
        assert_eq!(block.len(), 16, "{block:#?}");
        assert!(block.contains_key("x-lmgw-count-approximate"));
        assert!(block.contains_key("x-lmgw-images-omitted"));
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
