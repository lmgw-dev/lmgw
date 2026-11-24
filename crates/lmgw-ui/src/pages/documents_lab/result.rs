//! A kept run read for the page (WP12): the route's answer, whose document
//! texts the lab store keeps as files (`lab-artifact:<id>` where the text
//! was), as one row per file with where each of its texts is. Read as JSON
//! values rather than the typed answer: the stored answer has refs where
//! the route had an array, and a field a later work package adds must not
//! make a kept run unreadable.

use lmgw_api_types::documents::convert::{ConvertStats, ConvertSummary, ConvertWarning, ToolUsed};
use lmgw_api_types::lab_store::{artifact_ref_id, LabArtifactRole, LabRunView};
use serde_json::Value;

/// Where one text of a file is.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Text {
    /// A file the lab store keeps: fetched when it is shown.
    Kept {
        href: String,
        name: String,
        size: u64,
    },
    /// In the answer itself (a run the store could not keep).
    Inline(String),
}

impl Text {
    pub fn size(&self) -> u64 {
        match self {
            Self::Kept { size, .. } => *size,
            Self::Inline(s) => s.len() as u64,
        }
    }
}

/// One file's outcome.
#[derive(Clone, Debug, PartialEq, Default)]
pub(super) struct FileResult {
    pub index: usize,
    pub name: String,
    /// `converted`, `partial`, `failed`, `skipped` (docling: `success`,
    /// `partial_success`, `failure`, `skipped`, said as the native words).
    pub status: String,
    pub format: Option<String>,
    pub parts: u32,
    pub stats: Option<ConvertStats>,
    pub warnings: Vec<ConvertWarning>,
    /// docling's `errors`, one line each.
    pub errors: Vec<String>,
    /// A failed file's code and message.
    pub error: Option<(String, String)>,
    /// The markdown (`markdown`) or the plain text.
    pub content: Option<Text>,
    pub markdown: bool,
    /// The plain text beside the markdown (docling's `text_content`).
    pub text: Option<Text>,
    /// `output: json`: the parts as JSON.
    pub structure: Option<Text>,
    /// The structure is docling's `json_content`, a document of docling's
    /// own shape: shown as JSON only, since the outline reads lmgw's parts.
    pub structure_is_docling: bool,
}

/// A run's answer as the page shows it.
#[derive(Clone, Debug, PartialEq, Default)]
pub(super) struct RunResult {
    pub files: Vec<FileResult>,
    /// The request's own notes (an option not built yet, not applicable).
    pub notes: Vec<ConvertWarning>,
    pub tools: Vec<ToolUsed>,
    pub summary: Option<ConvertSummary>,
    /// docling's `processing_time`, seconds.
    pub processing_time: Option<f64>,
    /// docling's zip, kept whole.
    pub zip: Option<Text>,
}

/// `v` as text: a kept file's ref resolved against the run's outputs, or
/// the string itself.
fn text_of(run: &LabRunView, v: &Value) -> Option<Text> {
    let s = v.as_str()?;
    if let Some(id) = artifact_ref_id(s) {
        let a = run.artifacts.iter().find(|a| a.id == id)?;
        return Some(Text::Kept {
            href: a.href.clone(),
            name: a.name.clone(),
            size: a.size,
        });
    }
    Some(Text::Inline(s.to_string()))
}

/// An array as inline JSON text (an unkept run's structure).
fn json_text(run: &LabRunView, v: &Value) -> Option<Text> {
    match v {
        Value::Array(_) | Value::Object(_) => Some(Text::Inline(v.to_string())),
        _ => text_of(run, v),
    }
}

fn typed<T: serde::de::DeserializeOwned>(v: &Value) -> Option<T> {
    serde_json::from_value(v.clone()).ok()
}

