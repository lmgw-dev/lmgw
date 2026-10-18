//! The lab store's shapes (labs-store design §1): what the Audio, Image and
//! Decision labs keep — groups, runs and artifacts — and the answers of the
//! shared `/labs/api/{lab}/*` routes that read, delete and export them.
//!
//! One click in a lab is a **group**: the form as the lab held it, the plan
//! of its cells and the override when one was in effect. Each dispatch to a
//! `/v1` route is a **run**, which keeps the request it sent and the answer it
//! got as text, never re-serialized. Media bytes are **artifacts**, files
//! named by their content hash, which a group (its inputs) or a run (its
//! outputs) names.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::systemone::Ordered;

/// What the Image and Audio labs keep beside these: a kept file a form
/// names, a multipart request's summary, a title from a text (§5).
pub mod media;
pub use media::{title_of, LabFileRef, LabMultipartField, LabMultipartFile, LabMultipartSummary};

/// The Image and Audio labs' files export: what a manifest line says.
pub mod files;
pub use files::{LabFileKind, LabFilesInput, LabFilesManifestLine, FILES_FORMAT, FILES_MANIFEST};

/// Presets: named form parts a lab keeps (§3.2), the Decision lab's
/// question sets first.
pub mod presets;
pub use presets::{
    LabPresetBody, LabPresetKind, LabPresetList, LabPresetUpdate, LabPresetView, LabPresetWrite,
    LabPresetsQuery,
};

/// The `format` marker of each line of a JSONL export.
pub const EXPORT_FORMAT: &str = "lmgw.lab.v1";

/// How a stored request names an artifact where it sent the bytes: an image
/// data URL becomes `lab-artifact:<id>`, a string where a string was.
pub const ARTIFACT_REF_PREFIX: &str = "lab-artifact:";

/// `lab-artifact:<id>`.
pub fn artifact_ref(id: i64) -> String {
    format!("{ARTIFACT_REF_PREFIX}{id}")
}

/// The artifact id a `lab-artifact:<id>` names, or `None` for any other
/// string.
pub fn artifact_ref_id(s: &str) -> Option<i64> {
    s.strip_prefix(ARTIFACT_REF_PREFIX)?.parse().ok()
}

/// Which lab a group belongs to; the `{lab}` of `/labs/api/{lab}/*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum LabKind {
    Decision,
    Image,
    Audio,
}

impl LabKind {
    pub const ALL: [Self; 3] = [Self::Decision, Self::Image, Self::Audio];

    /// The stored and routed spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Image => "image",
            Self::Audio => "audio",
        }
    }

    /// The lab named `s`, or `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// One cell of a group's plan: the model it asks and the variant of the
/// form it sends. The cells run in plan order, one run each.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabCell {
    /// The public name asked.
    pub model: String,
    /// The lab's name for what was sent: the Decision lab's `as_written`,
    /// `no_descriptions` or `reversed`.
    pub variant: String,
}

/// Whether an artifact is a group's input or a run's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum LabArtifactRole {
    /// Sent with the group's requests (an image a decision model reads),
    /// shared by its cells.
    Input,
    /// What a run produced (a generated image, a speech clip).
    Output,
}

impl LabArtifactRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

/// One artifact: media bytes kept as a file named by their sha256.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabArtifactView {
    pub id: i64,
    /// The run that produced it; `null` for a group's input.
    pub run_id: Option<i64>,
    pub role: LabArtifactRole,
    /// Its place among the group's inputs or the run's outputs.
    pub ord: i64,
    /// The upload's name, or the one lmgw gave an output.
    pub name: String,
    /// The media type, sniffed from the bytes.
    pub mime: String,
    /// The file's size in bytes.
    pub size: u64,
    /// The sha256 of the bytes, which names the file.
    pub sha256: String,
    /// An input that came as a data URL: its head as typed, everything
    /// before the comma (`data:image/x-receipt;name=a.png;base64`). A request
    /// sends it as `<head>,<base64 of the bytes>`; `null` for bytes that came
    /// as bytes.
    pub data_url_head: Option<String>,
    /// Where the bytes are: `/labs/api/artifacts/{id}` from the API, the
    /// file's path inside the zip in an export.
    pub href: String,
    pub created_at: String,
}

