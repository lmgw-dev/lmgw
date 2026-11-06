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
    ("POST", "/v1/responses/input_tokens"),
];

/// Every route `gate::open` can answer on behalf of, so a fallback, a
/// candidate's model, or (were it not narrower, see [`RUNG_ROUTES`]) a rung
/// can show up on its response.
const GATED_ROUTES: &[(&str, &str)] = &[
    ("POST", "/v1/chat/completions"),
    ("POST", "/v1/messages"),
    ("POST", "/v1/messages/count_tokens"),
    ("POST", "/v1/responses"),
    ("POST", "/v1/responses/input_tokens"),
    ("POST", "/v1/completions"),
    ("POST", "/v1/embeddings"),
    ("POST", "/v1/rerank"),
    ("POST", "/v1/systemone"),
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
    // The Chat's dictation: the gate answers it too, so a failed call names
    // the fallback that had the audio.
    ("POST", "/chat/api/threads/{id}/transcribe"),
];

/// `x-lmgw-background`'s routes (per-request background design §3): every
/// `/v1` route — and `/tokenize` — that admits a local model for a client,
/// which is [`GATED_ROUTES`] without the Chat's dictation (a dashboard route
/// the Chat calls itself).
///
/// Also the only routes the header is **read** on ([`takes_background`],
/// *changed 2026-10-22*, §11.12): on `/v1/realtime`, its WebRTC calls, the
/// Chat and `/mcp` it is never parsed, so nothing those paths admit can
/// become a guest — not even through a context they hand on in-process.
const BACKGROUND_ROUTES: &[(&str, &str)] = &[
    ("POST", "/v1/chat/completions"),
    ("POST", "/v1/messages"),
    ("POST", "/v1/messages/count_tokens"),
    ("POST", "/v1/responses"),
    ("POST", "/v1/responses/input_tokens"),
    ("POST", "/v1/completions"),
    ("POST", "/v1/embeddings"),
    ("POST", "/v1/rerank"),
    ("POST", "/v1/systemone"),
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

/// Whether `x-lmgw-background` is read on `method path` — one of
/// [`BACKGROUND_ROUTES`], the table the API docs print, so what is documented
/// and what is honoured cannot drift apart. A `HEAD` is read as the `GET` it
/// runs: axum answers it with the route's `GET` handler, which admits what
/// the `GET` would (`/v1/audio/voices?probe=engine`).
pub(crate) fn takes_background(method: &str, path: &str) -> bool {
    let method = if method == "HEAD" { "GET" } else { method };
    BACKGROUND_ROUTES
        .iter()
        .any(|(m, p)| *m == method && *p == path)
}

/// Routes whose response can report a reasoning control or a sampling
/// parameter the route could not express, or a `max_tokens` lmgw filled in or
/// raised — the three chat-style
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

/// Routes `x-lmgw-count-approximate` can appear on (§5.1): the three counters
/// that answer a number. `/tokenize` answers the backend's own token ids,
/// which are never approximate.
const COUNT_APPROXIMATE_ROUTES: &[(&str, &str)] = &[
    ("POST", "/v1/count_tokens"),
    ("POST", "/v1/messages/count_tokens"),
    ("POST", "/v1/responses/input_tokens"),
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

/// The table (§4.9). In spec order: the reasoning trio, `x-lmgw-background`, then the two
/// literal-only headers (`x-lmgw-run`, `x-lmgw-admin-token`), the internal
/// `x-lmgw-face`, the gated-response trio and its images count, the four "route could not
/// comply" headers (`x-lmgw-params-ignored` beside `x-lmgw-reasoning-ignored`, openai
/// egress design §7.1), the ladder pair, and the new counter-approximation
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
            then lists enabled), thinking type between_tools on an Anthropic model whose \
            catalog says it takes that type and not disabled. An off never ends in an error: a refused one is retried with \
            what the refusal names and, failing that, with no reasoning control, so a model \
            that cannot run without reasoning answers with its default reasoning. Accepted on \
            /v1/chat/completions, /v1/messages, /v1/messages/count_tokens, /v1/responses \
            and /v1/responses/input_tokens.",
    },
    LmgwHeader {
        name: "x-lmgw-reasoning-effort",
        direction: Direction::Request,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_ROUTES),
        schema: HeaderSchema::Text,
        description: "An effort level (see capabilities.reasoning.levels of the model); 'none' \
            turns reasoning off. Accepted on /v1/chat/completions, /v1/messages, \
            /v1/messages/count_tokens, /v1/responses and /v1/responses/input_tokens.",
    },
    LmgwHeader {
        name: "x-lmgw-reasoning-budget",
        direction: Direction::Request,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_ROUTES),
        schema: HeaderSchema::Integer,
        description: "Integer thinking-token budget; 0 turns reasoning off. Expressible on \
            llama.cpp, Gemini and budget-style Anthropic models only. Accepted on \
            /v1/chat/completions, /v1/messages, /v1/messages/count_tokens, /v1/responses \
            and /v1/responses/input_tokens.",
    },
    LmgwHeader {
        name: "x-lmgw-background",
        direction: Direction::Request,
        audience: Audience::Client,
        scope: Scope::Routes(BACKGROUND_ROUTES),
        schema: HeaderSchema::Enum(&["on", "off"]),
        description: "on|off: run this one request as background work, for speculative or \
            prefetching clients. On a local model it uses the model only when that disturbs \
            nobody: it joins the model if it is loaded and no foreground request is waiting for \
            room, or starts it into free GPU memory (evicting only idle background-started \
            models), and never evicts, waits for or climbs a model foreground work uses. \
            Otherwise the model's configured fallback answers (x-lmgw-fallback, \
            x-lmgw-fallback-reason: background), or the request is refused with 503 \
            gpu_deferred: retry later. Foreground work never interrupts a background request \
            that has started; close the request to cancel it. On a candidate alias it makes \
            the alias walk as a background alias. A cloud model and a model running on the CPU \
            are unaffected. Absent or off: an ordinary request. Any other value is a 400.",
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
        description:
            "The model or alias that answered instead of the one requested: the last hop of \
            its chain of fallbacks (each model and alias has its own, and a fallback can have \
            one in turn); the answer still names the requested model. On the chat routes, a \
            fallback whose capabilities say it cannot see receives the request's images as text \
            placeholders, and x-lmgw-images-omitted gives the count.",
    },
    LmgwHeader {
        name: "x-lmgw-fallback-chain",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(GATED_ROUTES),
        schema: HeaderSchema::Text,
        description: "Every hop a fallback-served answer was handed to, in order, each with \
            the reason it was handed on, as an RFC 8941 list: \
            \"m2\";reason=hold, \"cloud\";reason=hold. Sent on every answer a fallback served, \
            next to x-lmgw-fallback (the last name) and x-lmgw-fallback-reason (the last \
            reason), so a client reads one header whatever the chain's length. A name that \
            cannot be an sf-string (a byte outside 0x20–0x7E) is left empty, the reason kept. \
            A streamed /v1/responses run names the hops of its first turn only: its headers \
            left with the first event.",
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
            "quota",
            "unreachable",
            "disabled",
            "gpu_setup",
            "engine_download",
        ]),
        description: "Why x-lmgw-fallback answered (the last hop's reason; \
            x-lmgw-fallback-chain has every hop's); always sent next to it. hold: the GPU hold \
            pauses local models. benchmark: a benchmark run has the GPU to itself until it \
            ends. external_vram: GPU memory used outside lmgw by another program left too \
            little room to load the model, so the fallback answered at once instead of waiting. \
            background: a background job's model could not load without disturbing \
            foreground work (any request that is not background traffic: a background candidate \
            alias's, or one sent with x-lmgw-background: on). \
            unavailable: a candidate alias's primary cannot be used at all \
            (missing, disabled, or lacking a capability the alias enables) and none of its \
            other models is loaded. quota: the upstream answered that its account's quota is \
            exhausted (insufficient_quota), or lmgw remembers it so until its next check; \
            nothing was charged for the refused attempt. unreachable: the upstream could not be \
            connected to, so nothing reached it. disabled: the model or alias asked for is \
            disabled, or its upstream is. gpu_setup: the local model could not start on the \
            graphics card, and lmgw's machine check names why (the card, its driver, or the \
            piece that lets models use it, not ready on the computer lmgw runs on); while that \
            check still describes the machine, the fallback answers without a start being \
            tried. engine_download: the engine the local model runs in is not on the computer \
            yet, and downloading it failed or cannot be done.",
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
        schema: HeaderSchema::Enum(&["enabled", "effort", "budget", "between_tools"]),
        description: "Comma-separated list of the reasoning controls (enabled, effort, budget) \
            the route could not express. enabled is also listed when an off went out as no \
            control or as the model's lowest level, either because the model cannot take the \
            off itself or because the provider refused every form of off and the model answered \
            with its default reasoning. An off sent to an Anthropic model as thinking type \
            between_tools (its own lowest setting, when it takes no disabled) is not listed. \
            between_tools: a /v1/messages request's thinking type between_tools went to a \
            model of another provider as that provider's off. Absent when every control was \
            applied.",
    },
    LmgwHeader {
        name: "x-lmgw-params-ignored",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_IGNORED_ROUTES),
        schema: HeaderSchema::Enum(&[
            "top_k",
            "min_p",
            "repeat_penalty",
            "presence_penalty",
            "frequency_penalty",
            "seed",
            "context_management",
            "compaction",
            "thinking.display",
        ]),
        description: "Comma-separated list of the request's sampling parameters (top_k, min_p, \
            repeat_penalty, presence_penalty, frequency_penalty, seed) the answering model's \
            provider does not take, so lmgw did not send them: the OpenAI platform takes no \
            top_k, min_p or repeat_penalty; Anthropic none of the six but top_k; Gemini no \
            min_p, repeat_penalty or penalties. llama.cpp and OpenAI-compatible providers are \
            sent all of them. Also context_management and compaction, a /v1/messages \
            request's Anthropic context management and on-demand compaction, which only an \
            Anthropic model's route takes (both sent on as written), and \
            thinking.display, which goes only to an Anthropic model, beside a thinking type \
            that takes it (adaptive or enabled). Also content.<type>, a /v1/messages \
            request's content block lmgw does not model (redacted_thinking, a type newer \
            than lmgw), which only an Anthropic model's route is sent, and answer.<type>, \
            such a block in an Anthropic model's whole answer, which a Chat Completions or \
            Responses client is not given (a streamed answer's headers leave before it). \
            On an Anthropic model's route, content.thinking and content.<type> name a \
            thinking or other Anthropic block lmgw recorded from an answer at another \
            provider's address (a stored /v1/responses chain), which this provider cannot \
            read and is not sent. answer.citations and content.citations name an \
            Anthropic text block's citations, which only a /v1/messages client and an \
            Anthropic model's route are given (citations lmgw recorded from an answer only \
            at that provider's address, and a citation with no text not at all). \
            Request fields lmgw does not model are sent as they came and never listed. \
            Absent when every parameter was sent.",
    },
    LmgwHeader {
        name: "x-lmgw-params-refused",
        direction: Direction::Response,
        audience: Audience::Client,
        scope: Scope::Routes(REASONING_IGNORED_ROUTES),
        schema: HeaderSchema::Enum(&[
            "temperature",
            "top_p",
            "top_k",
            "min_p",
            "repeat_penalty",
            "presence_penalty",
            "frequency_penalty",
            "seed",
        ]),
        description: "Comma-separated list of the request's sampling parameters the answering \
            model refused, which lmgw left out of the send (stop is not left out; its refusal \
            reaches the client). A 400 whose words refuse exactly one parameter the request \
            carried (Anthropic '`temperature` is deprecated for this \
            model.', OpenAI 'Unsupported parameter: 'temperature' is not supported with \
            this model.' or 'Unsupported value: 'temperature' does not support 0 with this \
            model.') is retried once without exactly that parameter, the refused attempt \
            logged with a note naming it, and when the retry answers the parameter is left \
            out of that model's later requests too, each saying so here. Nothing is left \
            out without a refusal on record. On a streamed /v1/responses run the header \
            names what was on record when the stream opened. Absent when every parameter \
            was sent.",
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
            "tools_omitted",
            "pending_calls_omitted",
        ]),
        description: "On the token counters, why the number is not exactly what the backend \
            would count for this request; a comma-separated list. flattened: the request's \
            structure was counted as plain text, so chat template, tool-definition and \
            per-message overhead are not included and the real prompt is larger. \
            tokenizer_guess: the backend's tokenizer is unknown, so the count uses tiktoken \
            o200k_base. media_bound: images are counted at the model's per-image upper bound. \
            media_omitted: image or audio parts are not in the number. message_framing: on \
            /v1/count_tokens, the backend counts messages, so the text was counted as one user \
            message, framing included. tools_omitted: on /v1/responses/input_tokens, the mcp \
            tools' definitions (listed from their servers when a run starts) are not in the \
            number. pending_calls_omitted: on /v1/responses/input_tokens, the stored response \
            being continued is waiting on calls, whose results are not in the number. Absent \
            when the count is exact.",
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
/// `x-lmgw-sample-rate`, `x-lmgw-voices-source`, `x-lmgw-background` and
/// `x-lmgw-params-ignored` (openai egress design §7.1). `Agent`, `Owner` and
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
        assert_eq!(block.len(), 20, "{block:#?}");
        assert!(block.contains_key("x-lmgw-fallback-chain"));
        assert!(block.contains_key("x-lmgw-count-approximate"));
        assert!(block.contains_key("x-lmgw-images-omitted"));
        assert!(block.contains_key("x-lmgw-reasoning"));
        assert!(block.contains_key("x-lmgw-params-refused"));
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

    /// `x-lmgw-fallback-reason`'s enum is every reason the gate stamps, in
    /// its order, and its description says each one (machine-check design
    /// §2.5: `gpu_setup`, `engine_download`).
    #[test]
    fn the_fallback_reason_enum_is_every_reason_the_gate_stamps() {
        let row = LMGW_HEADERS
            .iter()
            .find(|h| h.name == "x-lmgw-fallback-reason")
            .unwrap();
        let HeaderSchema::Enum(values) = row.schema else {
            panic!("{:?}", row.schema)
        };
        let reasons: Vec<&str> = crate::gate::FallbackReason::ALL
            .iter()
            .map(|r| r.as_str())
            .collect();
        assert_eq!(values, reasons.as_slice());
        for r in reasons {
            assert!(
                row.description.contains(&format!("{r}:")),
                "{r} is not described"
            );
        }
    }
}
