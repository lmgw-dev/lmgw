//! The Chat API's thread routes, under the "Chat" tag and merged into the
//! Chat plane (`planes::chat`): creating a thread, reading one whole, its
//! settings, pin, archive, move, keep and delete, the speech stop, the
//! message delete and edit, the folder patch and delete, and the search.
//! Every shape is `lmgw-api-types`' (`chat`, `chat_threads`,
//! `chat_folders`), the types the handlers read and write.

use lmgw_api_types::chat;
use lmgw_api_types::chat_folders;
use lmgw_api_types::chat_threads as threads;

use super::super::registry::{DocRoute, Req, Resp};
use super::chat::chat_route;
use super::chat_turns::turn_events;
use crate::web::{RowsQuery, SearchQuery};

fn ack(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
    g.root_schema_for::<chat::Ack>()
}

fn thread(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
    g.root_schema_for::<chat::Thread>()
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<threads::ThreadCreate>()),
            response: Resp::Json(thread),
            ..chat_route(
                "POST",
                "/chat/api/threads",
                "Create a thread",
                "A new thread, answered as the thread list carries it, with `voice_resolved`. A plain chat thread starts from the Chat's default system prompt \
                 and personality profile; an Admin Chat thread (kind: admin) has its built-in \
                 prompt. temporary: true creates a temporary thread kept in memory only, with \
                 a negative id; it is a chat thread whatever kind says. folder_id starts the \
                 thread from that folder's defaults laid over the Chat's own, and in an \
                 ongoing-conversation folder it becomes the current thread. A temporary \
                 thread in a folder is a 400 bad_request; a folder the caller cannot see a \
                 404 not_found. A device key cannot create an Admin Chat thread (403 \
                 forbidden), and the model alias must be within its alias scope (403 \
                 key_scope).",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<threads::ThreadDetail>()),
            ..chat_route(
                "GET",
                "/chat/api/threads/{id}",
                "Read a thread whole",
                "The thread with its settings, `voice_resolved` and `continue` (whether its \
                 last reply can be continued), every message oldest first, each with its \
                 attachments, the draft attachments that wait for the next message, and the \
                 thread's MCP tasks still running or waiting to enter it. A message of a gated \
                 turn lists the calls it still waits on as `pending_approvals`; \
                 POST /chat/api/threads/{id}/approvals decides them. A thread that is not \
                 there, or not the caller's to see, is a 404 not_found; a read that failed a \
                 500 internal, never an empty history. Read the thread again on every change \
                 the feed reports.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<threads::SettingsPatch>()),
            response: Resp::Json(|g| g.root_schema_for::<threads::SettingsAck>()),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/settings",
                "Change a thread's settings",
                "A patch: a field left out stays as it is, and for the settings that can be \
                 unset, null clears it back to the route's default. Settings that contradict \
                 each other (the reasoning and sampling overrides, the knowledge settings, a \
                 voice alias that cannot do what it is for) are a 400 bad_request and nothing \
                 is written; an unknown profile_id is a 400 unknown_profile. The answer \
                 re-judges `continue` under the new settings, and carries the voice as stored \
                 and what it resolves to now. A device key's mcp_tools and knowledge bases \
                 pass its tool scope first (403 tool_label_out_of_scope), its model and voice \
                 aliases its alias scope (403 key_scope); a device below full admin level \
                 changes no setting of a thread with lmgw's admin tools (403 \
                 chat_toolset_needs_full). A thread that is gone is a 404 not_found.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(ack),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/delete",
                "Delete a thread",
                "Deletes the thread with its messages and attachments. The owner's delete of a thread that is not there still answers \
                 ok; a device key's delete of a thread it cannot reach (gone, or Admin Chat) \
                 is a 404 not_found.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<threads::SpeechStopped>()),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/speech/stop",
                "Stop a thread's read-aloud",
                "Stops the thread's running read-alouds (a message spoken on request, or a \
                 reply read aloud as it streams); `stopped` counts them, 0 when nothing was \
                 playing. A thread the caller cannot reach is a 404 not_found.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<threads::PinRequest>()),
            response: Resp::Json(thread),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/pin",
                "Pin or unpin a thread",
                "Pinned threads are listed first and are never archived or deleted by the \
                 retention sweep. Pinning an archived thread also restores it. Answers the \
                 thread as the list carries it. A temporary thread has nothing to pin until \
                 it is kept: 409 temporary_thread. A thread the caller cannot reach is a 404 \
                 not_found.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<threads::MoveRequest>()),
            response: Resp::Json(thread),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/move",
                "Move a thread into a folder",
                "Moves the thread into the folder, or out of any folder with folder_id: null, \
                 and answers the thread as the list carries it. The change reaches every \
                 client as a thread.updated event. A temporary thread has no folder until it \
                 is kept: 409 temporary_thread. A thread or folder the caller cannot reach is \
                 a 404 not_found.",
            )
        },
        DocRoute {
            query: Some(|g| g.root_schema_for::<RowsQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<chat::ThreadRows>()),
            ..chat_route(
                "GET",
                "/chat/api/threads/rows",
                "Read some of the thread list's rows",
                "The rows of the threads named in `ids` (comma-separated, as 3,7), each \
                 exactly as GET /chat/api/threads lists it, active or archived, in the active \
                 list's order. For a client that follows the change feed and wants only the \
                 rows a `chat` frame named, instead of the whole list: a frame that names \
                 only messages moved those threads' updated_at and last_message_at and \
                 nothing else the list shows, so read these rows and sort the list again. \
                 Only stored threads are answered: a temporary id, one that is not there \
                 and one out of the caller's reach are left out, so a missing row is never \
                 an error. There is no cap on `ids` beyond the 64 KiB a request line may be \
                 (about 9,000 ids; more is a 414, and the whole list is then the read to \
                 fall back to). A part of `ids` that is not a number is a 400 bad_request. \
                 Owner only: a paired device's own change feed already carries whole rows.",
            )
        },
        DocRoute {
            query: Some(|g| g.root_schema_for::<SearchQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<threads::SearchPage>()),
            ..chat_route(
                "GET",
                "/chat/api/search",
                "Search the Chat",
                "Full-text search over thread titles, message text and the names of sent \
                 attachments, in pages of 50 threads, each with up to three of its best hits. \
                 Temporary threads are never searched. A device key's search leaves out Admin \
                 Chat threads, and names no thread as being in a folder it cannot see. A \
                 query shorter than two characters, or archived other than 0, 1 or all, is a \
                 400 bad_request.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<chat_folders::FolderPatch>()),
            response: Resp::Json(|g| g.root_schema_for::<chat_folders::FolderPatched>()),
            ..chat_route(
                "POST",
                "/chat/api/folders/{id}",
                "Change a folder",
                "A patch: a field left out is unchanged. Answers the folder as the list \
                 carries it, with `applied`: for an ongoing-conversation folder whose \
                 defaults changed (and apply_to_current not false), the current thread and \
                 the settings that reached it, otherwise null. The folder's other threads are \
                 never touched. defaults replaces the defaults whole; defaults_patch changes \
                 the fields it names (null unsets one) so a client never writes back what \
                 another changed meanwhile; both together are a 400 bad_request. ongoing \
                 needs a model in the defaults. What reaches the current thread passes the \
                 checks of a thread settings write, and a field the thread refuses fails the \
                 whole patch. A device key's default mcp_tools pass its tool scope, as on a \
                 folder create; it cannot set archive_days or purge_days, nor devices_hidden \
                 (403 forbidden). devices_hidden: false shows a folder to devices again that \
                 a device's delete hid from them; true is a 400. A folder the caller cannot \
                 see is a 404 not_found.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<chat_folders::FolderDelete>()),
            response: Resp::Json(ack),
            ..chat_route(
                "POST",
                "/chat/api/folders/{id}/delete",
                "Delete a folder",
                "threads: keep leaves the folder's threads in no folder, threads: delete \
                 deletes them with it; there is no default for destroying conversations. A \
                 device key's delete touches only the threads it can see: a folder that \
                 also holds Admin Chat or self-admin threads it cannot reach stays for them, \
                 hidden from every device from then on, and the answer is the same ok. A \
                 folder the caller cannot see is a 404 not_found.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<threads::ArchiveRequest>()),
            response: Resp::Json(thread),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/archive",
                "Archive or restore a thread",
                "archived: true archives the thread by hand; false restores it. Answers the \
                 thread as the list carries it. A temporary thread is discarded, not \
                 archived: 409 temporary_thread. A thread the caller cannot reach is a 404 \
                 not_found.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<threads::ThreadKept>()),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/persist",
                "Keep a temporary thread",
                "Writes a temporary thread (messages, attachments, settings) to the database \
                 as an ordinary thread and drops it from memory. Answers the stored thread \
                 and its new, positive id; the temporary id is gone. A thread that is already \
                 stored is a 409 not_temporary; a keep of the same thread already running a \
                 409 keep_in_progress; a keep while a realtime voice session is bound to the \
                 thread a 409 voice_session_active. An unknown temporary thread is a 404 \
                 not_found.",
            )
        },
        DocRoute {
            path_ints: &["id", "mid"],
            response: Resp::Json(ack),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/messages/{mid}/delete",
                "Delete a message",
                "Deletes that one message with its attachments; nothing after it moves. A \
                 device key below full admin level changes no message of a thread with lmgw's \
                 admin tools (403 chat_toolset_needs_full). A thread or message that is not \
                 there is a 404 not_found.",
            )
        },
        DocRoute {
            path_ints: &["id", "mid"],
            request: Req::Json(|g| g.root_schema_for::<threads::MessageEdit>()),
            response: Resp::JsonOrSse {
                json: |g| g.root_schema_for::<threads::ReplyEdited>(),
                events: turn_events!(turn, speech),
            },
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/messages/{mid}/edit",
                "Edit a message",
                "What the answer is depends on the message. A reply is rewritten in place and \
                 nothing is sent: its reasoning, token counts and tool record are cleared, \
                 since they no longer describe the text, and so are a spoken reply's unheard \
                 rest and timing; the answer is JSON, `ok` and the message as the thread \
                 lists it. A user message is rewritten in place (its attachments stay bound), \
                 every later message is deleted, and it is answered again: the answer is the \
                 event stream of a send, opening with a `turn` frame. An empty text is a 400 \
                 empty_message unless the message carries files. Its stored knowledge \
                 retrieval goes with the old text, so auto mode searches again; kb_refs, when \
                 sent, replaces its `#` picks. A dictated message whose text changed becomes \
                 a typed one. Everything that can refuse happens before anything is written. \
                 Any other role is a 400 bad_request. A device key below full admin level \
                 edits no message of a thread with lmgw's admin tools (403 \
                 chat_toolset_needs_full); its kb_refs pass its tool scope. A thread or \
                 message that is not there is a 404 not_found.",
            )
        },
    ]
}
