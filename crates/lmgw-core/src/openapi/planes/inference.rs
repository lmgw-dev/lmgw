//! The `/v1` plane's `DocRoute`s (api-docs design §4.8, §4.11): every
//! `CAPABILITY_TABLE` row under `/v1`, plus `/tokenize` (registered at the
//! root, `proxy/tokenize.rs`, §0 finding 6) and `/mcp`/`/mcp/admin` (the one
//! path whose methods do not share a capability — `POST /mcp/admin` is
//! `Admin`, its `GET`/`DELETE` siblings and all of `/mcp` are `Inference`).
//!
//! `dialect`, `model_task` and `endpoints` are copied from §4.8's table,
//! which is the audited inventory this file exists to turn into a real
//! registry — not re-derived here. Where the table's own bracket notation
//! disagreed with §4.11's explicit prose (the three `/v1/responses/{id}*`
//! rows: the table's shorthand said `[openai]`, §4.11 says "newly lists ...
//! under other"), §4.11 wins — it is the section that actually specifies
//! `lmgw_endpoints()`'s output.

use lmgw_api_types::openapi_ext::{endpoint_group, model_task};

use super::super::registry::{Dialect, DocRoute, Req, Resp};
use super::super::v1::{anthropic, aux, chat, mcp, media, models, responses};

