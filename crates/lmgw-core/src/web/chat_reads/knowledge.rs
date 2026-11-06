//! The knowledge bases on the Chat plane (chat-plane design §2.3, D7, D8,
//! D13–D15): `GET /chat/api/knowledge/bases`, the bases a thread may search,
//! and the source viewer a citation opens, `GET
//! /chat/api/knowledge/files/{id}/text` and `GET
//! /chat/api/knowledge/files/{id}/original`.
//!
//! **A device reads the owner's bases as the `kb` toolset its key's tool
//! scope bounds** (client-apps design L5, review W2-4):
//! - the list needs `kb__search` within its scope — what a thread's base is
//!   searched with. With it a device sees every base, as the owner's Chat
//!   does: a thread's selection ignores a base's `mcp_visible` switch
//!   (chat-complete design §9.4), and a device may already name any base
//!   there. The owner's switch on `kb__search` is not read, as a write of a
//!   base and a turn's retrieval do not read it (D13);
//! - a file's text needs `kb__read` too, the tool that reads a whole file,
//!   and that tool switched on: a device that cannot call it reads no whole
//!   file here either (D8). Its answer is [`ChatKnowledgeSource`], without
//!   the ingest record the Knowledge page's carries (D15);
//! - the original is not a device's yet: its bytes (a PDF's images, a
//!   document's metadata and revisions) are more than any `kb` tool hands
//!   out (D14).
//!
//! Every refusal is a `403` naming the tool and never a base, said before
//! anything is read, so a file id cannot be probed: `tool_label_out_of_scope`
//! as a write of a base gets it, and `forbidden` for the original.
//!
//! The owner is never refused, and reads what `/api/knowledge/files/{id}/*`
//! answers, through the same handlers (the text in the Chat's shape).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::chat_reads::{ChatKnowledgeBase, ChatKnowledgeBases, ChatKnowledgeSource};
use lmgw_api_types::knowledge::SourceQuery;

use super::super::chat::err_json;
use super::super::chat_caller::Caller;
use super::super::chat_extract::{ChatPath, ChatQuery};
use super::super::chat_tool_write::{CODE, KB_SEARCH};
use crate::knowledge::ops::KbView;
use crate::state::SharedState;

/// The tool a knowledge base's file is read with, whole (`mcp::kb`).
const KB_READ: &str = "kb__read";

/// `GET /chat/api/knowledge/bases` (module doc).
pub(in crate::web) async fn bases(State(state): State<SharedState>, caller: Caller) -> Response {
    if let Err(refused) = reach(&state, &caller, &[KB_SEARCH]).await {
        return refused;
    }
    match crate::knowledge::ops::list(&state).await {
        Ok(views) => Json(ChatKnowledgeBases {
            bases: views.into_iter().map(base).collect(),
        })
        .into_response(),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("reading the knowledge bases failed: {e}"),
        ),
    }
}

/// `GET /chat/api/knowledge/files/{id}/text` (module doc).
pub(in crate::web) async fn file_text(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
    ChatQuery(q): ChatQuery<SourceQuery>,
) -> Response {
    if let Err(refused) = reach(&state, &caller, &[KB_SEARCH, KB_READ]).await {
        return refused;
    }
    if caller.is_device() && state.snapshot().tool_disabled(KB_READ) {
        return err_json(
            StatusCode::FORBIDDEN,
            CODE,
            format!(
                "a knowledge base's file is read whole as {KB_READ}, which the owner switched off"
            ),
        );
    }
    let original = !caller.is_device();
    match super::super::api_knowledge::source(&state, id, q).await {
        Ok(s) => Json(ChatKnowledgeSource::of(s, original)).into_response(),
        Err(e) => super::super::api::ops_result(Err(e)),
    }
}

/// `GET /chat/api/knowledge/files/{id}/original` (module doc).
pub(in crate::web) async fn file_original(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
) -> Response {
    if caller.is_device() {
        return err_json(
            StatusCode::FORBIDDEN,
            "forbidden",
            "a knowledge base's original file is the owner's to download; a device reads its \
             extracted text",
        );
    }
    super::super::api_knowledge::original(State(state), Path(id)).await
}

/// `Ok` when every one of `tools` is within `caller`'s tool scope (the
/// owner's always is); the refusal naming the first that is not.
async fn reach(state: &SharedState, caller: &Caller, tools: &[&str]) -> Result<(), Response> {
    if !caller.is_device() {
        return Ok(());
    }
    let scope = caller.scope(state).await;
    match tools.iter().find(|t| !scope.admits(t)) {
        None => Ok(()),
        Some(t) => Err(err_json(
            StatusCode::FORBIDDEN,
            CODE,
            format!("a knowledge base is read as {}", scope.refusal(t)),
        )),
    }
}

/// One base as the Chat shows it.
fn base(v: KbView) -> ChatKnowledgeBase {
    ChatKnowledgeBase {
        id: v.kb.id,
        name: v.kb.name,
        files: v.counts.files,
        ready: v.counts.ready,
        embed_resolvable: v.embed_resolvable,
    }
}
