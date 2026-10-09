//! MCP approvals' documented routes (client-apps design §6), under the
//! "Chat" tag and merged into the Chat plane (`planes::chat`): deciding a
//! gated turn's calls, and the dictation route a voice answer goes through
//! to stay out of the conversation. Every shape is `lmgw-api-types`'
//! (`chat_approvals`, `chat_voice`). No op and no `x-lmgw` header.

use lmgw_api_types::chat_approvals as approvals;
use lmgw_api_types::chat_voice::Dictation;

use super::super::registry::{DocRoute, Req, Resp};
use super::chat::chat_route;

/// A `tool` frame of a turn: `{event: "approval", approval_request_id,
/// server_label, name, arguments, call_id}` for a call that waits, or the
/// start, args, ready and result frames of a call that runs.
pub(super) fn tool_frame(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "description": "A tool call of the turn, by `event`: start, args, ready ({index, \
            name, arguments, call_id, server_label, needs_approval: the call waits for a \
            decision and an approval frame follows, ui_resource: the namespaced MCP Apps UI \
            resource the tool links to — what a client that shows the call's view at once \
            needs) and result ({index, name, call_id, output, is_error, ms, server_label, \
            ui_resource, structured_content: the MCP result's structuredContent, content: its \
            content blocks as the server sent them — images in full, no bound of lmgw's own — \
            and task {id, task_id, server_label} for a call that started an MCP task: its \
            output is `started, job <task_id>`, and the result enters the thread later); the \
            model is given a result's content, and its structured content only when the \
            content is empty), or approval ({approval_request_id, server_label, name, \
            arguments, call_id}: a call that waits for a decision; name is the tool's own, \
            arguments a JSON string, call_id the call's id as its ready and result frames \
            carry it, to match the approval to the call by).",
        "required": ["event"],
        "properties": { "event": { "type": "string" } }
    })
}

/// `done`: what the turn saved, and the calls it waits on.
pub(super) fn done_frame(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let pending = g
        .subschema_for::<Vec<approvals::ApprovalRequest>>()
        .to_value();
    schemars::json_schema!({
        "type": "object",
        "description": "The turn's end: message_id (the reply it saved or appended to; absent \
            when nothing was saved), saved, model, answered_by, prompt_tokens, \
            completion_tokens, aborted, and pending_approvals when it stopped on calls that \
            wait for a decision again.",
        "properties": {
            "message_id": { "type": "integer" },
            "saved": { "type": "boolean" },
            "aborted": { "type": "boolean" },
            "pending_approvals": pending
        }
    })
}

pub(super) fn text_frame(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": { "text": { "type": "string" } }
    })
}

pub(super) fn error_frame(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": { "message": { "type": "string" }, "code": { "type": "string" } }
    })
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<approvals::ApprovalsRequest>()),
            response: Resp::Sse(&[
                ("delta", text_frame),
                ("reasoning", text_frame),
                ("tool", tool_frame),
                ("error", error_frame),
                ("done", done_frame),
            ]),
            ..chat_route(
                "POST",
                approvals::PATH,
                "Decide the calls a turn waits on",
                "A thread's MCP tools may wait for an approval (the thread's mcp_tools entry's \
                 require_approval: \"never\", \"always\" or {always: {tool_names}, never: \
                 {tool_names}}, as OpenAI's; any other shape, read_only included, is refused, \
                 and a device key may only tighten it, 403 approval_loosen_refused). A turn \
                 that calls one stops: \
                 it streams a tool frame {event: approval, approval_request_id, server_label, \
                 name, arguments, call_id} for each waiting call (call_id the id the call's \
                 ready and result frames carry) and done {pending_approvals}, and saves \
                 its reply with them; the other calls of that model turn wait with them. The \
                 thread's messages list them as the reply's pending_approvals, and the feed \
                 records approval.requested, each with the call_id. This route takes a verdict for every waiting call \
                 and resumes the turn as a continuation, streaming the frames a send streams \
                 (delta, reasoning, tool, usage, stop, error, done): the declined calls are \
                 answered \"The user declined this tool call: <reason>\", the approved ones and \
                 the calls that waited with them run, and the model answers; what it adds is \
                 appended to the same reply. The resumed turn runs as the principal that \
                 started it — its scope, key policy and attribution — whoever approves. The \
                 approver is on each approved call's request row (approved_by), in the feed's \
                 approval.decided, and in _meta[\"lmgw/approval\"] {decision: approved, by: \
                 {kind, name}} of a call forwarded to a device-hosted server; a call nobody \
                 approved carries null there. The first decision wins: a call decided already \
                 is 409 approval_decided, naming who decided. A waiting call without a verdict \
                 is 400 approval_missing, naming each; an id nothing waits under 404 \
                 approval_not_found; a starter whose key is gone or disabled 409 \
                 approval_starter_unavailable, naming it (nothing is decided then; nor when \
                 the starter's key is at its concurrency limit, its 429 key_rate); a reply \
                 that is no longer the thread's last message 409 approval_moved_on. A device \
                 approves only within its own reach: a call its key's tool scope does not \
                 admit, or one of lmgw's admin tools unless its own admin tools may do \
                 everything (full, capped by the gateway's level), is 403 \
                 approval_out_of_scope, naming each, and nothing of the request is decided; \
                 declining is open to any client that sees the thread. A decided turn that \
                 never starts closes its calls as not run, and the feed's approval.decided \
                 says approve: false, not_run: true for an approved one. A new \
                 message in the thread declines whatever still waits, \"the user moved on \
                 without deciding\", so every call is answered. speak reads the resumed reply \
                 aloud as a send's does.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Raw("audio/wav"),
            response: Resp::Json(|g| g.root_schema_for::<Dictation>()),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/transcribe",
                "Transcribe a recording",
                "The body is a recording, its container named by Content-Type: audio/wav (also \
                 audio/wave, audio/x-wav, audio/vnd.wave, none, application/octet-stream), \
                 audio/webm, audio/ogg (audio/opus), audio/mpeg (audio/mp3), audio/mp4 \
                 (audio/m4a, audio/x-m4a) or audio/flac (audio/x-flac); another is 415 \
                 unsupported_media_type. It is transcribed by the thread's speech-to-text model \
                 and written nowhere: dictation, and a spoken answer to an approval, which must \
                 stay out of the conversation (a new message declines a waiting call). The \
                 answer names the alias, the one that answered in its place (a GPU-hold or \
                 outside-VRAM fallback, also in x-lmgw-fallback), the call's time, the \
                 recording's length (WAV only) and the language heard. The body is bounded by \
                 max_body_mb (413 body_limit naming it). 400 empty_audio, 422 \
                 asr_not_configured, 503 gpu_hold, and a device key's own refusals.",
            )
        },
    ]
}
