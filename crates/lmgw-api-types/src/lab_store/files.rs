//! The Image and Audio labs' files export (`format=files`): the files the
//! runs made, under readable names, and the manifest that traces each one
//! back to the run that made it. One group's one file is answered as it is;
//! anything else is a zip of the files and [`FILES_MANIFEST`], one
//! [`LabFilesManifestLine`] per file.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::LabKind;

/// The `format` marker of each manifest line.
pub const FILES_FORMAT: &str = "lmgw.lab.files.v1";

/// The manifest's path inside a files zip.
pub const FILES_MANIFEST: &str = "manifest.jsonl";

/// What a file of the export is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum LabFileKind {
    /// A run's output as the store keeps it: a picture, a speech file, a
    /// task's audio.
    Output,
    /// A run's streamed or raw PCM speech, kept as it arrived and written as
    /// a WAV with the PCM format the form held (the gateway's
    /// `x-lmgw-sample-rate` wins over its rate).
    Wav,
    /// A transcription's or a task's text answer, as UTF-8 text.
    Transcript,
}

/// One input of the group a file's run read: an upload to transcribe, an
/// edit's picture. Named here, not carried in the zip: it is the owner's own
/// file, and the JSONL export carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabFilesInput {
    /// The input artifact.
    pub artifact_id: i64,
    /// The upload's name.
    pub name: String,
    pub mime: String,
    pub sha256: String,
}

/// One line of a files zip's manifest: a file, and the run it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabFilesManifestLine {
    /// Always `lmgw.lab.files.v1`.
    pub format: String,
    /// The file's path inside the zip:
    /// `<group id>-<model>-<voice or task>-<first words>.<ext>`.
    pub file: String,
    pub kind: LabFileKind,
    pub lab: LabKind,
    pub group_id: i64,
    /// The run; `null` for a run the store could not keep.
    pub run_id: Option<i64>,
    /// When the run was answered.
    pub created_at: String,
    /// The `/v1` route the run asked.
    pub endpoint: String,
    /// The model asked.
    pub model: String,
    /// Which model answered: a fallback's alias when one did.
    pub answered_by: String,
    /// The Audio lab's task (`tts`, `asr`, ...), the Image lab's
    /// `generate` or `edit`.
    pub task: String,
    /// The voice a speech run asked; `null` for anything else.
    pub voice: Option<String>,
    /// The text the run was given: what was spoken, the prompt; `null`
    /// when it was given none.
    pub text: Option<String>,
    /// The request as sent: its JSON, or for an upload the multipart
    /// summary (`LabMultipartSummary`); a request that is no JSON as its
    /// text.
    pub parameters: Value,
    /// The file's media type.
    pub mime: String,
    /// The file's size in the zip, in bytes; for a missing file, the kept
    /// artifact's.
    pub size: u64,
    /// The kept artifact the file is (or, for a WAV, was made from); `null`
    /// for a transcript.
    pub artifact_id: Option<i64>,
    /// That artifact's sha256, which names its file in the data dir.
    pub sha256: Option<String>,
    /// The artifact's file was not in the data dir when the export was
    /// written: the zip does not carry it, and `MISSING.txt` names it.
    #[serde(default)]
    pub missing: bool,
    /// The group's inputs the run read, which the zip does not carry.
    #[serde(default)]
    pub inputs: Vec<LabFilesInput>,
}