/// One dispatch to one `/v1` route and what came back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabRunView {
    /// `null` for a run the store could not keep: the cell's answer is still
    /// here, and `store_error` beside it says why.
    pub id: Option<i64>,
    pub group_id: i64,
    /// Its index in the group's plan.
    pub cell: u32,
    pub variant: String,
    /// The route dispatched to, `/v1/systemone`.
    pub endpoint: String,
    pub model_asked: String,
    /// Which model answered: `x-lmgw-fallback` when a fallback did, else the
    /// model asked.
    pub answered_by: String,
    /// `x-lmgw-fallback-reason`: why a fallback answered (`hold`, ...).
    pub fallback_reason: Option<String>,
    /// The answer's own `model`.
    pub upstream_model: Option<String>,
    /// The route's HTTP status, whatever it was.
    pub http_status: u16,
    /// A failure's code: the envelope's `lmgw_code`, else its `code` when
    /// that is a string, else its `type` (llama.cpp's numeric `code` only
    /// repeats the status), else the numeric `code`.
    pub error_code: Option<String>,
    /// A failure's message.
    pub error_message: Option<String>,
    /// The request as sent, as text, except that each image data URL is
    /// `lab-artifact:<id>`.
    pub request: String,
    /// The answer as received, as text; `null` when it was no text and is an
    /// output artifact instead.
    pub response: Option<String>,
    /// The answer's `Content-Type`.
    pub response_type: Option<String>,
    /// The `x-lmgw-*` headers the route stamped on its answer.
    pub headers: BTreeMap<String, String>,
    /// The answer's input tokens, when it reported them.
    pub input_tokens: Option<u64>,
    /// The answer's output tokens, when it reported them.
    pub output_tokens: Option<u64>,
    /// How long the dispatch took in the gateway, answer read included.
    pub latency_ms: u64,
    pub created_at: String,
    /// What the run produced, in order.
    pub artifacts: Vec<LabArtifactView>,
}

/// What makes a group an item of a set run (labs-store design §3.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabBatchItem {
    /// The set run.
    pub batch_id: i64,
    /// The item's place in the run's file, from 0.
    pub item_ix: u64,
    /// The item's own key, from the mapping's `key` column; `null` without
    /// one.
    #[serde(default)]
    pub key: Option<String>,
    /// The expected answers, per question id, as the store keeps a label:
    /// the option's name (`choice`), the level's index (`score`), `yes` or
    /// `no` (`noul`). Empty when the item is not labelled.
    pub expected: Ordered<String>,
}

/// What one click in a lab produced, or one item of a set run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabGroupView {
    pub id: i64,
    pub lab: LabKind,
    /// Derived once from the form: the state's first line, a prompt.
    pub title: String,
    /// The lab's form as the lab held it (the Decision lab's
    /// `DecisionForm`), with its media as the group's input artifacts.
    pub form: Value,
    /// The cells, in run order. A group with fewer runs than cells was
    /// stopped.
    pub plan: Vec<LabCell>,
    /// The request override, as typed (its image data URLs as
    /// `lab-artifact:<id>`), when one was in effect.
    #[serde(rename = "override")]
    pub override_text: Option<String>,
    pub created_at: String,
    /// The runs so far, in cell order.
    pub runs: Vec<LabRunView>,
    /// The group's inputs, in order.
    pub artifacts: Vec<LabArtifactView>,
    /// The set run it is an item of; `null` for a group of the history.
    #[serde(default)]
    pub item: Option<LabBatchItem>,
    /// A cell of it is being answered now: its request is out and its run
    /// not in yet. A group with no run that is not in flight was stopped.
    /// Deleting a group in flight is the 409 `in_flight`.
    #[serde(default)]
    pub in_flight: bool,
}

/// The query of `GET /labs/api/{lab}/groups`: one page of the history,
/// newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabGroupsQuery {
    /// Only groups older than this one (its id): the next page.
    pub before: Option<i64>,
    /// How many groups the page holds. Absent, the page is every group
    /// left: the server sets no maximum.
    pub limit: Option<u32>,
    /// Only groups whose plan asks this model.
    pub model: Option<String>,
}

