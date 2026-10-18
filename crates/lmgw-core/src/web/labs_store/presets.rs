//! A lab's presets (labs-store design §3.2): `/labs/api/{lab}/presets`, the
//! list, one preset, create, update (whole) and delete. A preset's kind must
//! be its lab's (a question set is the Decision lab's, an image prompt the Image
//! lab's, audio settings the Audio lab's), its name unique among
//! the lab's presets of that kind, and its body what the kind's type checks
//! (a set's questions as a form's, its thresholds naming its questions).
//! An update names the `updated_at` it read and is a 409 `set_changed` when
//! the preset moved since; a question it changes loses its threshold's
//! measurement. Deleting a set leaves the set runs that ran it their
//! snapshot.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::lab_store::{
    LabKind, LabPresetBody, LabPresetList, LabPresetUpdate, LabPresetWrite, LabPresetsQuery,
};

use super::super::chat::err_json;
use super::super::chat_extract::{ChatJson, ChatPath, ChatQuery};
use super::{lab_of, store_failed};
use crate::labs::presets;
use crate::state::SharedState;
use crate::store::{self, LabPresetRow, PresetWrite};

fn no_preset(lab: LabKind, id: i64) -> Response {
    err_json(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("the {} lab has no preset {id}", lab.as_str()),
    )
}

fn answer(row: &LabPresetRow) -> Response {
    match presets::view(row) {
        Ok(v) => Json(v).into_response(),
        Err(e) => store_failed("reading the preset", e),
    }
}

async fn list_answer(state: &SharedState, lab: LabKind, q: &LabPresetsQuery) -> Response {
    let rows = match store::lab_presets(&state.db, lab, q.kind).await {
        Ok(r) => r,
        Err(e) => return store_failed("reading the presets", e),
    };
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        match presets::view(r) {
            Ok(v) => out.push(v),
            Err(e) => return store_failed(&format!("reading preset {}", r.id), e),
        }
    }
    Json(LabPresetList { presets: out }).into_response()
}

/// What makes `name` and `preset` no preset of `lab`: its kind is another
/// lab's, its name is blank, its body does not check. The body's text when
/// it is one.
fn checked(lab: LabKind, name: &str, preset: &LabPresetBody) -> Result<String, Response> {
    let bad = |m: String| err_json(StatusCode::BAD_REQUEST, "bad_request", m);
    let kind = preset.kind();
    if kind.lab() != lab {
        return Err(bad(format!(
            "a {} is the {} lab's preset, not the {} lab's",
            kind.as_str(),
            kind.lab().as_str(),
            lab.as_str()
        )));
    }
    if name.trim().is_empty() {
        return Err(bad("name: give the preset a name".into()));
    }
    preset.check().map_err(bad)?;
    presets::body_text(preset).map_err(|e| store_failed("writing the preset", e))
}

fn taken(lab: LabKind, name: &str, preset: &LabPresetBody) -> Response {
    err_json(
        StatusCode::CONFLICT,
        "name_taken",
        format!(
            "the {} lab has a {} named '{}' already",
            lab.as_str(),
            preset.kind().as_str(),
            name.trim()
        ),
    )
}

/// The 409 for an update of a preset that moved since it was read.
fn changed(lab: LabKind, id: i64, read_at: &str) -> Response {
    err_json(
        StatusCode::CONFLICT,
        "set_changed",
        format!(
            "preset {id} of the {} lab changed since it was read (at {read_at}): read it again \
             and redo the edit on it",
            lab.as_str()
        ),
    )
}

/// `GET /labs/api/{lab}/presets?kind=`: by name.
pub async fn list(
    State(state): State<SharedState>,
    ChatPath(lab): ChatPath<String>,
    ChatQuery(q): ChatQuery<LabPresetsQuery>,
) -> Response {
    match lab_of(&lab) {
        Ok(lab) => list_answer(&state, lab, &q).await,
        Err(r) => r,
    }
}

