//! `POST /audio-lab/api/groups` (labs-store design §5): a kept group of one
//! cell, made before its request is sent: the page's whole form, the model
//! and the route. The request itself then goes to the route's hand-off with
//! `?group=<id>` ([`super::record`]), which records it as the group's run,
//! so the hand-off's body stays the `/v1` route's own.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::audio_lab::AudioAsk;
use lmgw_api_types::lab_store::{LabCell, LabKind};

use super::super::labs_store::groups::one;
use super::extract::LabJson;
use super::fail;
use crate::labs::record::{self, NewGroup};
use crate::state::SharedState;

pub async fn create(State(state): State<SharedState>, LabJson(ask): LabJson<AudioAsk>) -> Response {
    let model = ask.model.trim().to_string();
    if model.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "pick a model first");
    }
    let form = ask.form.clone();
    let recorded = record::group(
        &state.db,
        &state.data_dir,
        NewGroup {
            lab: LabKind::Audio,
            title: ask.title(),
            plan: vec![LabCell {
                model,
                variant: ask.route.as_str().into(),
            }],
            inputs: Vec::new(),
        },
        move |_| {
            serde_json::to_string(&form)
                .map(|f| (f, None))
                .map_err(|e| e.to_string())
        },
    )
    .await;
    let id = match recorded {
        Ok((id, _)) => id,
        Err(e) => {
            tracing::warn!("audio lab: keeping the group failed: {e}");
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("keeping the group failed: {e}"),
            );
        }
    };
    match one(&state, LabKind::Audio, id).await {
        Ok(Some(g)) => Json(g).into_response(),
        Ok(None) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("group {id} was kept and is gone"),
        ),
        Err(r) => r,
    }
}