/// The native answer.
fn native(run: &LabRunView, v: &Value, markdown: bool) -> RunResult {
    let files = v["documents"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, d)| FileResult {
            index: d["index"].as_u64().map_or(i, |n| n as usize),
            name: d["name"].as_str().unwrap_or_default().to_string(),
            status: d["status"].as_str().unwrap_or_default().to_string(),
            format: d["format"].as_str().map(String::from),
            parts: d["parts"].as_u64().unwrap_or(0) as u32,
            stats: typed(&d["stats"]),
            warnings: typed(&d["warnings"]).unwrap_or_default(),
            errors: Vec::new(),
            error: d["error"].as_object().map(|e| {
                (
                    e.get("code")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    e.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )
            }),
            content: text_of(run, &d["content"]),
            markdown,
            text: text_of(run, &d["text"]),
            structure: json_text(run, &d["structure"]),
            structure_is_docling: false,
        })
        .collect();
    RunResult {
        files,
        notes: typed(&v["warnings"]).unwrap_or_default(),
        tools: typed(&v["tools"]).unwrap_or_default(),
        summary: typed(&v["summary"]),
        ..Default::default()
    }
}

/// docling's status as the native word.
fn docling_status(s: &str) -> String {
    match s {
        "success" => "converted",
        "partial_success" => "partial",
        "failure" => "failed",
        other => other,
    }
    .to_string()
}

/// docling's one-file answer.
fn docling(run: &LabRunView, v: &Value) -> RunResult {
    let d = &v["document"];
    let errors: Vec<String> = v["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            format!(
                "{}: {}",
                e["module_name"].as_str().unwrap_or_default(),
                e["error_message"].as_str().unwrap_or_default()
            )
        })
        .collect();
    let file = FileResult {
        index: 0,
        name: d["filename"].as_str().unwrap_or_default().to_string(),
        status: docling_status(v["status"].as_str().unwrap_or_default()),
        errors,
        content: text_of(run, &d["md_content"]),
        markdown: true,
        text: text_of(run, &d["text_content"]),
        structure: json_text(run, &d["json_content"]),
        structure_is_docling: true,
        ..Default::default()
    };
    // No document: a failure body (`{"detail"}`, `TaskFailureResult`).
    let files = if d.is_null() { Vec::new() } else { vec![file] };
    RunResult {
        files,
        processing_time: v["processing_time"].as_f64(),
        ..Default::default()
    }
}

/// docling's zip: the archive, then each entry as a file of its own.
fn zip(run: &LabRunView) -> RunResult {
    let outs: Vec<_> = run
        .artifacts
        .iter()
        .filter(|a| a.role == LabArtifactRole::Output)
        .collect();
    let Some((whole, entries)) = outs.split_first() else {
        return RunResult::default();
    };
    let files = entries
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let markdown = a.name.ends_with(".md");
            let t = Text::Kept {
                href: a.href.clone(),
                name: a.name.clone(),
                size: a.size,
            };
            FileResult {
                index: i,
                name: a.name.clone(),
                status: "converted".into(),
                markdown,
                content: Some(t),
                ..Default::default()
            }
        })
        .collect();
    RunResult {
        files,
        zip: Some(Text::Kept {
            href: whole.href.clone(),
            name: whole.name.clone(),
            size: whole.size,
        }),
        ..Default::default()
    }
}