/// One page of a lab's history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabGroupPage {
    /// Newest first.
    pub groups: Vec<LabGroupView>,
    /// Every group the filter matches, this page's included.
    pub total: u64,
    /// The groups the filter matches that are older than this page's last.
    pub older: u64,
}

/// What a lab keeps, and the artifacts directory's files no row names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabUsage {
    pub lab: LabKind,
    /// Every group: the history's and the set runs' items.
    pub groups: u64,
    /// The set runs; their items are among `groups`.
    #[serde(default)]
    pub batches: u64,
    pub runs: u64,
    /// Artifact rows: each input of a group and output of a run.
    pub artifacts: u64,
    /// The distinct files those rows name.
    pub artifact_files: u64,
    /// The stored text: each group's title, form, plan, override and
    /// expected answers, each run's request, response and headers, each set
    /// run's set and mapping, in bytes.
    pub text_bytes: u64,
    /// The size of the distinct files the lab's rows name.
    pub artifact_bytes: u64,
    /// Files in the artifacts directory that no row of any lab names (left
    /// by a crash between a write and its commit); removable with
    /// `POST /labs/api/{lab}/orphans/delete`.
    pub orphan_files: u64,
    pub orphan_bytes: u64,
}

/// The file format of an export.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum LabExportFormat {
    /// Lossless: one `lmgw.lab.v1` line per group; a zip with the files when
    /// the scope has artifacts.
    #[default]
    Jsonl,
    /// The lab's flat table.
    Csv,
    /// The Image and Audio labs' files under readable names: one group's
    /// one file as it is, else a zip of the files and a `manifest.jsonl`
    /// ([`LabFilesManifestLine`]). The Decision lab makes no files (a 400).
    Files,
}

/// The query of `GET /labs/api/{lab}/export`: the format and the scope. No
/// scope is the whole lab, its history and its set runs' items; `group` one
/// group; `batch` one set run's items; `model`, `from` and `to` a filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabExportQuery {
    pub format: LabExportFormat,
    /// One group, by id.
    pub group: Option<i64>,
    /// One set run's items, in item order.
    pub batch: Option<i64>,
    /// Only groups whose plan asks this model.
    pub model: Option<String>,
    /// Only groups created at or after this time (RFC 3339).
    pub from: Option<String>,
    /// Only groups created before this time (RFC 3339).
    pub to: Option<String>,
}

/// One line of a JSONL export: a group with its runs and artifacts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabExportLine {
    /// Always `lmgw.lab.v1`.
    pub format: String,
    pub lab: LabKind,
    pub group: LabGroupView,
}

/// What a lab's cell answers: its run, whatever the route said, and why it
/// was not kept when the store failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabCellResult {
    pub run: LabRunView,
    /// Why the run was not kept (a full disk, a lock held too long). The
    /// answer is the route's all the same.
    pub store_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lab_is_named_by_its_spelling_and_nothing_else_is() {
        for k in LabKind::ALL {
            assert_eq!(LabKind::parse(k.as_str()), Some(k));
            assert_eq!(
                serde_json::to_string(&k).unwrap(),
                format!("\"{}\"", k.as_str())
            );
        }
        assert_eq!(LabKind::parse("Decision"), None);
        assert_eq!(LabKind::parse("chat"), None);
    }

    #[test]
    fn an_artifact_ref_names_its_id() {
        assert_eq!(artifact_ref(12), "lab-artifact:12");
        assert_eq!(artifact_ref_id("lab-artifact:12"), Some(12));
        assert_eq!(artifact_ref_id("lab-artifact:x"), None);
        assert_eq!(artifact_ref_id("data:image/png;base64,AA=="), None);
    }

    #[test]
    fn the_override_is_named_override_on_the_wire() {
        let g = LabGroupView {
            id: 1,
            lab: LabKind::Decision,
            title: "t".into(),
            form: Value::Null,
            plan: vec![],
            override_text: Some("{}".into()),
            created_at: String::new(),
            runs: vec![],
            artifacts: vec![],
            item: None,
            in_flight: false,
        };
        let v = serde_json::to_value(&g).unwrap();
        assert_eq!(v["override"], "{}");
    }
}
