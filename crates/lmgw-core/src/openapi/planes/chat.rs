//! The Chat API's documented routes (client-apps design §4.3): the subset
//! a desktop client uses — the thread and folder lists, a folder create,
//! an ongoing conversation's current thread, and the change feed — under
//! the "Chat" tag. Every shape is `lmgw-api-types`' (`chat`,
//! `chat_folders`, `chat_feed`), the types the handlers serialize. The rest
//! of the Chat API stays in `exclusions.rs` until a client needs it typed.

use lmgw_api_types::chat;
use lmgw_api_types::chat_feed as feed;
use lmgw_api_types::chat_folders;

use super::super::registry::{Dialect, DocRoute, Req, Resp};
use crate::web::chat_feed::FeedQuery;
use crate::web::ListThreadsQuery;

/// The two branches of a `*.created` / `*.updated` event, told apart by
/// `deleted` (review W6-8): the tombstone requires `deleted: true`, the row
/// has no `deleted`. Both shapes allow any field and require none, so
/// without this every instance matched both and `oneOf` matched none.
fn row_or_gone(row: serde_json::Value, gone: serde_json::Value) -> serde_json::Value {
    serde_json::json!([
        { "allOf": [row, { "not": { "required": ["deleted"] } }] },
        { "allOf": [gone, {
            "required": ["deleted"],
            "properties": { "deleted": { "const": true } }
        }] }
    ])
}

/// `thread.created` / `thread.updated`: the thread as the list carries it
/// now, or its tombstone once it is gone.
fn thread_now(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let row = g.subschema_for::<feed::ThreadChanged>().to_value();
    let gone = g.subschema_for::<feed::ThreadGone>().to_value();
    schemars::json_schema!({
        "description": "The thread as GET /chat/api/threads lists it now, with `by` beside its \
            fields; or, when the thread is gone by the time the event is delivered, \
            {thread_id, deleted: true, by}. `deleted` tells the two apart.",
        "oneOf": row_or_gone(row, gone)
    })
}

/// `folder.created` / `folder.updated`: the folder as the list carries it
/// now, or its tombstone.
fn folder_now(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let row = g.subschema_for::<feed::FolderChanged>().to_value();
    let gone = g.subschema_for::<feed::FolderGone>().to_value();
    schemars::json_schema!({
        "description": "The folder as GET /chat/api/folders lists it now (its counts as the \
            reader may see them), with `by` beside its fields; or, when the folder is gone by \
            the time the event is delivered, {folder_id, deleted: true, by}. `deleted` tells \
            the two apart.",
        "oneOf": row_or_gone(row, gone)
    })
}

