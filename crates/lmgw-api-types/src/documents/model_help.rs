//! Model help for conversions (document-conversion design §3 tier 1, §7.5,
//! §7.7, §8): which model read a file's pages ([`ModelCalls`], in a file's
//! `stats.models`), and what Settings › Documents shows about the
//! page-reading model ([`PageReader`], in `doc_tools`' answer; the same
//! shape says what it shows about the picture-description model).

use serde::{Deserialize, Serialize};

/// The calls one model answered for a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelCalls {
    /// The model that answered, by its lmgw name: the chosen model (the
    /// page-reading, picture-description or speech-to-text one), or the
    /// fallback that answered in its place (what `x-lmgw-fallback` names
    /// on a chat request).
    pub alias: String,
    /// The chosen model the calls were made for, when another model
    /// answered them (the GPU hold with a fallback configured); absent when
    /// `alias` is the chosen model itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_for: Option<String>,
    /// Calls it was sent, the failed ones included.
    pub calls: u32,
    /// Pages whose text is its answer.
    pub parts_read: u32,
    /// Pictures whose description is its answer (the picture-description
    /// model's calls; one model can be both).
    #[serde(default)]
    pub pictures_described: u32,
    /// Audio and video files whose transcript is its answer (the
    /// speech-to-text model's call, one per file, WP7).
    #[serde(default)]
    pub files_transcribed: u32,
    /// The most calls the conversion kept in flight at once: the model's
    /// slots (§7.7).
    pub in_flight_bound: u32,
    /// Where `in_flight_bound` comes from.
    pub bound_from: BoundFrom,
}

/// Where the bound on a conversion's model calls in flight comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BoundFrom {
    /// The slots the local model's llama-server was started with (its
    /// row's parallel slots, or llama-server's own count when the row leaves
    /// it to the server).
    RowSlots,
    /// The tool pool's size: the model is not a local llama.cpp model whose
    /// slots lmgw knows, and its upstream has no concurrency setting.
    ToolPool,
    /// One call for the whole file: an audio or video file's transcription
    /// (WP7).
    OneCall,
}

/// How the page-reading model's answers are read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PageAnswer {
    /// Markdown, asked for with lmgw's page prompts.
    #[default]
    Markdown,
    /// DocTags, docling's page format, asked for with docling's own prompt
    /// (a granite-docling model).
    Doctags,
}

/// Where a model that is not one of lmgw's own local models runs: the
/// upstream lmgw sends its requests to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Outside {
    /// The upstream's name, as Models › Upstreams shows it.
    pub service: String,
    /// The host of its address (no path, no credentials).
    pub host: String,
}

/// The model that answers in the page-reading model's place whenever that
/// one cannot run: its configured fallback (the GPU hold, a benchmark's
/// lease, a card too full for it, a climb that cannot happen, an upstream
/// out of quota or unreachable).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PageFallback {
    /// Its lmgw name.
    pub alias: String,
    /// It is one of lmgw's own local models, on this computer.
    pub local: bool,
    /// Where pages are sent when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside: Option<Outside>,
    /// Whether it takes pictures, where lmgw knows: `false` is a fallback
    /// that cannot read a page, so pages keep the tools' text while it
    /// answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sees: Option<bool>,
}

/// Settings › Documents' "Read pages with a model" row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PageReader {
    /// The chosen model's lmgw name; empty: none chosen, and no conversion
    /// asks a model.
    pub alias: String,
    pub answer: PageAnswer,
    /// What is wrong with the choice, in plain words: the name no longer
    /// resolves, or the model says it cannot see pictures. Empty when
    /// nothing is.
    pub problems: Vec<String>,
    /// The model is one of lmgw's own local models: it runs on this
    /// computer and needs the graphics card, so the GPU hold pauses it.
    pub local: bool,
    /// Where pages are sent when it is not: the upstream that serves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside: Option<Outside>,
    /// The model that answers whenever the chosen one cannot run, as
    /// configured on the model (its fallback); `None`: no fallback, pages
    /// keep their text without a model then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<PageFallback>,
    /// Advice for a model that answers in DocTags: what its row still needs
    /// (llama-server's `--special`). Empty when nothing is known to be
    /// missing.
    pub hints: Vec<String>,
}
