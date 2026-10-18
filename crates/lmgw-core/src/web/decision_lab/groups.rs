//! `POST /decision-lab/api/groups` (labs-store design §2.6 step 1): an Ask
//! becomes a stored group with its plan and no runs yet. The form is checked
//! (unless an override is sent instead: a lab exists to see what the route
//! says, so the override's only check is "a JSON object"), its images and
//! the override's data URLs become the group's input artifacts, and the
//! stored form and override name them by id. An override's
//! `lab-artifact:<id>` (an opened group's image) names a kept artifact: the
//! new group gets a row of its own for that file, so nothing is uploaded
//! again and deleting the old group does not take it.

use std::collections::HashMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::decision_lab::{DecisionAsk, DecisionForm, ImageInput};
use lmgw_api_types::lab_store::{artifact_ref, artifact_ref_id, LabKind};

use super::super::chat::err_json;
use super::super::chat_extract::ChatJson;
use super::super::labs_store::groups::{no_group, one};
use super::super::labs_store::store_failed;
use super::images;
use crate::egress::openai_wire::SystemOneBody;
use crate::labs::artifacts::parse_data_url;
use crate::labs::record::{self, NewArtifact, NewGroup, RecordError};
use crate::state::SharedState;
use crate::store;

/// The lab's own 400: a form or an Ask that is no request.
pub(super) fn bad(message: impl Into<String>) -> Response {
    err_json(StatusCode::BAD_REQUEST, "bad_request", message)
}

/// The form as stored: its images as the group's input artifacts, by id.
fn stored_form(form: &DecisionForm, ids: &[i64]) -> Result<String, String> {
    let mut form = form.clone();
    form.images = form
        .images
        .iter()
        .zip(ids)
        .map(|(img, id)| ImageInput::Artifact {
            id: *id,
            name: img.name().to_string(),
        })
        .collect();
    serde_json::to_string(&form).map_err(|e| e.to_string())
}

pub async fn create(
    State(state): State<SharedState>,
    ChatJson(ask): ChatJson<DecisionAsk>,
) -> Response {
    let plan = match ask.plan() {
        Ok(p) => p,
        Err(e) => return bad(e),
    };
    match &ask.override_text {
        None => {
            if let Err(e) = ask.form.check() {
                return bad(e);
            }
        }
        Some(o) => {
            if let Err(e) = SystemOneBody::parse(o.as_bytes()) {
                return bad(format!("override: {e}"));
            }
        }
    }

    // The form's images first, then each data URL among the override's.
    let mut inputs = Vec::new();
    // The kept artifacts named so far, by id: the input row each became.
    let mut kept: HashMap<i64, usize> = HashMap::new();
    for img in &ask.form.images {
        match img {
            ImageInput::DataUrl { name, url } => match parse_data_url(url) {
                Some(d) => inputs.push(NewArtifact::Bytes {
                    name: name.clone(),
                    bytes: d.bytes.into(),
                    declared_mime: d.declared,
                    data_url_head: Some(d.head),
                }),
                None => {
                    return bad(format!("image '{name}': not a base64 data URL"));
                }
            },
            ImageInput::Artifact { id, .. } => {
                kept.entry(*id).or_insert(inputs.len());
                inputs.push(NewArtifact::Existing { id: *id });
            }
        }
    }
    let form_images = inputs.len();
    let elements = match &ask.override_text {
        Some(o) => match images::elements(o) {
            Ok(e) => e,
            Err(e) => return bad(format!("override: {e}")),
        },
        None => Vec::new(),
    };
    // A data URL becomes an artifact only where its JSON string is written
    // without escapes: the cell writes it back as a plain JSON string, so
    // only then are the bytes sent the bytes typed. Any other stays inline.
    // A `lab-artifact:<id>` written the same way names an artifact the store
    // keeps (an opened group's image): the new group gets a row of its own
    // naming that file and its head, as a re-opened form's image does (the
    // form's own row when the form names it too), and the stored override
    // names that row. Its cell sends the data URL the file first came as.
    let mut swapped = Vec::new();
    for (k, e) in elements.iter().enumerate() {
        let plain = ask
            .override_text
            .as_deref()
            .is_some_and(|o| o.get(e.start + 1..e.end - 1) == Some(e.value.as_str()));
        if !plain {
            continue;
        }
        if let Some(id) = artifact_ref_id(&e.value) {
            let at = match kept.get(&id) {
                Some(at) => *at,
                None => {
                    match store::lab_artifact(&state.db, id).await {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            return bad(format!(
                                "override: {} names no artifact the store keeps",
                                e.value
                            ))
                        }
                        Err(err) => return store_failed("reading the override's images", err),
                    }
                    kept.insert(id, inputs.len());
                    inputs.push(NewArtifact::Existing { id });
                    inputs.len() - 1
                }
            };
            swapped.push((e, at));
        } else if let Some(d) = parse_data_url(&e.value) {
            swapped.push((e, inputs.len()));
            inputs.push(NewArtifact::Bytes {
                name: format!("override image {}", k + 1),
                bytes: d.bytes.into(),
                declared_mime: d.declared,
                data_url_head: Some(d.head),
            });
        }
    }

    let form = ask.form.clone();
    let override_text = ask.override_text.clone();
    let recorded = record::group(
        &state.db,
        &state.data_dir,
        NewGroup {
            lab: LabKind::Decision,
            title: ask.form.title(),
            plan,
            inputs,
        },
        move |ids| {
            let stored = stored_form(&form, &ids[..form_images])?;
            let over = override_text.map(|o| {
                let swaps = swapped
                    .into_iter()
                    .map(|(e, i)| (e, artifact_ref(ids[i])))
                    .collect();
                images::splice(&o, swaps)
            });
            Ok((stored, over))
        },
    )
    .await;
    let id = match recorded {
        Ok((id, _)) => id,
        Err(RecordError::Refused(m)) => return bad(m),
        Err(RecordError::Taken(m) | RecordError::Failed(m)) => {
            return store_failed("storing the group", m)
        }
    };
    match one(&state, LabKind::Decision, id).await {
        Ok(Some(g)) => Json(g).into_response(),
        Ok(None) => no_group(LabKind::Decision, id),
        Err(r) => r,
    }
}