/// A `DocRoute` with every field the inference plane leaves at its default —
/// `Resp::Untyped` on `response` is never actually served: every route below
/// overrides it, and a route that forgot to would fail `capability_for`'s
/// sibling checks (`doc_is_structurally_sound`'s "every op has a non-default
/// response" only checks *a* response exists, but an untyped 200 on a route
/// meant to have a real schema is exactly the kind of thing code review, not
/// this helper, should catch — so the placeholder reason names itself).
fn base(
    method: &'static str,
    path: &'static str,
    tag: &'static str,
    summary: &'static str,
) -> DocRoute {
    DocRoute {
        method,
        path,
        tag,
        summary,
        description: "",
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::None,
        response: Resp::Untyped("BUG: inference.rs route builder did not override the response"),
        dialect: Dialect::OpenAi,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        example: None,
    }
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        // -- Chat -----------------------------------------------------------
        DocRoute {
            description: "The chat-completions ingress this gateway understands \
                (ingress/openai.rs) plus lmgw's own reasoning controls and \
                chat_template_kwargs passthrough. `model` may be an alias, a local model id, \
                a candidate alias, or an exposed upstream's `prefix/…` id. An anthropic-beta \
                header is forwarded when the route is an Anthropic upstream.",
            request: Req::Json(chat::request),
            response: Resp::JsonOrSse {
                json: chat::response,
                events: &[("chunk", chat::stream_chunk)],
            },
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            model_task: Some(model_task::CHAT),
            writes: Some(false),
            example: Some(chat::example),
            ..base(
                "POST",
                "/v1/chat/completions",
                "openai",
                "Create a chat completion",
            )
        },
        // -- Responses --------------------------------------------------------
        DocRoute {
            description: "lmgw's own Responses API implementation (ingress/responses.rs) — \
                llama-server has none. MCP tools run server-side on lmgw's registered MCP \
                servers; background:true, truncation:\"auto\" and hosted tools are refused \
                rather than approximated. An anthropic-beta header is forwarded when the route \
                is an Anthropic upstream.",
            request: Req::Json(responses::request),
            response: Resp::JsonOrSse {
                json: responses::response,
                events: &[("response", responses::stream_event)],
            },
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            model_task: Some(model_task::CHAT),
            example: Some(responses::example),
            ..base("POST", "/v1/responses", "openai", "Create a response")
        },
        DocRoute {
            description: "A stored response (crate::responses) — only present when the \
                creating call ran with store: true and the gateway's own retention allows it.",
            response: Resp::Json(responses::response),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            ..base("GET", "/v1/responses/{id}", "openai", "Retrieve a response")
        },
        DocRoute {
            description: "Deletes a stored response.",
            response: Resp::Json(responses::deletion),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            ..base(
                "DELETE",
                "/v1/responses/{id}",
                "openai",
                "Delete a response",
            )
        },
        DocRoute {
            description: "This turn's `input` array as it was sent, for a stored response.",
            response: Resp::Json(responses::input_items),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            ..base(
                "GET",
                "/v1/responses/{id}/input_items",
                "openai",
                "List a response's input items",
            )
        },
        // -- Legacy completions -----------------------------------------------
        DocRoute {
            description: "The legacy text-completions shape (proxy/legacy.rs). Refuses a \
                non-OpenAI-protocol upstream with 400 rather than sending it a request its \
                own API cannot answer. A missing model is a 400 (missing 'model'), as on \
                every other ingress.",
            request: Req::Json(aux::completions_request),
            response: Resp::JsonOrSse {
                json: aux::completions_response,
                events: &[("chunk", aux::completions_stream_chunk)],
            },
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            model_task: Some(model_task::CHAT),
            writes: Some(false),
            example: Some(aux::completions_example),
            ..base(
                "POST",
                "/v1/completions",
                "openai",
                "Create a legacy text completion",
            )
        },
        // -- Embeddings / rerank ------------------------------------------------
        DocRoute {
            description: "dimensions is forwarded in the upstream's own spelling, and a \
                route that ignores it (llama-server) is a 400, not full-length vectors. \
                encoding_format base64 is encoded by the gateway for every upstream.",
            request: Req::Json(aux::embeddings_request),
            response: Resp::Json(aux::embeddings_response),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            model_task: Some(model_task::EMBEDDING),
            writes: Some(false),
            example: Some(aux::embeddings_example),
            ..base("POST", "/v1/embeddings", "openai", "Create embeddings")
        },
        DocRoute {
            description: "Jina and TEI request shapes both work (documents or texts); an \
                embedding-only row is refused. Answers Jina's results shape either way.",
            request: Req::Json(aux::rerank_request),
            response: Resp::Json(aux::rerank_response),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            model_task: Some(model_task::RERANK),
            writes: Some(false),
            example: Some(aux::rerank_example),
            ..base("POST", "/v1/rerank", "lmgw-inference", "Rerank documents")
        },
        // -- Anthropic messages -------------------------------------------------
        DocRoute {
            description: "The /v1/messages ingress this gateway understands \
                (ingress/anthropic.rs:14). A server tool (no input_schema) is refused with \
                400; redacted_thinking is accepted and dropped. A missing max_tokens is \
                filled in rather than refused (x-lmgw-max-tokens-defaulted). anthropic-beta \
                reaches an Anthropic upstream as one header, merged with the upstream row's \
                own; other protocols have no equivalent and are sent none.",
            request: Req::Json(anthropic::messages_request),
            response: Resp::JsonOrSse {
                json: anthropic::messages_response,
                events: &[("message", anthropic::stream_event)],
            },
            dialect: Dialect::Anthropic,
            endpoints: &[endpoint_group::ANTHROPIC],
            model_task: Some(model_task::CHAT),
            writes: Some(false),
            example: Some(anthropic::example),
            ..base("POST", "/v1/messages", "anthropic", "Create a message")
        },
        DocRoute {
            description: "The Anthropic SDKs' messages.count_tokens() (§5.2): on an \
                Anthropic-protocol upstream, the client body is sent through verbatim (only \
                model replaced), with its anthropic-beta flags, so server tools and new fields \
                count exactly. \
                x-lmgw-count-approximate explains any other route's approximation. \
                Enforces the key's alias scope like every other counter (a refusal is a \
                logged 403 key_scope, a success is not logged); no budget applies, the key's \
                or the gateway's, since a count costs nothing.",
            request: Req::Json(anthropic::count_request),
            response: Resp::Json(anthropic::count_response),
            dialect: Dialect::Anthropic,
            endpoints: &[endpoint_group::ANTHROPIC],
            model_task: Some(model_task::CHAT),
            writes: Some(false),
            example: Some(anthropic::count_example),
            ..base(
                "POST",
                "/v1/messages/count_tokens",
                "anthropic",
                "Count tokens for a message",
            )
        },
        // -- lmgw's own counters -------------------------------------------------
        DocRoute {
            description: "lmgw's own universal counter (proxy/count.rs): routed by model \
                exactly like the chat endpoints, so counting on a cold local model starts \
                its container. x-lmgw-count-approximate explains tokenizer_guess (an \
                OpenAI-protocol upstream tiktoken cannot identify) and message_framing (an \
                Anthropic/Gemini upstream counts messages, so the text went as one user \
                turn).",
            request: Req::Json(aux::count_request),
            response: Resp::Json(aux::count_response),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            model_task: Some(model_task::CHAT),
            writes: Some(false),
            example: Some(aux::count_example),
            ..base(
                "POST",
                "/v1/count_tokens",
                "lmgw-inference",
                "Count tokens (universal)",
            )
        },
        DocRoute {
            description: "llama.cpp's own /tokenize, for llama.cpp clients (§5.3): the \
                client object is forwarded to the backend's own /tokenize with model \
                rewritten, and the answer — token ids, or {id,piece} with with_pieces — \
                comes back verbatim, the backend's own errors included (but an upstream \
                401/403 is a 502: that is lmgw's credential). The body is read as JSON \
                whatever its Content-Type, as llama-server reads it. 501 \
                not_supported_error on any backend that is not a real llama-server: an id \
                from a tokenizer the backend does not run would be a hidden approximation. \
                Enforces the key's alias scope (a logged 403 key_scope); no budget applies. \
                Errors are llama.cpp-shaped, except the shared /v1 layers' own: the gate's \
                401, 403 and 429, and the 413 for a declared Content-Length over \
                max_body_mb, which are OpenAI-shaped.",
            request: Req::Json(media::tokenize_request),
            response: Resp::Json(media::tokenize_response),
            dialect: Dialect::LlamaCpp,
            endpoints: &[endpoint_group::OTHER],
            model_task: Some(model_task::CHAT),
            writes: Some(false),
            example: Some(media::tokenize_example),
            ..base("POST", "/tokenize", "llamacpp", "Tokenize text")
        },
        // -- Audio ---------------------------------------------------------------
        DocRoute {
            description: "OpenAI's TTS shape plus audio.cpp's own request options, forwarded \
                verbatim (model rewritten); a row's configured voice presets apply on top.",
            request: Req::Json(media::speech_request),
            response: Resp::Binary(media::AUDIO_MIME_TYPES),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            model_task: Some(model_task::TTS),
            writes: Some(false),
            ..base("POST", "/v1/audio/speech", "openai", "Generate speech")
        },
        DocRoute {
            description: "multipart/form-data upload (also accepts application/json); \
                byte-level passthrough to an OpenAI-protocol upstream only.",
            request: Req::Multipart(media::transcriptions_request),
            response: Resp::Json(media::transcription_response),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            model_task: Some(model_task::ASR),
            writes: Some(false),
            ..base(
                "POST",
                "/v1/audio/transcriptions",
                "openai",
                "Transcribe audio",
            )
        },
        DocRoute {
            description: "The same request as /v1/audio/transcriptions, with the transcript \
                detail arrays (words, segments, speaker turns) the plain route leaves out — \
                an lmgw route, not an OpenAI one.",
            request: Req::Multipart(media::transcriptions_request),
            response: Resp::Json(media::transcription_details_response),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            model_task: Some(model_task::ASR),
            writes: Some(false),
            ..base(
                "POST",
                "/v1/audio/transcriptions/details",
                "openai",
                "Transcribe audio with detail",
            )
        },
        DocRoute {
            description: "Forced alignment against a task: \"align\" row — lmgw's own route, \
                multipart/form-data only.",
            request: Req::Multipart(media::alignments_request),
            response: Resp::Json(media::alignments_response),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            writes: Some(false),
            ..base(
                "POST",
                "/v1/audio/alignments",
                "openai",
                "Align audio to text",
            )
        },
        DocRoute {
            description: "A row's own voice ids and presets, when it defines any — lmgw's \
                own route.",
            query: Some(|g| g.root_schema_for::<crate::server::VoicesQuery>()),
            response: Resp::Json(media::voices_response),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            model_task: Some(model_task::TTS),
            ..base("GET", "/v1/audio/voices", "openai", "List a model's voices")
        },
        // -- Images ---------------------------------------------------------------
        DocRoute {
            description: "OpenAI's images shape; sd.cpp CLI arguments may ride along in a \
                trailing <sd_cpp_extra_args>{…}</sd_cpp_extra_args> block on prompt (the \
                api-types image_lab.rs builder is the reference for its contents). Answers \
                base64 only (b64_json), never a URL.",
            request: Req::Json(media::image_generations_request),
            response: Resp::Json(media::image_response),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            model_task: Some(model_task::IMAGE_GENERATION),
            writes: Some(false),
            example: Some(media::image_generations_example),
            ..base(
                "POST",
                "/v1/images/generations",
                "openai",
                "Generate an image",
            )
        },
        DocRoute {
            description: "multipart/form-data upload; refused unless the resolved row's edit \
                pipeline flag is set.",
            request: Req::Multipart(media::image_edits_request),
            response: Resp::Json(media::image_response),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            model_task: Some(model_task::IMAGE_EDIT),
            writes: Some(false),
            ..base("POST", "/v1/images/edits", "openai", "Edit an image")
        },
        // -- Tasks (audio.cpp generic) ---------------------------------------------
        DocRoute {
            description: "audio.cpp's own generic task route; request is relayed to the \
                backend untouched.",
            request: Req::Json(media::task_request),
            response: Resp::Untyped("audio.cpp's own per-task JSON, relayed verbatim"),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            writes: Some(false),
            ..base(
                "POST",
                "/v1/tasks/run",
                "lmgw-inference",
                "Run a generic task",
            )
        },
        DocRoute {
            description: "The streaming-mode sibling of /v1/tasks/run.",
            request: Req::Json(media::task_request),
            response: Resp::Untyped("audio.cpp's own per-task stream, relayed verbatim"),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            writes: Some(false),
            ..base(
                "POST",
                "/v1/tasks/stream",
                "lmgw-inference",
                "Run a generic task (streaming)",
            )
        },
        // -- Models -----------------------------------------------------------------
        DocRoute {
            description: "Aggregates enabled aliases, public local models and expose-all \
                upstream catalogs. Served OpenAI-shaped unless the caller sends \
                anthropic-version, in which case the Anthropic SDK's own shape (model-\
                capabilities design §2.2) — this document names the OpenAI error dialect as \
                default; the Anthropic-dialect caller gets an Anthropic-shaped error body in \
                practice.",
            response: Resp::Json(models::model_list),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI, endpoint_group::ANTHROPIC],
            ..base("GET", "/v1/models", "openai", "List models")
        },
        DocRoute {
            description: "The id may contain '/' (a passthrough id carries its upstream \
                prefix and the provider's own slashes), hence the wildcard path parameter. \
                Same dual-dialect note as GET /v1/models.",
            response: Resp::Json(models::model_by_id),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI, endpoint_group::ANTHROPIC],
            ..base("GET", "/v1/models/{*id}", "openai", "Retrieve a model")
        },
        // -- This document itself ----------------------------------------------------
        DocRoute {
            description: "This description, filtered to the inference plane (§4.5).",
            response: Resp::Doc,
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            ..base(
                "GET",
                "/v1/openapi.json",
                "lmgw-inference",
                "Get the inference-plane OpenAPI description",
            )
        },
        // -- MCP ----------------------------------------------------------------------
        DocRoute {
            description: "MCP Streamable HTTP (mcp/ingress.rs): single JSON-RPC \
                request/notification/response per POST, no batching. initialize negotiates \
                MCP-Protocol-Version and assigns Mcp-Session-Id, which every later request on \
                the session must carry; a notification answers 202 with no body. tools/list \
                and tools/call reach the registered southbound MCP servers' tools, \
                namespaced <prefix>__.",
            request: Req::JsonRpc,
            response: Resp::Json(mcp::jsonrpc_response),
            dialect: Dialect::JsonRpc,
            example: Some(mcp::initialize_example),
            ..base("POST", "/mcp", "mcp", "Call the aggregate MCP endpoint")
        },
        DocRoute {
            description: "Opens the server→client notification stream for a valid session \
                (a session-less GET is 400/404): pushes notifications/tools/list_changed when \
                the aggregate's composition changes.",
            response: Resp::Sse(&[("notification", mcp::notification_event)]),
            dialect: Dialect::JsonRpc,
            ..base("GET", "/mcp", "mcp", "Open the MCP notification stream")
        },
        DocRoute {
            description: "Ends a session.",
            response: Resp::NoContent,
            dialect: Dialect::JsonRpc,
            ..base("DELETE", "/mcp", "mcp", "End an MCP session")
        },
        DocRoute {
            description: "The self-admin plane (mcp/selfadmin/catalog.rs): the built-in \
                lmgw__* tools and nothing else — a much narrower grant than /mcp, gated on an \
                owner credential rather than any gateway key. Also accepts the \
                x-lmgw-admin-token header as a third bearer spelling (adminToken security \
                scheme), which is how the MCP client configs that dial this plane \
                authenticate.",
            request: Req::JsonRpc,
            response: Resp::Json(mcp::jsonrpc_response),
            dialect: Dialect::JsonRpc,
            example: Some(mcp::admin_call_example),
            ..base(
                "POST",
                "/mcp/admin",
                "mcp",
                "Call the self-admin MCP endpoint",
            )
        },
        DocRoute {
            description: "As GET /mcp, on the self-admin plane's own session.",
            response: Resp::Sse(&[("notification", mcp::notification_event)]),
            dialect: Dialect::JsonRpc,
            ..base(
                "GET",
                "/mcp/admin",
                "mcp",
                "Open the self-admin MCP notification stream",
            )
        },
        DocRoute {
            description: "As DELETE /mcp, on the self-admin plane's own session.",
            response: Resp::NoContent,
            dialect: Dialect::JsonRpc,
            ..base(
                "DELETE",
                "/mcp/admin",
                "mcp",
                "End a self-admin MCP session",
            )
        },
    ]
}
