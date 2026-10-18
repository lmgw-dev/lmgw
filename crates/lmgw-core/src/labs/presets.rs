//! Presets as the API reads and writes them (labs-store design §3.2, §5):
//! the stored body is the kind's own type as JSON (a `QuestionSet`, an
//! `ImageGenForm`, `AudioSettings`), the view carries it typed beside its
//! `kind`.

use lmgw_api_types::audio_lab::AudioSettings;
use lmgw_api_types::decision_lab::QuestionSet;
use lmgw_api_types::image_lab::ImageGenForm;
use lmgw_api_types::lab_store::{LabKind, LabPresetBody, LabPresetKind, LabPresetView};

use crate::store::LabPresetRow;

/// The body's text as stored: the kind's type alone.
pub fn body_text(body: &LabPresetBody) -> Result<String, String> {
    match body {
        LabPresetBody::QuestionSet(s) => serde_json::to_string(s),
        LabPresetBody::ImagePrompt(f) => serde_json::to_string(f),
        LabPresetBody::AudioSettings(a) => serde_json::to_string(a),
    }
    .map_err(|e| e.to_string())
}

/// A stored body read as its kind's type.
pub fn body_of(kind: &str, text: &str) -> Result<LabPresetBody, String> {
    match LabPresetKind::parse(kind) {
        Some(LabPresetKind::QuestionSet) => serde_json::from_str::<QuestionSet>(text)
            .map(LabPresetBody::QuestionSet)
            .map_err(|e| format!("the stored question set does not read: {e}")),
        Some(LabPresetKind::ImagePrompt) => serde_json::from_str::<ImageGenForm>(text)
            .map(|f| LabPresetBody::ImagePrompt(Box::new(f)))
            .map_err(|e| format!("the stored image prompt does not read: {e}")),
        Some(LabPresetKind::AudioSettings) => serde_json::from_str::<AudioSettings>(text)
            .map(|a| LabPresetBody::AudioSettings(Box::new(a)))
            .map_err(|e| format!("the stored audio settings do not read: {e}")),
        None => Err(format!("'{kind}' is no preset kind this build knows")),
    }
}

/// A stored preset as the API's view.
pub fn view(row: &LabPresetRow) -> Result<LabPresetView, String> {
    Ok(LabPresetView {
        id: row.id,
        lab: LabKind::parse(&row.lab).unwrap_or(LabKind::Decision),
        name: row.name.clone(),
        preset: body_of(&row.kind, &row.body)?,
        created_at: row.created_at.clone(),
        updated_at: row.updated_at.clone(),
    })
}

/// A stored preset's question set; `Err` when it is another kind or does
/// not read.
pub fn question_set(row: &LabPresetRow) -> Result<QuestionSet, String> {
    match body_of(&row.kind, &row.body)? {
        LabPresetBody::QuestionSet(s) => Ok(s),
        other => Err(format!(
            "preset {} is a {}, not a question set",
            row.id,
            other.kind().as_str()
        )),
    }
}