/// `POST /labs/api/{lab}/presets`.
pub async fn create(
    State(state): State<SharedState>,
    ChatPath(lab): ChatPath<String>,
    ChatJson(w): ChatJson<LabPresetWrite>,
) -> Response {
    let lab = match lab_of(&lab) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let body = match checked(lab, &w.name, &w.preset) {
        Ok(b) => b,
        Err(r) => return r,
    };
    match store::insert_lab_preset(&state.db, lab, w.preset.kind(), w.name.trim(), &body).await {
        Ok(PresetWrite::Written(row)) => answer(&row),
        Ok(PresetWrite::NameTaken) => taken(lab, &w.name, &w.preset),
        Ok(PresetWrite::Missing | PresetWrite::Changed) => {
            store_failed("writing the preset", "no row came back")
        }
        Err(e) => store_failed("writing the preset", e),
    }
}

/// `GET /labs/api/{lab}/presets/{id}`.
pub async fn get(
    State(state): State<SharedState>,
    ChatPath((lab, id)): ChatPath<(String, i64)>,
) -> Response {
    let lab = match lab_of(&lab) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match store::lab_preset(&state.db, Some(lab), id).await {
        Ok(Some(row)) => answer(&row),
        Ok(None) => no_preset(lab, id),
        Err(e) => store_failed("reading the preset", e),
    }
}

/// `POST /labs/api/{lab}/presets/{id}`: the preset rewritten whole, if it
/// is still the one read (`updated_at`). A question set's question that the
/// update changes loses its threshold's measurement (its value stays).
pub async fn update(
    State(state): State<SharedState>,
    ChatPath((lab, id)): ChatPath<(String, i64)>,
    ChatJson(w): ChatJson<LabPresetUpdate>,
) -> Response {
    let lab = match lab_of(&lab) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let before = match store::lab_preset(&state.db, Some(lab), id).await {
        Ok(Some(row)) => row,
        Ok(None) => return no_preset(lab, id),
        Err(e) => return store_failed("reading the preset", e),
    };
    if before.updated_at != w.updated_at {
        return changed(lab, id, &w.updated_at);
    }
    let mut preset = w.preset.clone();
    match (&mut preset, presets::body_of(&before.kind, &before.body)) {
        (LabPresetBody::QuestionSet(after), Ok(LabPresetBody::QuestionSet(was))) => {
            after.unmeasure_changed(&was);
        }
        // A body this build cannot read keeps no measurement to judge, nor
        // does one of another kind.
        (LabPresetBody::QuestionSet(after), _) => {
            after.unmeasure_changed(&Default::default());
        }
        // An image prompt and audio settings measure nothing.
        (LabPresetBody::ImagePrompt(_) | LabPresetBody::AudioSettings(_), _) => {}
    }
    let body = match checked(lab, &w.name, &preset) {
        Ok(b) => b,
        Err(r) => return r,
    };
    match store::update_lab_preset(
        &state.db,
        lab,
        id,
        &w.updated_at,
        preset.kind(),
        w.name.trim(),
        &body,
    )
    .await
    {
        Ok(PresetWrite::Written(row)) => answer(&row),
        Ok(PresetWrite::NameTaken) => taken(lab, &w.name, &preset),
        Ok(PresetWrite::Changed) => changed(lab, id, &w.updated_at),
        Ok(PresetWrite::Missing) => no_preset(lab, id),
        Err(e) => store_failed("writing the preset", e),
    }
}

/// `POST /labs/api/{lab}/presets/{id}/delete`: answers the lab's presets
/// after it.
pub async fn delete(
    State(state): State<SharedState>,
    ChatPath((lab, id)): ChatPath<(String, i64)>,
) -> Response {
    let lab = match lab_of(&lab) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match store::delete_lab_preset(&state.db, lab, id).await {
        Ok(true) => list_answer(&state, lab, &LabPresetsQuery::default()).await,
        Ok(false) => no_preset(lab, id),
        Err(e) => store_failed("deleting the preset", e),
    }
}
