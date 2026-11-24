//! Document conversion (document-conversion design): the capability table,
//! the detection of the optional tools, and what Settings › Documents reads.
//! A dispatcher only; each concern is a child module.
//!
//! - [`caps`]: the one table of handlers, formats and tools, and
//!   [`caps::resolve`] (§4.1, §4.2).
//! - [`tools`]: the in-memory snapshot of every tool's probe, and when it is
//!   taken again (§4.3, §4.4).
//! - `probe`: one tool's lookup and version run; `watch`: the files whose
//!   change makes a probe stale; `winpath`: the Windows `PATH`.
//! - `platform`: which install hints this system gets.
//! - [`report`]: the page's [`DocCaps`] answer, rendered from the two.
//!
//! The conversion (`POST /v1/documents/convert`, §7.1, §7.6; the route's
//! HTTP half is `server/documents.rs`):
//! - [`options`]: the request's options, judged into a plan;
//! - [`batch`]: every file side by side, the answer and its status, the
//!   request's output bound (`budget`);
//! - [`file`]: one file, resolved through [`caps::resolve`] and read;
//! - `pages`, `shape`: the `pages` option, and the markdown's counts and
//!   forms (plain text, tables as lines, compact tables);
//! - [`doc`]: a file's structure, parts of blocks, the one intermediate
//!   markdown, text and `json` ([`doc_json`]) are written from;
//! - [`pdf_layout`]: a PDF's structure from poppler's geometry;
//! - [`layout_model`]: docling's layout model on ONNX Runtime, where it is
//!   chosen: a PDF page's regions, which the layout pass then builds from.
//! - [`ocr`]: text recognition (tesseract) of scanned pages and pictures;
//! - [`reader`]: lmgw's reader executable, the child that parses web pages,
//!   e-books, e-mail, legacy Excel workbooks and image files (WP4); the
//!   gateway's own part of a page and a workbook is [`html`] and [`xls`].
//! - [`pictures`]: a file's pictures found in reading order and written as
//!   placeholders, embedded or referenced (WP10).
//! - [`libreoffice`], [`pandoc`], [`media`]: old Word and PowerPoint files
//!   through LibreOffice, RTF, the text markups and Word files with
//!   equations through pandoc, audio and video through the speech-to-text
//!   model (a video's sound taken out by ffmpeg) (WP7);
//! - [`own`]: Chat attachments and knowledge bases reading what their own
//!   sniffer refuses through the same table (WP7).
//!
//! What every step runs on:
//! - [`run`]: one external tool run, shared with `extract/pdf.rs`;
//! - [`pool`]: the gateway-wide tool pool (§7.7), sized by `pool_size`.
//!
//! Async jobs and the docling-serve adapter (WP5):
//! - [`jobs`]: one job per async request, its files' results on disk, its
//!   end of life;
//! - [`docling_serve`]: docling-serve's request shapes mapped onto the
//!   native options, its answers built from the native answer;
//! - [`calls`]: what each conversion request sent, recorded beside its
//!   request-log row, and the per-key summary (§7.8).
//! - [`source_fetch`]: the adapter's `http` sources fetched, behind the
//!   owner's setting and allowed sites (WP5b, ruling 9, D28).
//!
//! Model help, tier 1 (WP6):
//! - [`model_help`]: the page-reading model, which pages go to it, the
//!   request's model-call pool, and the PDF stage that asks it;
//! - [`doctags`]: a docling model's page format read into blocks.

pub mod batch;
pub mod budget;
pub mod callers;
pub mod calls;
pub mod caps;
pub mod confine;
pub mod describe;
pub mod doc;
pub mod doc_json;
pub mod docling_serve;
pub mod doctags;
pub mod file;
pub mod html;
pub mod jobs;
mod key_check;
pub mod layout_model;
pub mod libreoffice;
pub mod media;
pub mod model_help;
pub mod ocr;
pub mod options;
pub mod own;
mod pages;
pub mod pandoc;
pub mod pdf_layout;
pub mod pictures;
mod platform;
pub mod pool;
mod pool_size;
mod probe;
pub mod quoted;
pub mod reader;
pub mod report;
pub mod run;
pub mod sent;
mod shape;
pub mod source_fetch;
pub mod tools;
mod watch;
mod winpath;
pub mod xls;

pub use probe::TOOL_PROBE_TIMEOUT;

use lmgw_api_types::documents::DocCaps;

use crate::state::SharedState;

/// `doc_tools`, the op and `lmgw__doc_tools`: the last check, with the
/// tools whose binary appeared, went away or changed since probed again.
pub async fn doc_tools(state: &SharedState) -> Result<DocCaps, String> {
    let facts = state.doc_tools.current().await;
    let mut caps = report::doc_caps(&facts, report::platform(state.deployment().is_container()));
    caps.web = web_sources(state);
    caps.models = layout_model::status::rows(state).await;
    caps.page_reader = model_help::settings::facts(state).await;
    model_help::settings::apply(&mut caps);
    caps.picture_describer = describe::settings::facts(state).await;
    describe::settings::apply(&mut caps);
    media::page_note(&mut caps, &state.snapshot().settings.documents.asr_alias);
    caps.speech = media::settings::facts(state).await;
    caps.job_bounds = jobs::quota::facts(state).await;
    Ok(caps)
}

/// The web-source settings as in force.
fn web_sources(state: &SharedState) -> lmgw_api_types::documents::WebSources {
    let s = state.snapshot();
    lmgw_api_types::documents::WebSources {
        fetch: s.settings.documents.fetch_sources,
        hosts: s.settings.documents.source_hosts.clone(),
        pool: source_fetch::slots::facts(s.settings.max_body_mb),
    }
}

/// `doc_tools_check`: Settings' "Check again", every tool probed now.
pub async fn doc_tools_check(state: &SharedState) -> Result<DocCaps, String> {
    let facts = state.doc_tools.check().await;
    let mut caps = report::doc_caps(&facts, report::platform(state.deployment().is_container()));
    caps.web = web_sources(state);
    caps.models = layout_model::status::rows(state).await;
    caps.page_reader = model_help::settings::facts(state).await;
    model_help::settings::apply(&mut caps);
    caps.picture_describer = describe::settings::facts(state).await;
    describe::settings::apply(&mut caps);
    media::page_note(&mut caps, &state.snapshot().settings.documents.asr_alias);
    caps.speech = media::settings::facts(state).await;
    caps.job_bounds = jobs::quota::facts(state).await;
    Ok(caps)
}

/// `doc_model_add`, the op and `lmgw__doc_model_add`: a model file queued
/// through the download queue ([`layout_model::status::add`]).
pub async fn doc_model_add(
    state: &SharedState,
    id: &str,
) -> Result<lmgw_api_types::documents::models::DocModelAddResult, String> {
    layout_model::status::add(state, id).await
}