/// `run` read: `docling` for the docling route's answer, `markdown` when the
/// request asked for markdown.
pub(super) fn read(run: &LabRunView, docling_route: bool, markdown: bool) -> RunResult {
    let zipped = run
        .response_type
        .as_deref()
        .is_some_and(|t| t.starts_with("application/zip"));
    if zipped {
        return zip(run);
    }
    let Some(v) = run
        .response
        .as_deref()
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
    else {
        return RunResult::default();
    };
    if docling_route {
        docling(run, &v)
    } else {
        native(run, &v, markdown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(response: &str, response_type: &str, artifacts: Value) -> LabRunView {
        serde_json::from_value(serde_json::json!({
            "id": 3, "group_id": 1, "cell": 0, "variant": "native",
            "endpoint": "/v1/documents/convert", "model_asked": "documents/convert",
            "answered_by": "documents/convert", "fallback_reason": null, "upstream_model": null,
            "http_status": 200, "error_code": null, "error_message": null,
            "request": "{}", "response": response, "response_type": response_type,
            "headers": {}, "input_tokens": null, "output_tokens": null, "latency_ms": 12,
            "created_at": "", "artifacts": artifacts
        }))
        .unwrap()
    }

    fn artifact(id: i64, name: &str) -> Value {
        serde_json::json!({"id": id, "run_id": 3, "role": "output", "ord": 0, "name": name,
            "mime": "text/plain", "size": 42, "sha256": "x", "data_url_head": null,
            "href": format!("/labs/api/artifacts/{id}"), "created_at": ""})
    }

    #[test]
    fn a_native_answer_reads_file_by_file_with_its_kept_texts() {
        let r = run(
            r#"{"documents":[
                {"index":0,"name":"example-a.pdf","status":"converted","format":"pdf","parts":2,
                 "content":"lab-artifact:9","structure":null,"warnings":[],"stats":{"parts_by_source":{"text_layer":2,"ocr":0,"vision":0,"native":0},"headings":[1,2,0,0,0,0],"tables":1,"table_cells":8,"pictures":0,"model_calls":0,"time_ms":40},"error":null},
                {"index":1,"name":"broken.pdf","status":"failed","format":null,"parts":0,
                 "content":null,"structure":null,"warnings":[],"stats":{"parts_by_source":{"text_layer":0,"ocr":0,"vision":0,"native":0},"headings":[0,0,0,0,0,0],"tables":0,"table_cells":0,"pictures":0,"model_calls":0,"time_ms":1},
                 "error":{"code":"unreadable","message":"bad xref","tool":null}}],
              "summary":{"files":2,"converted":1,"partial":0,"failed":1,"skipped":0},
              "tools":[{"id":"poppler","version":"26.08.0"}],
              "warnings":[{"code":"not_yet","message":"no ocr yet","tool":null,"parts":[],"option":"ocr"}]}"#,
            "application/json",
            serde_json::json!([artifact(9, "example-a.md")]),
        );
        let r = read(&r, false, true);
        assert_eq!(r.files.len(), 2);
        let a = &r.files[0];
        assert_eq!(a.status, "converted");
        assert_eq!(a.stats.as_ref().unwrap().tables, 1);
        assert_eq!(
            a.content,
            Some(Text::Kept {
                href: "/labs/api/artifacts/9".into(),
                name: "example-a.md".into(),
                size: 42
            })
        );
        assert_eq!(
            r.files[1].error,
            Some(("unreadable".into(), "bad xref".into()))
        );
        assert_eq!(r.notes[0].option.as_deref(), Some("ocr"));
        assert_eq!(r.tools[0].id, "poppler");
        assert_eq!(r.summary.unwrap().failed, 1);
    }

    #[test]
    fn an_unkept_answer_shows_its_inline_texts() {
        let r = run(
            r#"{"documents":[{"index":0,"name":"a.txt","status":"converted","content":null,"structure":[{"kind":"document","number":1,"blocks":[]}]}]}"#,
            "application/json",
            serde_json::json!([]),
        );
        let r = read(&r, false, false);
        assert!(matches!(&r.files[0].structure, Some(Text::Inline(s)) if s.starts_with('[')));
        assert_eq!(r.files[0].content, None);
    }

    #[test]
    fn docling_answers_read_as_one_file_or_the_zips_entries() {
        let r = run(
            r#"{"document":{"filename":"x.pdf","md_content":"lab-artifact:4","text_content":"plain","json_content":null},"status":"partial_success","errors":[{"module_name":"lmgw","error_message":"json not yet"}],"processing_time":0.5}"#,
            "application/json",
            serde_json::json!([artifact(4, "x.md")]),
        );
        let d = read(&r, true, true);
        assert_eq!(d.files[0].status, "partial");
        assert_eq!(d.files[0].text, Some(Text::Inline("plain".into())));
        assert_eq!(d.files[0].errors, ["lmgw: json not yet"]);
        assert_eq!(d.processing_time, Some(0.5));
        assert!(d.files[0].structure_is_docling);
        let j = run(
            r#"{"document":{"filename":"x.pdf","md_content":null,"json_content":{"schema_name":"DoclingDocument","body":{}}},"status":"success"}"#,
            "application/json",
            serde_json::json!([]),
        );
        let j = read(&j, true, true);
        assert!(j.files[0].structure.is_some());
        assert_eq!(
            super::super::viewer::forms(&j.files[0]),
            [super::super::viewer::Form::Json]
        );

        let z = run(
            "",
            "application/zip",
            serde_json::json!([
                artifact(1, "converted_docs.zip"),
                artifact(2, "a.md"),
                artifact(3, "b.md")
            ]),
        );
        let z = read(&z, true, true);
        assert_eq!(z.files.len(), 2);
        assert!(z.zip.is_some());
        assert!(z.files[1].markdown);
    }
}
