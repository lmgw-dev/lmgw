//! The docling-serve adapter (document-conversion design §7.4): a thin
//! client-shape adapter over the native conversion. It reads docling-serve's
//! request shapes ([`sources`]), maps docling's options onto the native ones
//! ([`options`], §7.4.2), runs the same conversion as
//! `POST /v1/documents/convert` (sync through [`batch::convert_each`], async
//! as a [`jobs`] job), and answers in docling-serve's shapes ([`answer`]:
//! one document in the body, several as `converted_docs.zip`, [`zip`]).
//! It adds no conversion logic of its own.
//!
//! lmgw's notes about the options travel in `x-lmgw-option-notes`, the
//! request's `convert_calls` record and one log line; the body changes only
//! where output was really lost (D16).
//!
//! **No call to a place the caller chose** (D21): callbacks are not called,
//! `put`/`s3`/`presigned_url` and the other storage targets and sources are
//! a named 400. The one exception is an `http` source, fetched ([`web`],
//! through `convert::source_fetch`) only while the owner lets lmgw fetch
//! web addresses and only from the sites the owner listed (ruling 9, D28);
//! otherwise it is a named 400. Its host is recorded either way.

use std::time::Instant;

use lmgw_api_types::documents::adapter_usage::OptionOutcome;
use lmgw_api_types::documents::convert::{ConvertedDocument, DocStatus, Pictures};
use lmgw_api_types::documents::InstallPlatform;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::batch::{self, Each};
use super::budget::OutputBudget;
use super::calls::{Noted, Record, Value as Rec};
use super::file::Input;
use super::{jobs, options as native_options};
use crate::state::SharedState;

pub mod answer;
mod formats;
pub mod lang;
pub mod options;
pub mod sources;
pub mod web;
pub mod zip;

/// The docling-serve release whose routes these are.
pub const COMPAT: &str = "1.38";

/// A request refused before any file is converted: its status, docling's
/// `detail`, and lmgw's name for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Refused {
    pub status: u16,
    pub code: &'static str,
    pub detail: Value,
}

impl Refused {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            detail: Value::String(message.into()),
        }
    }
}

/// docling's target: the answer in the body or as a zip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Inbody,
    Zip,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbody => "inbody",
            Self::Zip => "zip",
        }
    }
}

/// A docling request as read: its files, its options, its target, and the
/// notes about the request's shape (callbacks, the older source lists).
pub struct Parsed {
    pub route: &'static str,
    pub inputs: Vec<Input>,
    pub options: Map<String, Value>,
    pub target: Target,
    pub shape_notes: Vec<Noted>,
    /// Its `http` sources, each with its host and outcome for the record
    /// (`refused` until fetched); [`web::fetch_all`] fetches them.
    pub web: Vec<web::WebSource>,
    /// Files decided before the conversion: the `http` sources whose fetch
    /// failed, each a failed entry.
    pub decided: Vec<ConvertedDocument>,
}

/// How a docling job is answered, stored with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub md: bool,
    pub text: bool,
    pub missing_outputs: Vec<String>,
    pub target: Target,
    pub abort_on_error: bool,
    /// `x-lmgw-option-notes`, as the request was answered.
    pub notes: Option<String>,
}

/// A request ready to convert.
pub struct Prepared {
    pub inputs: Vec<Input>,
    pub skipped: Vec<ConvertedDocument>,
    pub plan: native_options::Plan,
    pub notes: Vec<lmgw_api_types::documents::convert::ConvertWarning>,
    pub spec: Spec,
    pub deadline: Option<std::time::Duration>,
    pub record: Record,
}

/// The record of a request, before or without its conversion.
pub fn record(p: &Parsed, noted: Vec<Noted>) -> Record {
    Record {
        route: p.route.to_string(),
        files: p.inputs.len() + p.decided.len() + p.web.iter().filter(|w| !w.done()).count(),
        target: Some(p.target.as_str().to_string()),
        options: noted,
        hosts: p.web.iter().map(|w| (w.host.clone(), w.outcome)).collect(),
    }
}

type Judged = (
    options::Mapped,
    native_options::Plan,
    Vec<lmgw_api_types::documents::convert::ConvertWarning>,
);

/// `p`'s options mapped and judged, or their 422 with the record.
fn judged(
    p: &Parsed,
    platform: InstallPlatform,
    options_at: &[&str],
    reader: &super::model_help::Reader,
) -> Result<Judged, (Refused, Record)> {
    let mut mapped = match options::map(&p.options) {
        Ok(m) => m,
        Err(bad) => {
            let mut r = Refused::new(422, "invalid_option", "");
            r.detail = bad.detail(options_at);
            return Err((r, record(p, Vec::new())));
        }
    };
    mapped.lower_for(reader.is_chosen());
    match native_options::plan_for(&mapped.native, platform, reader) {
        Ok((plan, notes)) => Ok((mapped, plan, notes)),
        Err(e) => {
            let noted = std::mem::take(&mut mapped.noted);
            Err((Refused::new(422, e.code, e.message), record(p, noted)))
        }
    }
}

/// Judge `p`'s options alone: what the route runs before it fetches web
/// sources, so a request that answers 422 downloads nothing.
pub fn judge(
    p: &Parsed,
    platform: InstallPlatform,
    options_at: &[&str],
    reader: &super::model_help::Reader,
) -> Result<(), (Refused, Record)> {
    judged(p, platform, options_at, reader).map(|_| ())
}

