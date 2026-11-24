//! docling-serve's answers built from the native entries
//! (document-conversion design §7.4.3, D16): one file with the `inbody`
//! target is docling's `ConvertDocumentResponse`; several files, or the
//! `zip` target, are `converted_docs.zip`; an async task's state is
//! `TaskStatusResponse`, and a task that failed as a whole answers
//! `TaskFailureResult`.
//!
//! The body changes only where output was really lost: `status` is
//! `partial_success` and `errors` names it for parts lmgw could not read
//! and for outputs it does not write yet (`json`, `html`, …); everything
//! else lmgw has to say is in the headers.

use lmgw_api_types::documents::convert::{ConvertedDocument, DocStatus, ErrorCode, WarningCode};
use lmgw_api_types::documents::docling::{
    ConversionStatus, ConvertDocumentResponse, ErrorItem, ExportDocumentResponse,
    PublicFailureInfo, TaskFailureResult, TaskProcessingMeta, TaskStatusResponse, TaskType,
};
use lmgw_api_types::documents::jobs::{JobFileState, JobState};
use serde_json::Map;

use super::zip::{self, LAYOUT};
use super::{abort_partial, Spec, Target};
use crate::extract::convert::jobs::answer::Loaded;

/// What a conversion answers on the docling routes.
pub enum Answer {
    Json(Box<ConvertDocumentResponse>),
    /// The archive, and the counts for `x-lmgw-documents`.
    Zip(Vec<u8>),
    /// No file was exported into the zip: docling's task failure.
    Failure(TaskFailureResult),
}

/// `x-lmgw-documents`: the files' outcomes counted.
pub fn counts(docs: &[ConvertedDocument]) -> String {
    let n = |s: DocStatus| docs.iter().filter(|d| d.status == s).count();
    format!(
        "converted={}, partial={}, failed={}, skipped={}",
        n(DocStatus::Converted),
        n(DocStatus::Partial),
        n(DocStatus::Failed),
        n(DocStatus::Skipped)
    )
}

fn item(component: &str, message: String, category: &str, page: Option<u32>) -> ErrorItem {
    ErrorItem {
        component_type: component.to_string(),
        module_name: "lmgw".to_string(),
        error_message: message,
        category: category.to_string(),
        page_no: page,
    }
}

/// The warnings that mean text was or may have been lost (parts not read;
/// an image drawn blank, whose text a scan's page then does not show the
/// page-reading model or text recognition).
fn lost(code: WarningCode) -> bool {
    matches!(
        code,
        WarningCode::ToolMissing
            | WarningCode::NoReader
            | WarningCode::PartFailed
            | WarningCode::ModelHeld
            | WarningCode::ModelCannotSee
            | WarningCode::ModelNotAllowed
            | WarningCode::ImageNotDecoded
    )
}

/// docling's `FailureCategory` for a failed file's error. A file refused
/// by one of lmgw's named bounds is `policy`, as docling files a document
/// past its own limits (`max_num_pages`, `max_file_size`): the output's
/// (`output_too_large`) and a tool's memory limit past the wide one
/// (`memory_limit`) alike; the file is docling's `failure` either way.
/// `backend_failure` stays for a file the tools could not read.
fn category(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::DeadlineExceeded | ErrorCode::SourceTimeout => "timeout",
        ErrorCode::OutputTooLarge
        | ErrorCode::MemoryLimit
        | ErrorCode::SourceRefused
        | ErrorCode::SourceTooLarge
        | ErrorCode::ModelNotAllowed => "policy",
        // The page-reading model was the file's only reader: paused, or its
        // calls failed (docling's word for a model that did not answer).
        ErrorCode::ModelHeld | ErrorCode::ModelFailed => "inference_failure",
        ErrorCode::Interrupted => "internal",
        _ => "backend_failure",
    }
}

