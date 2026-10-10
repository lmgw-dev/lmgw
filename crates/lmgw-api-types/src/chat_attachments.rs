//! The Chat API's attachment routes: the upload's query, a PDF's mode and a
//! re-transcription. An attachment's own metadata is
//! [`AttachmentMeta`](crate::chat_threads::AttachmentMeta).
//!
//! The gateway reads and builds these types itself, and the API document is
//! generated from them.

use serde::{Deserialize, Serialize};

/// `POST /chat/api/threads/{id}/attachments`'s query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UploadQuery {
    /// The file's name, shown on its chip and kept in an export. The kind
    /// is sniffed from the bytes, never taken from the name or a content
    /// type. Blank is stored as `untitled`.
    pub name: String,
}

/// `POST /chat/api/attachments/{id}/mode`'s body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModeRequest {
    /// `text` sends the PDF's extracted text, `images` its pages as
    /// images.
    #[cfg_attr(feature = "schema", schemars(extend("enum" = ["text", "images"])))]
    pub mode: String,
}

/// The answer to a mode change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModeSet {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The attachment.
    pub id: i64,
    /// The mode now set, `text` or `images`.
    pub mode: String,
}

/// The answer to a new transcription.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Transcribed {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The attachment.
    pub id: i64,
    /// The attachment's `meta` now, the transcript's alias and token
    /// estimate in place of the earlier `transcript_error`. The same object
    /// as `AttachmentMeta::meta`.
    #[cfg_attr(
        feature = "schema",
        schemars(with = "crate::chat_threads::AttachmentFacts")
    )]
    pub meta: serde_json::Value,
}
