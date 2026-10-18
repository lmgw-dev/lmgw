//! The Image and Audio labs' flat tables (labs-store design §1.7, §5): one
//! row per run, the form's few fields that say what was asked, and the
//! files a run made by count, size and id (the files themselves are the
//! JSONL export's zip). A group with no run (a stopped ask) has no row.

use lmgw_api_types::audio_lab::AudioLabForm;
use lmgw_api_types::image_lab::ImageLabForm;
use lmgw_api_types::lab_store::{LabGroupView, LabRunView};
use serde::Deserialize;

use super::csv::csv_line;

/// The run columns both tables start with.
const RUN_COLUMNS: [&str; 12] = [
    "group_id",
    "run_id",
    "created_at",
    "endpoint",
    "model_asked",
    "answered_by",
    "fallback_reason",
    "http_status",
    "error_code",
    "error_message",
    "latency_ms",
    "response_type",
];

/// The Image lab's own columns, after the run's.
pub const IMAGE_COLUMNS: [&str; 10] = [
    "prompt",
    "negative_prompt",
    "size",
    "n",
    "steps",
    "seed",
    "output_format",
    "images",
    "image_bytes",
    "artifact_ids",
];

/// The Audio lab's own columns, after the run's.
pub const AUDIO_COLUMNS: [&str; 8] = [
    "task",
    "input",
    "voice",
    "language",
    "text",
    "files",
    "file_bytes",
    "artifact_ids",
];

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

/// The header record of `own` after the run's columns.
fn header(own: &[&str]) -> String {
    let all: Vec<&str> = RUN_COLUMNS.iter().chain(own).copied().collect();
    csv_line(&all)
}

pub fn image_header() -> String {
    header(&IMAGE_COLUMNS)
}

pub fn audio_header() -> String {
    header(&AUDIO_COLUMNS)
}

/// The run's own columns.
fn run_fields(g: &LabGroupView, r: &LabRunView) -> Vec<String> {
    vec![
        g.id.to_string(),
        opt(r.id),
        r.created_at.clone(),
        r.endpoint.clone(),
        r.model_asked.clone(),
        r.answered_by.clone(),
        opt(r.fallback_reason.as_ref()),
        r.http_status.to_string(),
        opt(r.error_code.as_ref()),
        opt(r.error_message.as_ref()),
        r.latency_ms.to_string(),
        opt(r.response_type.as_ref()),
    ]
}

/// The files a run made: their count, their size and their ids.
fn file_fields(r: &LabRunView) -> [String; 3] {
    let ids: Vec<String> = r.artifacts.iter().map(|a| a.id.to_string()).collect();
    [
        r.artifacts.len().to_string(),
        r.artifacts.iter().map(|a| a.size).sum::<u64>().to_string(),
        ids.join(" "),
    ]
}

/// The records of an Image lab group: one per run.
pub fn image_rows(g: &LabGroupView) -> String {
    let form = serde_json::from_value::<ImageLabForm>(g.form.clone()).unwrap_or_default();
    let f = &form.form;
    let size = match (f.width.trim(), f.height.trim()) {
        ("", "") => String::new(),
        (w, h) => format!("{w}x{h}"),
    };
    let mut out = String::new();
    for r in &g.runs {
        let mut fields = run_fields(g, r);
        fields.extend([
            f.prompt.clone(),
            f.negative_prompt.clone(),
            size.clone(),
            f.n.clone(),
            f.steps.clone(),
            f.seed.clone(),
            f.output_format.clone(),
        ]);
        fields.extend(file_fields(r));
        out.push_str(&csv_line(&fields));
    }
    out
}

/// A transcription's or a task's `text`, when the answer has one.
fn text_of(r: &LabRunView) -> String {
    #[derive(Deserialize)]
    struct Said {
        text: Option<String>,
    }
    r.response
        .as_deref()
        .and_then(|t| serde_json::from_str::<Said>(t).ok())
        .and_then(|s| s.text)
        .unwrap_or_default()
}

/// The records of an Audio lab group: one per run.
pub fn audio_rows(g: &LabGroupView) -> String {
    let form = serde_json::from_value::<AudioLabForm>(g.form.clone()).unwrap_or_default();
    let speech = &form.settings.speech;
    let mut out = String::new();
    for r in &g.runs {
        let speaking = r.endpoint == "/v1/audio/speech";
        let mut fields = run_fields(g, r);
        fields.extend([
            form.task.clone(),
            if speaking {
                speech.input.clone()
            } else {
                String::new()
            },
            if speaking {
                speech.voice.clone()
            } else {
                String::new()
            },
            speech.language.clone(),
            text_of(r),
        ]);
        fields.extend(file_fields(r));
        out.push_str(&csv_line(&fields));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_header_is_the_runs_columns_then_the_labs() {
        let h = image_header();
        assert!(h.starts_with("group_id,run_id,created_at,endpoint,"), "{h}");
        assert!(h.ends_with("images,image_bytes,artifact_ids\r\n"), "{h}");
        let h = audio_header();
        assert!(h.ends_with("text,files,file_bytes,artifact_ids\r\n"), "{h}");
    }
}
