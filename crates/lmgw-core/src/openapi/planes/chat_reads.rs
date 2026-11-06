//! The Chat's reads beside its threads and folders (chat-plane design §2),
//! under the "Chat" tag: the settings it uses, the toolsets a thread may
//! attach and one's tools, the knowledge bases and the source viewer. Every
//! shape is `lmgw_api_types::chat_reads`' (the source viewer's query
//! `lmgw_api_types::knowledge`'s), the types the handlers write.

use lmgw_api_types::chat_reads as reads;
use lmgw_api_types::knowledge as kb;

use super::super::registry::{DocRoute, Resp};
use super::chat::chat_route;

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<reads::ChatSettings>()),
            ..chat_route(
                "GET",
                "/chat/api/settings",
                "Read the settings the Chat uses",
                "The gateway settings a Chat client reads, and only those: the knowledge \
                 excerpt budget a thread without its own uses, the global archive and purge \
                 days an ongoing folder's empty fields fall back to, the Chat's and the \
                 realtime voice's text-to-speech alias (a thread inherits the first that is \
                 not empty), the largest request body (a dictation's recording is one), the \
                 seconds between two reads of an MCP task's status, and a realtime session's \
                 pre-roll and ping interval. No secret is among them, and every caller reads \
                 the same values. They change only through Settings, and a client reads them \
                 again when it needs them.",
            )
        },
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<reads::ChatToolList>()),
            ..chat_route(
                "GET",
                "/chat/api/tools",
                "List the toolsets a thread may attach",
                "For a thread's tool picker: lmgw's built-in toolsets first (lmgw, docs, kb, \
                 image, audio, in that order), each with its tools, then the registered MCP \
                 servers and service agents by label, each with its connection's status and \
                 how many of its tools, as last listed, the caller may tick. Connects nothing; \
                 GET /chat/api/tools/{label} lists a server's tools. Under the caller's tool \
                 scope: the owner is listed every built-in toolset, one that offers nothing \
                 now with the reason (status unavailable), and every enabled server; a device \
                 key is listed what GET /v1/mcp/servers lists it — the toolsets that offer it \
                 tools and the enabled servers its scope reaches — and the self-admin \
                 toolset (lmgw) only while its admin tools are at full, since below that a \
                 device uses the threads that carry it and attaches it to none. A tool the \
                 owner switched off is never listed or counted.",
            )
        },
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<reads::ChatToolDetail>()),
            ..chat_route(
                "GET",
                "/chat/api/tools/{label}",
                "List one toolset's tools",
                "The tools a thread attaching `label` alone gets, under their exposed names \
                 (the spelling a thread's allowed_tools and require_approval store), the \
                 owner's switched-off tools and those outside the caller's tool scope left \
                 out — the same resolution a turn runs. Connects that one server when it is \
                 not up, but starts no service agent: one whose app is not running is a 502 \
                 mcp_list_tools_failed saying so, and a turn that attaches it starts it. A \
                 label the caller may not attach (unknown, disabled, outside its scope, or \
                 lmgw for a device below full) is a 404 not_found in the resolver's words; \
                 a server that cannot be listed a 502 mcp_list_tools_failed.",
            )
        },
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<reads::ChatKnowledgeBases>()),
            ..chat_route(
                "GET",
                "/chat/api/knowledge/bases",
                "List the knowledge bases a thread may search",
                "Every knowledge base with its file counts and whether its embedding model \
                 still resolves (without one a search is by keyword only), for a thread's \
                 Knowledge section and the composer's # picker. A thread's bases are its own \
                 choice, so a base's MCP visibility does not narrow the list. A device key \
                 reads the bases only with kb__search within its tool scope, the tool a \
                 thread's base is searched with: without it the answer is 403 \
                 tool_label_out_of_scope, naming the tool and no base.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            query: Some(|g| g.root_schema_for::<kb::SourceQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<reads::ChatKnowledgeSource>()),
            ..chat_route(
                "GET",
                "/chat/api/knowledge/files/{id}/text",
                "Read a knowledge file's extracted text",
                "The source viewer a citation opens: the file's extracted text (null until \
                 it has been ingested once), every chunk's byte range in it, and the chunk \
                 named by `chunk` as `highlight`; a citation whose chunk id a re-ingest \
                 renamed is found again by its stored span_start, span_end and sha, and \
                 `notice` says so. The file is described by what the viewer shows (name, \
                 kind, status, pages), without its ingest record; `original` says whether \
                 the caller may download the original. A device key reads it only with \
                 kb__search and kb__read within its tool scope and kb__read switched on by \
                 the owner (403 tool_label_out_of_scope otherwise, said before the file is \
                 looked up). 400 op_failed for a file that does not exist.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Binary(&["*/*"]),
            ..chat_route(
                "GET",
                "/chat/api/knowledge/files/{id}/original",
                "Download a knowledge file's original",
                "The uploaded bytes, with the media type they were sniffed as and \
                 Content-Disposition: attachment under the file's name (an ASCII fallback \
                 plus the RFC 5987 filename*). The owner's: a device key is refused \
                 (403 forbidden, said before the file is looked up), since the original \
                 carries more than any kb tool hands out; it reads the extracted text. 404 \
                 not_found for a file that does not exist or whose original is gone from \
                 disk.",
            )
        },
    ]
}
