//! `GET /chat/api/media/defaults` (chat-media-tools design §3.8,
//! `Cap::Chat`): the image and audio tools' default models at each level
//! the caller asks for — Settings → Chat's always, a folder's with
//! `?folder=`, a thread's and its folder's with `?thread=` — what they
//! resolve to for that caller now, why a stored one does not, and the
//! choices the pickers offer (each tool's `model` enum as the caller's key
//! sees it). A folder or thread the caller does not reach is the `404
//! not_found` of one that is not there (L3).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use super::super::chat::err_json;
use super::super::chat_caller::Caller;
use super::super::chat_extract::ChatQuery;
use super::super::chat_repo::ChatRepo;
use crate::state::SharedState;

/// The levels asked for.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct MediaDefaultsQuery {
    /// A folder: its own defaults, and what it inherits from Settings.
    #[serde(default)]
    pub folder: Option<i64>,
    /// A thread: its own, its folder's (when it is in one), and what it
    /// inherits. Wins over `folder`.
    #[serde(default)]
    pub thread: Option<i64>,
}

pub async fn defaults(
    State(state): State<SharedState>,
    caller: Caller,
    ChatQuery(q): ChatQuery<MediaDefaultsQuery>,
) -> Response {
    let missing = |what: &str| {
        err_json(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{what} not found"),
        )
    };
    let folder_of = |id: i64| folder_seen(&state, &caller, id);
    // A device's thread in a folder the device does not see still takes
    // the folder's defaults at its turn (`chat_turn::approval_floor`): it
    // resolves with them here too, and only the folder's own field is left
    // out of the answer.
    let mut hide_folder = false;
    let (folder, thread) = match (q.thread, q.folder) {
        (Some(id), _) => {
            let repo = ChatRepo::of(id);
            let Ok(Some(t)) = repo.thread_as(&state, &caller, id).await else {
                return missing("thread");
            };
            // Its folder as stored: a device's copy has none when the
            // device does not see it.
            let stored = repo.thread(&state, id).await.ok().flatten();
            let folder = match stored.and_then(|s| s.folder_id) {
                Some(fid) => {
                    let f = crate::store::get_chat_folder(&state.db, fid)
                        .await
                        .ok()
                        .flatten();
                    let snap = state.snapshot();
                    hide_folder = f.as_ref().is_some_and(|f| !caller.sees_folder(&snap, f));
                    f.map(|f| f.defaults.media_defaults.unwrap_or_default())
                }
                None => None,
            };
            (folder, Some(t.media_defaults))
        }
        (None, Some(id)) => match folder_of(id).await {
            Some(f) => (Some(f.defaults.media_defaults.unwrap_or_default()), None),
            None => return missing("folder"),
        },
        (None, None) => (None, None),
    };
    let key = caller.ctx().client_key;
    let mut view = crate::mcp::media::defaults::view(&state, key.as_deref(), folder, thread).await;
    if hide_folder {
        view.folder = None;
    }
    Json(view).into_response()
}

/// Folder `id` when the caller sees it.
async fn folder_seen(
    state: &SharedState,
    caller: &Caller,
    id: i64,
) -> Option<crate::store::ChatFolder> {
    let snap = state.snapshot();
    crate::store::get_chat_folder(&state.db, id)
        .await
        .ok()
        .flatten()
        .filter(|f| caller.sees_folder(&snap, f))
}