/// The fields every documented Chat route shares.
fn chat_route(
    method: &'static str,
    path: &'static str,
    summary: &'static str,
    description: &'static str,
) -> DocRoute {
    DocRoute {
        method,
        path,
        tag: "chat",
        summary,
        description,
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::None,
        response: Resp::NoContent,
        dialect: Dialect::Dashboard,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        example: None,
    }
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            query: Some(|g| g.root_schema_for::<ListThreadsQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<chat::ThreadList>()),
            ..chat_route(
                "GET",
                "/chat/api/threads",
                "List the Chat's threads",
                "The stored threads, pinned first and then the most recently active; the \
                 temporary threads in a list of their own; how many threads are archived; and \
                 every folder with its thread counts. ?archived=1 lists the archived threads \
                 instead, the most recently archived first, and ?archived=all both. A device \
                 key's lists and counts leave out every Admin Chat thread, and every thread \
                 that carries the self-admin toolset (lmgw) unless the device may use lmgw's \
                 admin tools. Open the change feed first, then load this list: the feed's \
                 thread.* and folder.* events keep it current from then on.",
            )
        },
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<chat::FolderList>()),
            ..chat_route(
                "GET",
                "/chat/api/folders",
                "List the Chat's folders",
                "Every folder in the list's order: its defaults (what a new thread in it \
                 starts with), whether it is one ongoing conversation and which thread is \
                 current, its own retention, and its thread counts as the caller may see \
                 them. Folder names need not be unique, so a client that looks its folder up \
                 by name keeps the id it found.",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<chat::FolderCreate>()),
            response: Resp::Json(|g| g.root_schema_for::<chat::Folder>()),
            ..chat_route(
                "POST",
                "/chat/api/folders",
                "Create a folder",
                "A new folder, last in the list, answered as the list carries it. `ongoing` \
                 marks it as one ongoing conversation, which needs a model in its defaults: \
                 without one the answer is a 400 naming defaults.model_alias. A device key \
                 writes only within its own scopes: a tool server or knowledge base its tool \
                 scope does not reach is a 403 tool_label_out_of_scope, an alias outside its \
                 alias scope a 403 key_scope, and the self-admin toolset (lmgw) is its to \
                 attach only when its admin tools are full: below that the answer is a 403 \
                 chat_toolset_needs_full, or tool_label_out_of_scope when they are off. A \
                 device key cannot set archive_days or purge_days (403 forbidden, naming the \
                 fields): a folder's own retention is set from the dashboard.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::OptionalJson(|g| g.root_schema_for::<chat_folders::CurrentRequest>()),
            response: Resp::Json(|g| g.root_schema_for::<chat_folders::CurrentThread>()),
            ..chat_route(
                "POST",
                "/chat/api/folders/{id}/current",
                "Get an ongoing conversation's current thread",
                "The thread every client of an ongoing-conversation folder continues in. A \
                 new one starts when the folder has none the caller can reach (first), when \
                 no message came for the folder's idle minutes while no turn runs there \
                 (idle), or on new: true (requested) — except that a current thread with no \
                 message yet is answered instead, so no empty threads pile up. The body may \
                 be left out: it is new: false. A new thread starts from the folder's \
                 defaults, and every client hears of it as the feed's folder.current. Two \
                 clients asking at once get the same thread. A folder that is not an ongoing \
                 conversation is a 409 not_ongoing, one whose defaults name no model a 409 \
                 folder_no_model, and one the caller may not see a 404.",
            )
        },
        DocRoute {
            method: "GET",
            path: "/chat/api/feed",
            tag: "chat",
            summary: "Follow the Chat's changes",
            description: "One SSE stream per client. `hello` first: the \
            epoch, the cursor the stream continues from, the keep-alive interval and the \
            retention, who the feed is for, the hold, and the turns and voice sessions live \
            now. Then stored events (thread.*, folder.*, folder.current), each with an `id:` \
            that is its cursor \"<epoch>:<seq>:<tag>\" (opaque: hand it back as it came), \
            rendered as the thread or folder is when it \
            is delivered; and live events (turn.*, voice.*, hold, state), which have no id. \
            Open the feed first, then load the list: a turn in `hello` may name a thread \
            whose thread.created follows in the catch-up. Resume with ?since=<cursor> or \
            Last-Event-ID (which wins: a browser reconnects with the URL it opened). A cursor \
            the feed cannot honour — another database's, one whose event is not the one the \
            gateway holds at that number now (its data restored from an older copy), older \
            than the retention, newer than the newest — gets `resync` with the reason, then \
            events from now; so does a stream still catching up when the retention prunes \
            past it, and one at an event that cannot be read after a few attempts. A client that reads live events slower than they \
            happen, past the live buffer, gets `state` instead of the events it missed. \
            Keep-alive comments every keepalive_s seconds; one may carry an `id:` and no \
            data, which only moves the cursor. When the key is disabled, rotated, deleted or \
            expires, `revoked` and the end. When the gateway shuts down, the stream ends \
            without an event: reconnect with the last cursor once it is back. A device key never \
            receives an event about an Admin Chat thread, nor about a folder a device deleted \
            while it held threads the device could not see (until it is shown to devices \
            again); and about a thread or folder with the self-admin toolset (lmgw) attached \
            only while what the device's admin tools may do is above off \
            (hello.self_admin: its level capped by the gateway's self-admin level). The gaps \
            that \
            leaves in the sequence are expected, and a thread in a folder the device does not \
            see is in no folder for it. When a thread or folder leaves a device's reach (the \
            toolset attached), the device receives its deletion, and a fresh `state` when that \
            changed what is live for it; when it comes back (the toolset taken off, the folder \
            shown again), its creation, and the same. When what the device's admin tools may \
            do moves above off (its own level raised, or the gateway's self-admin level raised \
            from off), the threads and folders with the toolset arrive as \
            thread.created and folder.created (and its threads in such a folder as \
            thread.updated), then a fresh `state`; when it moves to off, they go as \
            thread.deleted and folder.deleted, in a delete's order, then a fresh `state`. \
            `state` carries self_admin, what the device's admin tools may do now (its level \
            capped by the gateway's self-admin level), and follows every change of the \
            device's level and every change of the gateway's that moves it, so a connected \
            device learns there that it changed. A device that resumes from a cursor \
            older than a change above off or back gets `resync` at that point instead, and \
            nothing about those threads and folders that it may not see now, or that was \
            written while it could not see them: reload what you show.",
            tool: None,
            query: Some(|g| g.root_schema_for::<FeedQuery>()),
            path_ints: &[],
            request: Req::None,
            response: Resp::Sse(&[
                ("hello", |g| g.root_schema_for::<feed::Hello>()),
                ("thread.created", thread_now),
                ("thread.updated", thread_now),
                ("thread.deleted", |g| {
                    g.root_schema_for::<feed::ThreadGone>()
                }),
                ("folder.created", folder_now),
                ("folder.updated", folder_now),
                ("folder.deleted", |g| {
                    g.root_schema_for::<feed::FolderGone>()
                }),
                ("folder.current", |g| {
                    g.root_schema_for::<feed::FolderCurrent>()
                }),
                ("turn.started", |g| g.root_schema_for::<feed::TurnStarted>()),
                ("turn.done", |g| g.root_schema_for::<feed::TurnDone>()),
                ("voice.bound", |g| g.root_schema_for::<feed::VoiceBound>()),
                ("voice.ended", |g| g.root_schema_for::<feed::VoiceEnded>()),
                ("hold", |g| g.root_schema_for::<feed::FeedHold>()),
                ("state", |g| g.root_schema_for::<feed::LiveState>()),
                ("resync", |g| g.root_schema_for::<feed::Resync>()),
                ("revoked", |g| g.root_schema_for::<feed::Revoked>()),
            ]),
            dialect: Dialect::Dashboard,
            endpoints: &[],
            model_task: None,
            confirm_note: None,
            writes: None,
            example: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The feed's documented events are the ones a client's `FeedEvent`
    /// reads, no more and no fewer.
    #[test]
    fn the_feed_documents_every_event_a_client_reads() {
        let feed = routes()
            .into_iter()
            .find(|r| r.path == "/chat/api/feed")
            .expect("the feed is documented");
        let Resp::Sse(events) = feed.response else {
            panic!("the feed is an event stream")
        };
        let mut documented: Vec<&str> = events.iter().map(|(name, _)| *name).collect();
        let mut read: Vec<&str> = feed::EVENTS.to_vec();
        documented.sort_unstable();
        read.sort_unstable();
        assert_eq!(documented, read);
    }
}