/// Map `p`'s options and judge them: the conversion's plan, or the refusal
/// with the record that says what was sent.
pub async fn prepare(
    p: Parsed,
    platform: InstallPlatform,
    options_at: &[&str],
    reader: &super::model_help::Reader,
) -> Result<Prepared, (Refused, Record)> {
    let (mut mapped, mut plan, native_notes) = judged(&p, platform, options_at, reader)?;
    mapped.settle(&native_notes);
    // One file in the body has no place for picture files: its markdown
    // links them as the zip would, and the note says where they are.
    if mapped.native.pictures == Pictures::Referenced
        && p.target == Target::Inbody
        && p.inputs.len() == 1
    {
        for n in mapped
            .noted
            .iter_mut()
            .filter(|n| n.name == "image_export_mode")
        {
            n.outcome = OptionOutcome::Mapped;
            n.detail = Some("referenced pictures are files of the zip target only".into());
        }
    }
    // Both forms asked: the conversion writes the text form from its own
    // block model, so it matches `output: text`.
    plan.also_text = mapped.md && mapped.text;
    let mut noted = p.shape_notes.clone();
    noted.append(&mut mapped.noted);
    if p.inputs.is_empty() && p.decided.is_empty() {
        return Err((
            Refused::new(
                422,
                "no_files",
                "the request carries no file: send one or more files parts, or a non-empty \
                 sources list",
            ),
            record(&p, noted),
        ));
    }
    let hosts: Vec<(String, &'static str)> =
        p.web.iter().map(|w| (w.host.clone(), w.outcome)).collect();
    let (inputs, mut skipped) = match &mapped.from_formats {
        Some(allowed) => formats::split(p.inputs, allowed).await,
        None => (p.inputs, Vec::new()),
    };
    // The sources whose fetch failed answer as failed files.
    skipped.extend(p.decided);
    let spec = Spec {
        md: mapped.md,
        text: mapped.text,
        missing_outputs: mapped.missing_outputs.clone(),
        target: p.target,
        abort_on_error: mapped.abort_on_error,
        notes: super::calls::header(&noted),
    };
    let record = Record {
        route: p.route.to_string(),
        files: inputs.len() + skipped.len(),
        target: Some(p.target.as_str().to_string()),
        options: noted,
        hosts,
    };
    if let Some(h) = &spec.notes {
        tracing::info!(route = p.route, "docling-serve request options: {h}");
    }
    Ok(Prepared {
        inputs,
        skipped,
        plan,
        notes: native_notes,
        spec,
        deadline: mapped.deadline,
        record,
    })
}

/// Convert `p` now: every file's entry in request order, and the time it
/// took.
pub async fn convert_now(state: &SharedState, p: Prepared) -> (Vec<ConvertedDocument>, f64) {
    let started = Instant::now();
    let facts = state.doc_tools.current().await;
    let each = Each {
        done: None,
        deadline: p.deadline,
        spool: None,
    };
    let (_, a) = Box::pin(batch::convert_each(
        p.inputs,
        &p.plan,
        &facts,
        p.notes,
        OutputBudget::request(),
        &each,
    ))
    .await;
    let mut docs = a.documents;
    docs.extend(p.skipped);
    docs.sort_by_key(|d| d.index);
    (docs, started.elapsed().as_secs_f64())
}

/// Start `p` as a job; its id.
pub async fn start_job(
    state: &SharedState,
    p: Prepared,
    key_id: Option<i64>,
    caller: super::pool::Caller,
    slot: Option<crate::policy::ConcurrencyGuard>,
) -> Result<String, crate::error::GatewayError> {
    let adapter = serde_json::to_string(&p.spec).ok();
    let abort_on_error = p.spec.abort_on_error;
    jobs::start(
        state,
        jobs::Start {
            key_id,
            origin: jobs::Origin::Docling,
            files: p.inputs,
            decided: p.skipped,
            plan: p.plan,
            notes: p.notes,
            deadline: p.deadline,
            adapter,
            abort_on_error,
            caller,
            slot,
        },
    )
    .await
}

/// A note about the request's shape (not one of docling's options).
pub fn shape_note(name: &str, outcome: OptionOutcome, detail: &str) -> Noted {
    Noted::new(name, Rec::Set, outcome).detail(detail)
}

/// What `abort_on_error` makes of a file read with parts left out: a failed
/// file, as docling aborts its conversion at the first error.
pub fn abort_partial(docs: &mut [ConvertedDocument]) {
    for d in docs.iter_mut().filter(|d| d.status == DocStatus::Partial) {
        let codes: Vec<String> = d
            .warnings
            .iter()
            .filter_map(|w| {
                serde_json::to_value(w.code)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
            })
            .collect();
        d.status = DocStatus::Failed;
        d.content = None;
        d.error = Some(lmgw_api_types::documents::convert::ConvertError {
            code: lmgw_api_types::documents::convert::ErrorCode::Unreadable,
            message: format!(
                "abort_on_error: parts of the file could not be read ({})",
                codes.join(", ")
            ),
            tool: None,
        });
    }
}

#[cfg(test)]
mod tests;
