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
            description: "Every voice name a TTS model answers to — lmgw's own route. A \
                local audio model is answered from lmgw's catalog (presets, the voices its \
                package ships, its embeddings, the voice library) without starting it; \
                ?probe=engine asks its own server instead, starting it if it is down. A remote \
                model is asked.",
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
        // -- Realtime ---------------------------------------------------------------
        DocRoute {
            description:
                "OpenAI's GA Realtime protocol over a WebSocket (ws://<host>/v1/realtime), \
                answered by a cascade of this gateway's own aliases: voice activity and \
                end-of-turn detection inside lmgw (Silero VAD; Smart Turn for semantic_vad), \
                speech to text with realtime.asr_alias, the chat model, and text to speech with \
                realtime.tts_alias — local models first, any of them a cloud alias if the owner \
                chooses (Settings → Realtime, which also shows what the cascade holds on the \
                GPU). `model` is resolved in this order: a realtime.model_map name, an OpenAI \
                Realtime name (gpt-realtime*, never taken as an alias), which \
                realtime.default_model answers, or a chat alias; it may be left out and named \
                in the first session.update instead. What a name resolved to is echoed in \
                session.lmgw.resolved. Implemented: conversation sessions (session.type \
                realtime) with audio or text output, PCM16 at 24 kHz in and out, server_vad, \
                semantic_vad and manual turns, barge-in with a word check, session.update, \
                input_audio_buffer.append/commit/clear, conversation.item.create/retrieve/\
                truncate/delete, response.create and response.cancel, client-side function \
                tools, and MCP tools the gateway runs itself on a session not bound to a Chat \
                thread. An MCP session tool {\"type\": \"mcp\", \"server_label\"} names one of \
                this gateway's own MCP servers (by its tool prefix or name) or a built-in \
                toolset (docs, kb, and lmgw for an owner credential while the self-admin tools \
                are on), resolved as on /v1/responses under the key's tool scope; GET \
                /v1/mcp/servers lists the labels a caller may use and GET \
                /v1/mcp/servers/{label} one label's tools. server_url, connector_id, headers, \
                authorization and server_description are accepted and never used: lmgw dials \
                no URL a client names, and echoes authorization and header values as \
                [redacted]. allowed_tools takes a name list or {tool_names}; read_only and a \
                require_approval that would gate a tool are refused (approvals are not built \
                here yet: leave it out or send \"never\"); one entry per server_label. The \
                session lists a label's tools as an mcp_list_tools item \
                (mcp_list_tools.in_progress, then .completed or .failed — a failed label also \
                gets an error with code mcp_list_tools_failed saying why, and a later \
                session.update naming it lists it again), and a response waits for the \
                listings in flight. A model's call to one of those tools is an mcp_call item \
                (server_label, name — the server's own tool name — arguments, output, error, \
                always present), streamed as response.mcp_call_arguments.delta and .done, then \
                run by the gateway before response.done (response.mcp_call.in_progress, then \
                .completed with the output or .failed with a tool_execution_error). The call \
                ends the response: the client sends response.create for the answer, as \
                OpenAI's Realtime API has it (@openai/agents does so by itself, once per \
                response). A cancel or barge-in abandons a running call, which then fails \
                saying it may or may not have run. A response's tools may narrow to a label \
                the session has listed, and tool_choice takes {\"type\": \"mcp\", \
                \"server_label\"} with an optional name. A client may replay mcp_call items as \
                history; \
                mcp_list_tools and the approval items are refused. Each call writes a \
                realtime-tool usage row. \
                Refused with an error naming what is missing: transcription sessions, \
                G.711 formats, output_audio_buffer.clear (WebRTC only) and an input_audio item \
                without its transcript. Accepted and echoed but not applied: noise_reduction, \
                and truncation (lmgw.resolved.truncation says \"disabled\"; an overflow is an \
                error). Not served at all: WebRTC, SIP, ephemeral client secrets, live partial \
                transcripts and out-of-band responses. \
                Credentials as on every /v1 route; a browser, which cannot set headers on a \
                WebSocket, may offer its key as the subprotocol \
                `openai-insecure-api-key.<key>` alongside `realtime` — accepted on this route \
                only. A beta client (OpenAI-Beta: realtime=v1) is refused with 400 \
                beta_protocol_unsupported, and with Require API key off an anonymous upgrade \
                from another page's Origin is refused with 403. Chat models list this route in \
                their capabilities.endpoints on GET /v1/models once an ASR and a TTS alias are \
                configured. The TTS gets lmgw's speech instructions — \
                session.lmgw.speech_instructions, a response.create's \
                response.lmgw.speech_instructions, else realtime.speech_instructions — as a \
                style, or as the description a voice-design (vdes) model designs its voice \
                from; session.lmgw.resolved.speech says what is in effect, and the transcript \
                leaves out the inline tags ([laughter]) the voice renders. Markdown a voice \
                cannot read is not spoken: fenced code blocks and tables (lines that start \
                with |, from one that ends with | or is followed by a delimiter row) are \
                skipped — the transcript leaves them out, and the model's history keeps them. \
                On a style or \
                passthrough TTS that renders no tags (Qwen3 CustomVoice, Auk, MOSS-TTS; a \
                cloud TTS only when its alias's capabilities.speech override declares \
                instructions 'style'), the tags that open a sentence, or a part of one \
                spoken on its own, are a delivery cue instead ([laughing]): sent after the \
                style as the instructions until that sentence ends, best effort, never read \
                out (resolved.speech.cues). The dashboard's voice mode binds a session to a \
                Chat thread with chat_thread=<id> (negative for a temporary chat): the admin \
                capability only — the dashboard's cookie or an owner key; refused before the \
                upgrade, as flat JSON and checked in this order, with 403 \
                chat_thread_not_allowed, 400 owned_by_thread beside model, 404 \
                chat_thread_not_found, and 409 chat_thread_admin for an Admin Chat thread (no \
                voice mode there). session.created then names the thread in \
                session.lmgw.resolved.chat_thread ({id, title, temporary, admin_tools}), and \
                the connect warm loads the thread's chat, speech-to-text and text-to-speech \
                models as one group, which may evict idle models as the requests it announces \
                would, whatever realtime.warm_on_connect says (that setting is for unbound \
                sessions). The \
                thread then owns the chat model, prompt, tools, speech models, voice, speech \
                style, language, seed and conversation: a session.update that changes one of \
                them, a response.create that overrides one, and conversation.item.create or \
                .delete are refused with owned_by_thread (an echoed value is accepted). Each \
                response is the thread's own chat turn — its model, prompt with the \
                spoken-style instructions after it, sampling, attachments, knowledge bases \
                and MCP tools, which never become function_call items — written to the \
                thread's history: the user turn with how it was spoken, the reply cut to \
                what was heard (the unheard rest kept beside it, a reply nobody heard \
                deleted unless it ran tools), never over a turn another window started \
                meanwhile. Code blocks and tables are announced rather than skipped \
                silently (\"Code block, rust.\", \"Table.\"; in the thread's language for en, de, \
                fr, es and it), and the dashboard's read-aloud of a reply speaks the same way. \
                No audio is stored: a spoken turn is kept as text. \
                With chat_voice_audio_input on (Settings, or the thread's voice.audio_input), a \
                turn whose answering model takes audio input — the thread's chat model, or the \
                fallback a GPU hold, a benchmark run, an outside-VRAM verdict or a candidate \
                alias hands it to, wherever it runs — goes to it as audio at the commit, while \
                the speech-to-text model transcribes it beside (a model that does not take \
                audio input, one whose capabilities lmgw cannot read, or one lmgw cannot send \
                audio to, such as an Anthropic upstream, is refused it before anything is sent, \
                and the turn goes again as its transcript); \
                the reply is held until the transcript is in, then plays (a thread with no \
                speech-to-text model that resolves always goes as its transcript: only the \
                transcript tells words from noise). A turn that came back \
                without words ends the response quietly as response.done {cancelled, reason \
                no_words}, with no error and nothing written; a failed transcription of a turn \
                the model heard (it answered from the audio) plays the reply and marks the user \
                message (voice.transcript_error) instead of sending transcription.failed, while \
                one no model heard yet, or a turn never transcribed at all (no speech-to-text \
                model, the key's policy, the session's end), is said as with the setting off \
                and ends the response quietly as response.done {cancelled, reason \
                transcription_failed}. A new turn cuts a held reply whatever interrupt_response \
                says, and a held reply's tools run only once its user message is written (a turn \
                that came back without words runs none). A model's server that refuses the audio gets the transcript \
                once, and the session's later turns to that model go as text when the refusal \
                names the audio or the transcript retry answered (a server that went away under \
                the audio is not retried, and its later turns go as text too). \
                One session per thread: a second bind takes the thread over, and \
                the older session gets error chat_thread_taken_over and the close 4000. A \
                bound session — and no other — also sends lmgw's own events: \
                lmgw.chat.frame ({response_id, event, data}: the chat turn's frames \
                verbatim — turn, retrieval, delta, reasoning, tool, usage, stats, stop, \
                state, error, done; reasoning is shown, stored with the reply and never \
                spoken, and done's reasoning_note says in a sentence when the model reasoned \
                although the turn asked for reasoning off — a voice turn does unless the \
                thread sets reasoning; no off ends in an error — and its images_note when a \
                fallback that cannot see answered and got the turn's images as placeholders), \
                lmgw.chat.user ({message_id, content, voice, \
                response_id}: response_id when that response heard the turn as audio, whose \
                user message is written once the turn is transcribed, after the reply may have \
                started — the page puts it before that reply), lmgw.chat.input ({response_id, \
                input: audio or transcript, why}: how a response's turns reach the chat model \
                and why the transcript, sent at each launch, when an audio attempt was \
                refused, and when a llama-server that went away under the audio sends the \
                session's later turns to it as text; never with chat_voice_audio_input off), \
                lmgw.chat.reply ({message_id, content, unheard, voice}, {message_id, \
                removed: true} or {message_id, skipped, voice}), lmgw.model.state ({stage, alias, \
                state, ms, …}: the connect warm's and each turn's model loading), \
                lmgw.chat.thread ({chat_thread: {id, title, temporary, admin_tools}}: the thread \
                as a response re-read it, when that differs from session.lmgw.resolved.\
                chat_thread — a title the first spoken turn named, the admin tools switched on or \
                off) and lmgw.response.timing ({response_id, message_id, end_of_turn_ms, asr_ms, \
                first_token_ms, reasoning_ms, first_clause_ms, first_audio_ms, total_ms, \
                to_first_audio_ms, cold, models: {asr, chat, tts}, input, input_why, \
                transcript_wait_ms}: the timing log line as \
                data, each stage measured from the one before, reasoning_ms the part of \
                first_token_ms the model reasoned (absent when it did not), cold naming the \
                stages that loaded, input whether the chat model heard the turn as audio or \
                read its transcript, input_why why the transcript, transcript_wait_ms how long \
                the first output was held for the transcript (the three absent with \
                chat_voice_audio_input off, transcript_wait_ms on the transcript path), each model \
                with the alias that answered when a fallback did, first_clause: \
                \"announcement\" when the first clause said was one; sent once the reply is \
                written, message_id null when nothing was saved, and stored with the reply). \
                A refusal mid-session is an error event; its codes: owned_by_thread (a \
                client event changing what the thread owns), empty_turn (a response that would \
                answer no turn), superseded and not_saved (another turn took the thread, or the \
                reply could not be saved; its speech stops), chat_thread_not_found and \
                chat_thread_admin, tts_not_configured, voice_not_configured, voice_not_found, \
                instructions_required, voice_needs_transcript and speech_unavailable (the \
                thread's speech, judged per \
                response after response.created), chat_history_write_failed (the store refused \
                the spoken turn; its words lead the next user message), and \
                chat_thread_taken_over before the takeover's close.",
            query: Some(|g| g.root_schema_for::<crate::realtime::RealtimeQuery>()),
            response: Resp::WebSocket {
                subprotocol: crate::realtime::REALTIME_PROTOCOL,
                frames: "Each frame is one JSON text event of OpenAI's GA Realtime protocol: \
                    client events in, server events (each with a server-minted event_id) out. \
                    session.created arrives first.",
            },
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OPENAI],
            ..base(
                "GET",
                "/v1/realtime",
                "openai",
                "Open a realtime voice session",
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
        // -- MCP labels (realtime-server-tools design §1.4) --------------------------
        DocRoute {
            description: "The labels this caller may put into a {\"type\": \"mcp\", \
                \"server_label\"} tool on /v1/realtime or /v1/responses, for a tool picker. \
                Connects nothing: read from the configuration under the caller's tool scope — \
                enabled servers only, and the lmgw self-admin toolset only for an owner \
                credential while the self-admin tools are on.",
            response: Resp::Json(mcp::servers_list),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            ..base("GET", "/v1/mcp/servers", "mcp", "List the MCP labels")
        },
        DocRoute {
            description: "One label's tools, exactly as a /v1/realtime session's \
                mcp_list_tools item lists them (the server's own tool names, description \
                always a string), under the caller's tool scope and the owner's per-tool \
                switches. Connects that one server if it is not up (up to the lazy-list \
                budget). 404 for a label this caller may not use, naming the available ones; \
                502 when the server cannot be listed.",
            response: Resp::Json(mcp::server_detail),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            ..base(
                "GET",
                "/v1/mcp/servers/{label}",
                "mcp",
                "List one MCP label's tools",
            )
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
