//! The Chat API's streaming turn routes, under the "Chat" tag and merged
//! into the Chat plane (`planes::chat`): send, continue, regenerate, the
//! read-aloud of a stored reply and the voice warm-up. Their requests are
//! `lmgw-api-types`' `chat_turn`, their frames `chat_frames` — the types the
//! handlers parse and write.
//!
//! This module also holds the frame vocabulary every turn route shares
//! (send, continue, regenerate, edit, approvals, a job's answer): one schema
//! function per event name, and [`turn_events!`] for the list a route
//! streams.

use lmgw_api_types::chat_frames as frames;
use lmgw_api_types::chat_turn::{SendRequest, TurnRequest, WarmRequest};
use lmgw_api_types::chat_voice::ModelState;
use schemars::{JsonSchema, Schema, SchemaGenerator};

use super::super::registry::{DocRoute, Req, Resp};
use super::chat::chat_route;

fn of<T: JsonSchema>(g: &mut SchemaGenerator) -> Schema {
    g.root_schema_for::<T>()
}

pub(super) fn turn_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::TurnStarted>(g)
}
pub(super) fn retrieval_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::RetrievalFrame>(g)
}
pub(super) fn text_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::TextFrame>(g)
}
pub(super) fn tool_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::ToolFrame>(g)
}
pub(super) fn usage_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::UsageFrame>(g)
}
pub(super) fn stop_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::StopFrame>(g)
}
pub(super) fn stats_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::Timings>(g)
}
pub(super) fn error_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::ErrorFrame>(g)
}
pub(super) fn done_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::DoneFrame>(g)
}
pub(super) fn state_frame(g: &mut SchemaGenerator) -> Schema {
    of::<ModelState>(g)
}
pub(super) fn voice_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::VoiceFrame>(g)
}
pub(super) fn speech_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::SpeechFrame>(g)
}
pub(super) fn speech_done_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::SpeechDone>(g)
}
pub(super) fn speech_error_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::SpeechError>(g)
}
fn warm_done_frame(g: &mut SchemaGenerator) -> Schema {
    of::<frames::WarmDone>(g)
}

/// The events a turn route streams: `turn_events!()` for those every turn
/// can send; `turn_events!(turn)` adds `turn` (a route that writes a user
/// message first); `turn_events!(speech)` adds the speech frames of a request
/// that may ask to `speak`; `turn_events!(turn, speech)` both.
macro_rules! turn_events {
    (@all [$($pre:tt)*] [$($post:tt)*]) => {
        &[
            $($pre)*
            ("state", $crate::openapi::planes::chat_turns::state_frame),
            ("retrieval", $crate::openapi::planes::chat_turns::retrieval_frame),
            ("delta", $crate::openapi::planes::chat_turns::text_frame),
            ("reasoning", $crate::openapi::planes::chat_turns::text_frame),
            ("tool", $crate::openapi::planes::chat_turns::tool_frame),
            ("usage", $crate::openapi::planes::chat_turns::usage_frame),
            ("stop", $crate::openapi::planes::chat_turns::stop_frame),
            ("stats", $crate::openapi::planes::chat_turns::stats_frame),
            ("error", $crate::openapi::planes::chat_turns::error_frame),
            ("done", $crate::openapi::planes::chat_turns::done_frame),
            $($post)*
        ]
    };
    () => { turn_events!(@all [] []) };
    (turn) => {
        turn_events!(@all [("turn", $crate::openapi::planes::chat_turns::turn_frame),] [])
    };
    (speech) => {
        turn_events!(@all [] [
            ("voice", $crate::openapi::planes::chat_turns::voice_frame),
            ("speech", $crate::openapi::planes::chat_turns::speech_frame),
            ("speech_done", $crate::openapi::planes::chat_turns::speech_done_frame),
            ("speech_error", $crate::openapi::planes::chat_turns::speech_error_frame),
        ])
    };
    (turn, speech) => {
        turn_events!(@all [("turn", $crate::openapi::planes::chat_turns::turn_frame),] [
            ("voice", $crate::openapi::planes::chat_turns::voice_frame),
            ("speech", $crate::openapi::planes::chat_turns::speech_frame),
            ("speech_done", $crate::openapi::planes::chat_turns::speech_done_frame),
            ("speech_error", $crate::openapi::planes::chat_turns::speech_error_frame),
        ])
    };
}
pub(crate) use turn_events;