/// One file's docling status and errors.
pub fn status_and_errors(spec: &Spec, d: &ConvertedDocument) -> (ConversionStatus, Vec<ErrorItem>) {
    let mut errors = Vec::new();
    let mut status = match d.status {
        DocStatus::Converted => ConversionStatus::Success,
        DocStatus::Partial => ConversionStatus::PartialSuccess,
        DocStatus::Failed => ConversionStatus::Failure,
        DocStatus::Skipped => ConversionStatus::Skipped,
    };
    if let Some(e) = &d.error {
        errors.push(item(
            "document_backend",
            e.message.clone(),
            category(e.code),
            None,
        ));
    }
    for w in d.warnings.iter().filter(|w| lost(w.code)) {
        let page = (w.parts.len() == 1).then(|| w.parts[0]);
        errors.push(item("pipeline", w.message.clone(), "backend_failure", page));
    }
    if matches!(
        status,
        ConversionStatus::Success | ConversionStatus::PartialSuccess
    ) {
        for f in &spec.missing_outputs {
            errors.push(item(
                "user_input",
                format!(
                    "to_formats {}: lmgw does not write this output yet",
                    crate::extract::convert::quoted::quoted(f)
                ),
                "policy",
                None,
            ));
        }
        if !spec.missing_outputs.is_empty() {
            status = ConversionStatus::PartialSuccess;
        }
    }
    (status, errors)
}

/// One file in the body.
pub fn document(spec: &Spec, d: &ConvertedDocument, seconds: f64) -> ConvertDocumentResponse {
    let (status, errors) = status_and_errors(spec, d);
    let mut doc = ExportDocumentResponse {
        filename: d.name.clone(),
        md_content: None,
        json_content: None,
        html_content: None,
        text_content: None,
        doctags_content: None,
        doclang_content: None,
    };
    for (ext, text) in zip::outputs(spec, d) {
        match ext {
            "md" => doc.md_content = Some(text),
            _ => doc.text_content = Some(text),
        }
    }
    ConvertDocumentResponse {
        document: doc,
        status,
        errors,
        processing_time: seconds,
        timings: Map::new(),
        confidence: None,
    }
}

/// The answer for `docs` (every file's entry, in request order).
pub fn render(spec: &Spec, mut docs: Vec<ConvertedDocument>, seconds: f64) -> (Answer, String) {
    if spec.abort_on_error {
        abort_partial(&mut docs);
    }
    let mut counted = counts(&docs);
    if docs.len() == 1 && spec.target == Target::Inbody {
        return (
            Answer::Json(Box::new(document(spec, &docs[0], seconds))),
            counted,
        );
    }
    let zip::Entries {
        files: entries,
        artifacts,
        renamed,
        shortened,
    } = zip::entries(spec, &docs, LAYOUT);
    if renamed > 0 {
        counted.push_str(&format!(", entry_names_renamed={renamed}"));
    }
    if shortened > 0 {
        counted.push_str(&format!(
            ", entry_names_shortened={shortened} (stems cut to {} bytes)",
            zip::MAX_STEM_BYTES
        ));
    }
    if entries.is_empty() {
        let codes: Vec<String> = docs
            .iter()
            .map(|d| {
                d.error
                    .as_ref()
                    .and_then(|e| serde_json::to_value(e.code).ok())
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| "skipped".into())
            })
            .collect();
        return (
            Answer::Failure(failure(
                format!(
                    "No documents were exported: none of the {} file(s) was converted ({})",
                    docs.len(),
                    codes.join(", ")
                ),
                false,
            )),
            counted,
        );
    }
    match zip::write_with(&entries, &artifacts) {
        Ok(bytes) => (Answer::Zip(bytes), counted),
        Err(e) => (
            Answer::Failure(failure(
                format!("writing converted_docs.zip failed: {e}"),
                true,
            )),
            counted,
        ),
    }
}

/// [`render`] on a blocking thread: the zip, the pictures' base64 decoded
/// into it and the JSON answer are as large as the answer, so they are
/// never made on the async runtime.
pub async fn render_off(
    spec: &Spec,
    docs: Vec<ConvertedDocument>,
    seconds: f64,
) -> (Answer, String) {
    let spec = spec.clone();
    match tokio::task::spawn_blocking(move || render(&spec, docs, seconds)).await {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("writing a docling answer stopped: {e}");
            (
                Answer::Failure(failure(
                    "writing the answer stopped unexpectedly".to_string(),
                    true,
                )),
                String::new(),
            )
        }
    }
}

