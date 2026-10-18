//! A lab's history (labs-store design §1.5, §1.6): its groups in pages,
//! newest first, one group, deleting one, clearing the lab, the usage read
//! and the orphans' removal. Every write answers the lab's usage after it,
//! which is what the results head shows next.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::lab_store::{LabGroupPage, LabGroupsQuery, LabKind};

use super::super::chat::err_json;
use super::super::chat_extract::{ChatPath, ChatQuery};
use super::{lab_of, store_failed};
use crate::labs::{self, views};
use crate::state::SharedState;
use crate::store::{self, LabGroupFilter, LabGroupScope};

/// The usage of `lab`, as an answer.
async fn usage_answer(state: &SharedState, lab: LabKind) -> Response {
    match labs::usage::usage(&state.db, &state.data_dir, lab).await {
        Ok(u) => Json(u).into_response(),
        Err(e) => store_failed("reading the lab's usage", e),
    }
}

/// `GET /labs/api/{lab}/groups`: one page, newest first, with the total and
/// how many older groups are left. No `limit` is every group left.
pub async fn list(
    State(state): State<SharedState>,
    ChatPath(lab): ChatPath<String>,
    ChatQuery(q): ChatQuery<LabGroupsQuery>,
) -> Response {
    let lab = match lab_of(&lab) {
        Ok(l) => l,
        Err(r) => return r,
    };
    // The history: a set run's items are listed by their run.
    let f = LabGroupFilter {
        model: q.model.as_deref().filter(|m| !m.is_empty()),
        scope: LabGroupScope::History,
        ..LabGroupFilter::lab(lab)
    };
    let read = async {
        let rows = store::lab_groups_newest(&state.db, &f, q.before, q.limit).await?;
        let total = store::count_lab_groups(&state.db, &f, None).await?;
        // An empty page (`limit=0`) leaves every group from the cursor on.
        let from = rows.last().map(|last| last.id).or(q.before);
        let older = store::count_lab_groups(&state.db, &f, from).await?;
        let mut groups = views::groups(&state.db, rows, &|a| views::api_href(a.id)).await?;
        views::mark_in_flight(&mut groups, &state.lab_cells);
        Ok::<_, crate::error::GatewayError>(LabGroupPage {
            groups,
            total,
            older,
        })
    };
    match read.await {
        Ok(page) => Json(page).into_response(),
        Err(e) => store_failed("reading the lab's history", e),
    }
}

/// `GET /labs/api/{lab}/groups/{id}`.
pub async fn get(
    State(state): State<SharedState>,
    ChatPath((lab, id)): ChatPath<(String, i64)>,
) -> Response {
    let lab = match lab_of(&lab) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match one(&state, lab, id).await {
        Ok(Some(g)) => Json(g).into_response(),
        Ok(None) => no_group(lab, id),
        Err(r) => r,
    }
}

/// Group `id` of `lab` as its view; `None` when there is none.
pub(crate) async fn one(
    state: &SharedState,
    lab: LabKind,
    id: i64,
) -> Result<Option<lmgw_api_types::lab_store::LabGroupView>, Response> {
    let row = store::lab_group(&state.db, lab, id)
        .await
        .map_err(|e| store_failed("reading the group", e))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let mut views = views::groups(&state.db, vec![row], &|a| views::api_href(a.id))
        .await
        .map_err(|e| store_failed("reading the group", e))?;
    views::mark_in_flight(&mut views, &state.lab_cells);
    Ok(views.pop())
}

/// The 404 for a group that is not there, or another lab's.
pub(crate) fn no_group(lab: LabKind, id: i64) -> Response {
    err_json(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("the {} lab has no group {id}", lab.as_str()),
    )
}

/// `POST /labs/api/{lab}/groups/{id}/delete`: the group, its runs, its
/// artifact rows, and the files no other row names. Answers the usage after.
/// A group in flight is the 409 `in_flight`: its run is on its way, and
/// deleting the group under it would leave the answer with nowhere to be
/// kept. Clear is not refused: it empties the lab, and a run that lands
/// after it is answered as not kept.
pub async fn delete(
    State(state): State<SharedState>,
    ChatPath((lab, id)): ChatPath<(String, i64)>,
) -> Response {
    let lab = match lab_of(&lab) {
        Ok(l) => l,
        Err(r) => return r,
    };
    // A set run's item goes with its run, not alone: its metrics count it.
    match store::lab_group(&state.db, lab, id).await {
        Ok(Some(g)) => {
            if let (Some(batch), Some(ix)) = (g.batch_id, g.item_ix) {
                return err_json(
                    StatusCode::CONFLICT,
                    "batch_item",
                    format!(
                        "group {id} is item {} of set run {batch}: delete the set run",
                        ix + 1
                    ),
                );
            }
        }
        Ok(None) => return no_group(lab, id),
        Err(e) => return store_failed("reading the group", e),
    }
    if state.lab_cells.live(id) {
        return err_json(
            StatusCode::CONFLICT,
            "in_flight",
            format!("group {id} is being answered: stop it, or delete it once its run is in"),
        );
    }
    match labs::delete::delete(&state.db, &state.data_dir, lab, Some(id)).await {
        Ok(0) => no_group(lab, id),
        Ok(_) => usage_answer(&state, lab).await,
        Err(e) => store_failed("deleting the group", e),
    }
}

/// `POST /labs/api/{lab}/clear`: every group and set run of the lab, as a
/// delete each; a set run's live job drops its calls in flight first.
/// Answers the usage after.
pub async fn clear(State(state): State<SharedState>, ChatPath(lab): ChatPath<String>) -> Response {
    let lab = match lab_of(&lab) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match store::lab_batches(&state.db, lab).await {
        Ok(batches) => labs::batch::abort_live(&state, &batches),
        Err(e) => return store_failed("reading the lab's set runs", e),
    }
    match labs::delete::delete(&state.db, &state.data_dir, lab, None).await {
        Ok(_) => usage_answer(&state, lab).await,
        Err(e) => store_failed("clearing the lab", e),
    }
}

/// `GET /labs/api/{lab}/usage`.
pub async fn usage(State(state): State<SharedState>, ChatPath(lab): ChatPath<String>) -> Response {
    match lab_of(&lab) {
        Ok(lab) => usage_answer(&state, lab).await,
        Err(r) => r,
    }
}

/// `POST /labs/api/{lab}/orphans/delete`: the artifacts directory's files no
/// row names. Answers the usage after.
pub async fn delete_orphans(
    State(state): State<SharedState>,
    ChatPath(lab): ChatPath<String>,
) -> Response {
    let lab = match lab_of(&lab) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match labs::artifacts::remove_orphans(&state.db, &state.data_dir).await {
        Ok(_) => usage_answer(&state, lab).await,
        Err(e) => store_failed("removing the orphan files", e),
    }
}