/// What every turn stream has in common, for the routes' descriptions (a
/// macro, so `concat!` can take it).
macro_rules! frames_doc {
    () => {
        "The answer is a server-sent event stream; every frame's data is JSON of \
    the type its event name has below. state: a model stage loading, held, falling back or \
    failing (the thread's model); retrieval: what the knowledge bases found for the user \
    message, before the model answers; delta: a piece of the answer; reasoning: a piece of \
    the model's reasoning; tool: a tool call of the turn, by its `event` (start, args, ready, \
    approval, result); usage: the token counts so far; stop: why the model stopped; stats: a \
    local model's timings; error: a failure, the stream goes on to done; done: the end, \
    naming the reply it saved and its token counts, and `pending_approvals` when it stopped \
    on calls that wait for a decision. A turn that is refused before it says anything \
    streams error and done {aborted: true}. A client that closes the stream stops the turn; \
    the partial reply is saved."
    };
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<SendRequest>()),
            response: Resp::Sse(turn_events!(turn, speech)),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/send",
                "Send a message and stream the answer",
                concat!(
                    "Writes the user message (with the draft attachments it binds and the \
                     knowledge bases it picks) and streams the model's answer. The message is \
                     stored before the model is called, so a client that goes away has still \
                     recorded what it asked; an untitled thread is titled from it. `content` \
                     may be empty when attachments are bound. A dictated message names its \
                     origin in `voice` (via: dictation); with `speak`, the reply is read aloud \
                     as it streams and the stream also carries voice, speech, speech_done and \
                     speech_error frames (the speech frames' audio is base64 PCM16, 24 kHz, \
                     mono). The first frame is `turn` with the user message's id. ",
                    "Refused before anything is written: 400 empty_message (no text and no \
                     attachment), 400 bad_request (a knowledge base that does not exist), \
                     400 attachment_not_draft (an id that is no draft of the thread), 400 \
                     model_no_vision (a new image and a model that cannot see), 422 \
                     attachment_blocked (a file the thread's model cannot take, named), 404 \
                     not_found and a device key's own refusals (its scope, its budget). A body that is not \
                     JSON is 400 bad_request, JSON of another shape (an unknown or mistyped \
                     field, `voice` included) 422 bad_request, and a `voice.via` other than \
                     dictation (realtime, say) 400 bad_request. 409 \
                     attachment_not_draft: a concurrent send took a draft first. A turn that \
                     fails after the stream began says so in an error frame. ",
                    frames_doc!()
                ),
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::OptionalJson(|g| g.root_schema_for::<TurnRequest>()),
            response: Resp::Sse(turn_events!(speech)),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/continue",
                "Continue the thread's last reply",
                "The model continues the thread's last reply (an assistant prefill) and \
                 streams the continuation only: the delta frames are what is added, done's \
                 message_id is the continued reply, and the stored reply's text and reasoning \
                 grow by what came (its token counts become this call's). The body is optional (a body that is not JSON is 400 bad_request, JSON of another \
                 shape 422 bad_request); \
                 with `speak` the continuation is read aloud as it streams, from the clause the \
                 reply broke off in. 409 continue_unavailable when the thread's `continue` \
                 state says no (its reason is the message): there is no reply yet, the last \
                 message is not a reply, the reply ran tools, or the thread's model cannot take \
                 a prefill. The same code answers a reply that stopped being the last message \
                 meanwhile. A route the \
                 request is re-routed to that cannot take a prefill is refused in the stream, \
                 with an error frame. 404 not_found, and a device key's own refusals. The \
                 frames are those of a send, without the `turn` frame.",
            )
        },
        DocRoute {
            path_ints: &["id", "mid"],
            request: Req::OptionalJson(|g| g.root_schema_for::<TurnRequest>()),
            response: Resp::Sse(turn_events!(turn, speech)),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/messages/{mid}/regenerate",
                "Answer a message again",
                "Cuts the thread at the message and streams a new answer. On a reply, it and \
                 every later message are deleted and the model answers the history before it, \
                 which has to end with a user message (or a job's result): 409 \
                 nothing_to_answer otherwise, and nothing is deleted. On a user message, every \
                 later message is deleted, it is answered again, and the stream opens with a \
                 `turn` frame carrying its id. Any other role is 400 bad_request; 404 \
                 not_found for a thread or message that is not there; a device key below full \
                 admin level changes no message of a thread with lmgw's admin tools (403 \
                 chat_toolset_needs_full). The model's capabilities are read before anything \
                 is cut. The body is optional (a body that is not JSON is 400 bad_request, JSON of \
                 another shape 422 bad_request); with `speak` the answer is read aloud as it \
                 streams. The frames are those of a send.",
            )
        },
        DocRoute {
            path_ints: &["id", "mid"],
            response: Resp::Sse(&[
                ("state", state_frame),
                ("voice", voice_frame),
                ("speech", speech_frame),
                ("speech_done", speech_done_frame),
                ("speech_error", speech_error_frame),
            ]),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/messages/{mid}/speak",
                "Read a stored reply aloud",
                "Reads the reply as it is shown (its content, then a spoken reply's unheard \
                 rest) with the thread's text-to-speech model and voice, and streams the audio \
                 as it is made: `state` while the speech model is not resident (loading, then \
                 ready, fallback, held or failed), `voice` once its route is open, a `speech` \
                 frame per clause (seq counting from 0, the text said, pcm: base64 PCM16 \
                 little-endian, 24 kHz, mono) and `speech_done` with the totals. A thread that \
                 cannot speak (no speech model, no such voice, a voice that needs instructions) \
                 and a voice that fails mid-way end with a `speech_error` frame (codes \
                 tts_not_configured, voice_not_found, voice_not_configured, \
                 instructions_required, or the failure's kind). One speech row is written per \
                 call. Closing the stream stops the synthesis at its next step; \
                 POST /chat/api/threads/{id}/speech/stop stops every read-aloud of the thread. \
                 Refused before any frame: 404 not_found (thread or message), 400 bad_request \
                 (the message is not a reply), 400 empty_message (a reply with no text).",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<WarmRequest>()),
            response: Resp::Sse(&[("state", state_frame), ("done", warm_done_frame)]),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/voice/warm",
                "Warm a thread's speech models",
                "A press of the microphone (or the start of a call) warms the models the voice \
                 turn will use before the request it announces. `stages` names them (asr, tts, \
                 chat: the thread's own model), each once; they are the thread's resolved \
                 aliases, warmed as one group through the request admission, so the warm may \
                 evict idle models exactly as the request would, and a group that does not fit \
                 together is warmed without evicting. The stream sends one `state` frame per \
                 change of each stage (loading, then ready with the time it took, or held, \
                 fallback, skipped, failed), then `done` with no fields once every stage has \
                 settled. Closing the stream drops what still waits for admission; a container \
                 start already in flight finishes. Refused before anything is warmed: 404 \
                 not_found, 400 bad_request (no stages, or an unknown name), 422 \
                 asr_not_configured or tts_not_configured (no alias for the stage at any \
                 level), and, for a device key, its own refusal (scope, budget) for a stage \
                 whose alias it may not use — nothing starts then.",
            )
        },
    ]
}