/// docling's `TaskFailureResult`.
pub fn failure(message: String, retryable: bool) -> TaskFailureResult {
    TaskFailureResult {
        kind: "TaskFailureResult".to_string(),
        failure: info(message, retryable),
    }
}

fn info(message: String, retryable: bool) -> PublicFailureInfo {
    PublicFailureInfo {
        category: if retryable { "internal" } else { "unknown" }.to_string(),
        message,
        retryable,
        phase: "execution".to_string(),
        details: Map::new(),
    }
}

/// A job's state as docling's `TaskStatusResponse`.
pub fn task_status(l: &Loaded) -> TaskStatusResponse {
    let state = l.state();
    let files: Vec<JobFileState> = l
        .files
        .iter()
        .map(|f| JobFileState::parse(&f.state))
        .collect();
    let n = |s: JobFileState| files.iter().filter(|f| **f == s).count() as u32;
    let processed = files
        .iter()
        .filter(|f| **f != JobFileState::Pending)
        .count() as u32;
    let task_status = match state {
        JobState::Running if processed == 0 => ConversionStatus::Pending,
        JobState::Running => ConversionStatus::Started,
        JobState::Succeeded => ConversionStatus::Success,
        JobState::Failed | JobState::Cancelled | JobState::Interrupted => ConversionStatus::Failure,
    };
    let failed = matches!(
        state,
        JobState::Failed | JobState::Cancelled | JobState::Interrupted
    );
    let message = l.row.error.clone();
    TaskStatusResponse {
        task_id: l.row.id.clone(),
        task_type: TaskType::Convert,
        task_status,
        task_position: None,
        task_meta: Some(TaskProcessingMeta {
            num_docs: files.len() as u32,
            num_processed: processed,
            num_succeeded: n(JobFileState::Converted),
            num_partially_succeeded: n(JobFileState::Partial),
            num_failed: n(JobFileState::Failed)
                + n(JobFileState::Cancelled)
                + n(JobFileState::Interrupted),
        }),
        error_message: failed.then(|| message.clone()).flatten(),
        failure: failed.then(|| {
            info(
                message.unwrap_or_else(|| "the task failed".into()),
                state == JobState::Interrupted,
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::documents::convert::{ConvertError, ConvertWarning};

    fn spec(target: Target, missing: &[&str]) -> Spec {
        Spec {
            md: true,
            text: false,
            missing_outputs: missing.iter().map(|s| s.to_string()).collect(),
            target,
            abort_on_error: false,
            notes: None,
        }
    }

    fn doc(status: DocStatus) -> ConvertedDocument {
        ConvertedDocument {
            index: 0,
            name: "example.pdf".into(),
            status,
            format: Some("pdf".into()),
            parts: 2,
            content: (status != DocStatus::Failed).then(|| "Page one".into()),
            structure: None,
            pictures: Vec::new(),
            text: None,
            warnings: Vec::new(),
            stats: Default::default(),
            error: (status == DocStatus::Failed).then(|| ConvertError {
                code: ErrorCode::ToolMissing,
                message: "install poppler-utils to read PDFs".into(),
                tool: Some("poppler".into()),
            }),
        }
    }

    #[test]
    fn one_file_in_the_body_has_docling_fields_and_nulls_for_the_rest() {
        let (a, counted) = render(
            &spec(Target::Inbody, &[]),
            vec![doc(DocStatus::Converted)],
            0.5,
        );
        let Answer::Json(r) = a else {
            panic!("not json")
        };
        let v = serde_json::to_value(&*r).unwrap();
        assert_eq!(v["document"]["md_content"], "Page one");
        assert!(v["document"]["json_content"].is_null() && v["document"]["text_content"].is_null());
        assert_eq!(v["status"], "success");
        assert_eq!(v["errors"], serde_json::json!([]));
        assert_eq!(v["timings"], serde_json::json!({}));
        assert!(v.get("confidence").is_some());
        assert_eq!(counted, "converted=1, partial=0, failed=0, skipped=0");
    }

    #[test]
    fn lost_parts_and_missing_outputs_are_partial_success_with_errors() {
        let mut d = doc(DocStatus::Partial);
        d.warnings.push(ConvertWarning {
            code: WarningCode::NoReader,
            message: "page 2 has no text layer".into(),
            tool: None,
            parts: vec![2],
            option: None,
        });
        let r = document(&spec(Target::Inbody, &["json"]), &d, 0.1);
        assert_eq!(r.status, ConversionStatus::PartialSuccess);
        assert_eq!(r.errors.len(), 2);
        assert_eq!(r.errors[0].page_no, Some(2));
        assert!(r.errors[1].error_message.contains("to_formats json"));
        let r = document(&spec(Target::Inbody, &[]), &doc(DocStatus::Failed), 0.1);
        assert_eq!(r.status, ConversionStatus::Failure);
        assert_eq!(r.errors[0].category, "backend_failure");
    }

    /// S6: a file the page-reading model was to read answers with the
    /// model's reason in docling's categories.
    #[test]
    fn a_file_failed_by_the_model_has_the_model_s_category() {
        for (code, category) in [
            (ErrorCode::ModelHeld, "inference_failure"),
            (ErrorCode::ModelFailed, "inference_failure"),
            (ErrorCode::ModelNotAllowed, "policy"),
        ] {
            let mut d = doc(DocStatus::Failed);
            d.error = Some(ConvertError {
                code,
                message: "page 1 would have been read by the page-reading model".into(),
                tool: None,
            });
            let r = document(&spec(Target::Inbody, &[]), &d, 0.1);
            assert_eq!(r.status, ConversionStatus::Failure);
            assert_eq!(r.errors[0].category, category, "{code:?}");
        }
        // A page a blind fallback could not read is a lost part.
        let mut d = doc(DocStatus::Partial);
        d.warnings.push(ConvertWarning {
            code: WarningCode::ModelCannotSee,
            message: "page 2 would have been read by the page-reading model".into(),
            tool: None,
            parts: vec![2],
            option: None,
        });
        let r = document(&spec(Target::Inbody, &[]), &d, 0.1);
        assert_eq!(r.errors.len(), 1);
        assert_eq!(r.errors[0].page_no, Some(2));
    }

    /// Review 2 of the poppler limit, nit: the file's named bounds are both
    /// docling's `policy` on a `failure`; an image drawn blank is listed on
    /// its page and leaves the file's status as it is.
    #[test]
    fn named_bounds_are_policy_and_a_blank_image_is_listed() {
        for code in [ErrorCode::OutputTooLarge, ErrorCode::MemoryLimit] {
            let mut d = doc(DocStatus::Failed);
            d.error = Some(ConvertError {
                code,
                message: "a bound".into(),
                tool: None,
            });
            let r = document(&spec(Target::Inbody, &[]), &d, 0.1);
            assert_eq!(r.status, ConversionStatus::Failure);
            assert_eq!(r.errors[0].category, "policy", "{code:?}");
        }
        let mut d = doc(DocStatus::Converted);
        d.warnings.push(ConvertWarning {
            code: WarningCode::ImageNotDecoded,
            message: "pdftoppm could not decode a JPEG 2000 image on page 3".into(),
            tool: Some("poppler".into()),
            parts: vec![3],
            option: None,
        });
        let r = document(&spec(Target::Inbody, &[]), &d, 0.1);
        assert_eq!(r.status, ConversionStatus::Success);
        assert_eq!(
            (
                r.errors.len(),
                r.errors[0].page_no,
                r.errors[0].category.as_str()
            ),
            (1, Some(3), "backend_failure")
        );
    }

    #[test]
    fn several_files_with_nothing_converted_are_a_task_failure() {
        let mut b = doc(DocStatus::Failed);
        b.index = 1;
        let (a, _) = render(
            &spec(Target::Zip, &[]),
            vec![doc(DocStatus::Failed), b],
            0.1,
        );
        let Answer::Failure(f) = a else {
            panic!("not a failure")
        };
        assert!(f.failure.message.contains("tool_missing, tool_missing"));
    }
}
