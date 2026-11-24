//! The Documents lab's flat table (labs-store design §1.7, document-conversion
//! design WP12): one row per run, what was asked (the route, the output, the
//! pages, the files) and how the files came out, counted from the answer's
//! `summary`; the text itself is the run's output files. A group with no run
//! (a stopped ask) has no row.

use lmgw_api_types::documents_lab::DocumentsLabForm;
use lmgw_api_types::lab_store::LabGroupView;
use serde::Deserialize;

use super::csv::csv_line;
use super::export_csv_media::{file_fields, header as with_run_columns, run_fields};

/// The Documents lab's own columns, after the run's.
pub const DOCUMENTS_COLUMNS: [&str; 11] = [
    "route",
    "output",
    "pages",
    "input_files",
    "converted",
    "partial",
    "failed",
    "skipped",
    "files",
    "file_bytes",
    "artifact_ids",
];

pub fn header() -> String {
    with_run_columns(&DOCUMENTS_COLUMNS)
}

/// The answer's `summary`, when it has one (the native route's).
#[derive(Deserialize, Default)]
#[serde(default)]
struct Summary {
    converted: u32,
    partial: u32,
    failed: u32,
    skipped: u32,
}

#[derive(Deserialize)]
struct Said {
    summary: Option<Summary>,
}

/// The records of a Documents lab group: one per run.
pub fn rows(g: &LabGroupView) -> String {
    let form = serde_json::from_value::<DocumentsLabForm>(g.form.clone()).unwrap_or_default();
    let o = &form.settings.options;
    let output = serde_json::to_value(o.output)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let route = serde_json::to_value(form.settings.route)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let mut out = String::new();
    for r in &g.runs {
        let summary = r
            .response
            .as_deref()
            .and_then(|t| serde_json::from_str::<Said>(t).ok())
            .and_then(|s| s.summary);
        let count = |f: fn(&Summary) -> u32| summary.as_ref().map(|s| f(s).to_string());
        let mut fields = run_fields(g, r);
        fields.extend([
            route.clone(),
            output.clone(),
            o.pages.clone().unwrap_or_default(),
            form.files.len().to_string(),
            count(|s| s.converted).unwrap_or_default(),
            count(|s| s.partial).unwrap_or_default(),
            count(|s| s.failed).unwrap_or_default(),
            count(|s| s.skipped).unwrap_or_default(),
        ]);
        fields.extend(file_fields(r));
        out.push_str(&csv_line(&fields));
    }
    out
}
