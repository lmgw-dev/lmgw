//! Settings › Documents (document-conversion design §4.4, §8): what this
//! computer can turn into text, the optional tools lmgw found with their
//! versions, and the missing packages with what installing each would give.
//!
//! `POST /api/op/doc_tools` and `lmgw__doc_tools` answer [`DocCaps`] from the
//! last probe (the tools whose binary appeared, went away or changed since
//! are probed again first); `POST /api/op/doc_tools_check` probes every tool
//! again. Every row comes from the one capability table
//! (`lmgw_core::extract::convert::caps`), the same one conversions resolve
//! through, so the page and a conversion cannot disagree.
//!
//! `label`, `said`, `gives` and the install hints are plain words for the
//! person; the ids, handler names, paths and licences are the expert view's.
//!
//! [`convert`] is `POST /v1/documents/convert`'s request and answer,
//! [`structure`] its `output: "json"` form, [`pictures`] its files' pictures, [`jobs`] its async jobs, [`docling`] the
//! docling-serve-compatible routes' shapes, and [`adapter_usage`] "Who calls the
//! docling route", and [`models`] the model files the page lists.

use serde::{Deserialize, Serialize};

pub mod adapter_usage;
pub mod bounds;
pub mod convert;
pub mod docling;
pub mod jobs;
pub mod model_help;
pub mod models;
pub mod pictures;
pub mod structure;

/// What this computer can convert, and what installing a package would add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocCaps {
    /// Every optional tool lmgw knows, found or not, in the table's order.
    pub tools: Vec<ToolRow>,
    /// The kinds of file, one row each, as the page lists them.
    pub formats: Vec<FormatRow>,
    /// The tools that are not installed, each with how to install it here
    /// and what it would give.
    pub missing: Vec<MissingRow>,
    /// When every tool was last probed (at start, or by `doc_tools_check`):
    /// unix seconds. A tool probed again on its own since carries its own
    /// [`ToolRow::checked_at`].
    pub checked_at: i64,
    /// What the install hints are written for: this computer's system, or
    /// the server image.
    pub platform: InstallPlatform,
    /// The tool pool every conversion step shares: its size and what it was
    /// derived from (the expert view's "Parallel tool runs").
    pub pool: PoolFacts,
    /// Whether the docling adapter fetches web addresses, and from where
    /// (the page's "Web addresses").
    #[serde(default)]
    pub web: WebSources,
    /// The model files lmgw can use for converting, found or not
    /// ([`models`]).
    #[serde(default)]
    pub models: Vec<models::DocModelRow>,
    /// The page-reading model ("Read pages with a model").
    #[serde(default)]
    pub page_reader: model_help::PageReader,
    /// The picture-description model ("Describe pictures with"), in the
    /// same shape (its `answer` and `hints` stay empty).
    #[serde(default)]
    pub picture_describer: model_help::PageReader,
    /// Whether lmgw's document tools run confined here (Landlock), or why
    /// not (performance round review S5); each tool's row says when it runs
    /// without confinement for a reason of its own
    /// ([`ToolRow::not_confined`]).
    #[serde(default)]
    pub confinement: Confinement,
    /// The speech-to-text model audio and videos are transcribed with
    /// ("Transcribe audio with", WP7), in the page-reading row's shape: who
    /// answers, where it runs and who answers in its place; its `answer`,
    /// `hints` and the fallback's `sees` say nothing about audio.
    #[serde(default)]
    pub speech: model_help::PageReader,
    /// The memory answers in flight hold, gateway-wide (security round A).
    #[serde(default)]
    pub answers: bounds::AnswerFacts,
    /// What async jobs may store and queue (security round A).
    #[serde(default)]
    pub job_bounds: bounds::JobBounds,
}

/// The docling adapter's `http` sources (document-conversion design ruling
/// 9, D28): the settings `documents.fetch_sources` and
/// `documents.source_hosts`, as in force.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WebSources {
    /// "Let callers send web addresses". Off: a web source is a named 400.
    pub fetch: bool,
    /// "Allowed sites": host names or addresses, each with an optional
    /// `:port` (without one: port 80 for `http`, 443 for `https`), in their
    /// normal spelling. A private, loopback or link-local address, or one of
    /// this computer's own, is fetched only when listed itself.
    pub hosts: Vec<String>,
    /// The fetches in flight gateway-wide: a pool of their own with the
    /// tool pool's size.
    #[serde(default)]
    pub pool: WebFetchPool,
}

/// The web-source fetch pool (design ruling 9, §9): how many downloads run
/// at once across all requests, what runs and waits now, and the bytes one
/// download may hold. At most `slots × per_fetch_bytes` are held at once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WebFetchPool {
    /// The size in force: the tool pool's ([`PoolFacts::slots`]).
    pub slots: u32,
    /// Downloads running now.
    pub running: u32,
    /// Downloads waiting for a slot now.
    pub waiting: u32,
    /// `max_body_mb` in bytes, the most one download holds; 0 while
    /// `max_body_mb` is 0 (no limit), when web sources are refused.
    pub per_fetch_bytes: u64,
}

/// The gateway-wide tool pool (design §7.7, D19): how many tool runs and
/// native reads run at once, and the memory they are charged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PoolFacts {
    /// The size in force.
    pub slots: u32,
    /// The size derived from this machine: [`PoolInputs::cores`], at least
    /// one. The memory bounds the steps by their charges
    /// ([`PoolInputs::charges`]), not the size.
    pub derived: u32,
    pub derived_from: PoolInputs,
    /// The size was set by hand ("Parallel tool runs", the
    /// `documents.tool_slots` setting); `slots` is that setting, and the
    /// memory is not charged then.
    pub by_hand: bool,
    /// Steps are admitted while their charges fit
    /// [`PoolInputs::memory_bytes`]: false when the size was set by hand or
    /// the memory is not read on this system yet.
    pub memory_counted: bool,
    /// The memory charged to the steps running now, in bytes.
    pub charged_bytes: u64,
    /// Steps running now.
    pub running: u32,
    /// Steps waiting for a slot now.
    pub waiting: u32,
    /// The most slots one caller (a key, every keyless request together, or
    /// lmgw's own Chat and knowledge-base reads) holds at once: the size
    /// less one, at least one (security round A). Memory, where counted,
    /// bounds it further by [`StepCharge::per_caller`].
    #[serde(default)]
    pub per_caller_slots: u32,
}

/// What the pool's size is derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PoolInputs {
    /// The logical cores lmgw may use (a CPU quota respected).
    pub cores: u32,
    /// The memory lmgw may use, in bytes; `None` where lmgw does not read it
    /// yet (Windows, macOS), and the size then follows the cores alone.
    pub memory_bytes: Option<u64>,
    pub memory_source: Option<MemorySource>,
    /// What the main kinds of step are charged, and how many of each fit.
    pub charges: PoolCharges,
}

/// What the main kinds of step are charged (design §14 performance round):
/// the most memory each can use, its tools' limits and the outputs it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PoolCharges {
    /// A PDF's own runs and its layout (also the picture gate).
    pub pdf_file: StepCharge,
    /// One page's pictures cropped out of its render (also every step
    /// without a charge of its own).
    pub page_crops: StepCharge,
    /// One page of text recognition: its render and its read.
    pub ocr_page: StepCharge,
    /// One run of lmgw's reader (a web page, an e-mail, a picture's pixels).
    pub reader_run: StepCharge,
}

/// One kind of step's charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StepCharge {
    /// The memory one such step is charged, in bytes.
    pub bytes: u64,
    /// How many such steps run at once at most: the size, or fewer when
    /// their charges fill the memory (at least 1).
    pub fit: u32,
    /// How many such steps one caller runs at once at most: its share of
    /// the slots, and as many as fit the memory with one more such step
    /// left free for another caller (at least 1).
    #[serde(default)]
    pub per_caller: u32,
}

/// Where [`PoolInputs::memory_bytes`] was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MemorySource {
    /// The machine's memory.
    Physical,
    /// A smaller cgroup limit lmgw runs under (a container's, or a systemd
    /// unit's `MemoryMax=`).
    Cgroup,
}

/// Whether lmgw can confine the programs it starts for conversions here
/// (lmgw's document reader applying a Landlock ruleset).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Confinement {
    /// The Landlock ABI the reader applied, when it can confine.
    pub abi: Option<u32>,
    /// Why it cannot (no Landlock in the kernel, the reader missing, its
    /// check failing): the tools then run with their memory limit and closed
    /// descriptors alone, and LibreOffice is not used.
    pub why_not: Option<String>,
    /// What this kernel's Landlock cannot enforce, each with the ABI that
    /// would.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lacks: Vec<String>,
}

/// Whether a tool is there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ToolState {
    /// Its binary was found and answered with its version.
    Found,
    /// Its binary is on no directory lmgw looks in.
    Missing,
    /// Its binary is there, but its version run failed or did not finish:
    /// lmgw does not use it until a check succeeds. `message` says why.
    ProbeFailed,
}

/// One optional tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolRow {
    /// `poppler`, `tesseract`, `pandoc`, `ffmpeg`, `libreoffice`.
    pub id: String,
    /// Its name in plain words: "PDF tools (poppler)".
    pub label: String,
    pub state: ToolState,
    /// The version it reported, as it wrote it.
    pub version: Option<String>,
    /// The binary that was found (found, or failed its probe).
    pub path: Option<String>,
    /// What else its probe read: its binaries, language packs, audio
    /// decoders or installed parts.
    pub extras: Vec<ToolExtra>,
    /// Why its probe failed, its stderr's first lines included.
    pub message: Option<String>,
    /// Its licence, as an SPDX expression where there is one.
    pub licence: String,
    /// The kinds of file ([`FormatRow::label`]) lmgw reads with it now.
    /// Empty: lmgw does not use it for converting yet.
    pub used_for: Vec<String>,
    /// When this tool was last probed: unix seconds.
    pub checked_at: i64,
    /// Why lmgw runs this found tool without confinement (with its memory
    /// limit and closed descriptors alone): where it is installed, its
    /// language data's place, its confined check run failing, or no
    /// confinement here at all. `None` when it runs confined, or when lmgw
    /// does not start it.
    #[serde(default)]
    pub not_confined: Option<String>,
}

/// One fact a probe read beside the version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolExtra {
    pub kind: ExtraKind,
    /// What is there.
    pub present: Vec<String>,
    /// What lmgw looked for and is not there.
    pub absent: Vec<String>,
}

/// What a [`ToolExtra`] lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ExtraKind {
    /// The tool's binaries (poppler's `pdftotext`, `pdftoppm`, `pdftohtml`).
    Binaries,
    /// Installed language packs (tesseract's codes: `eng`, `deu`).
    Languages,
    /// The audio decoders that matter for videos (ffmpeg's `aac`, `mp3`,
    /// `opus`, `vorbis`, `flac`, `ac3`, `eac3`).
    AudioDecoders,
    /// Installed parts (LibreOffice's Writer, Calc, Impress).
    Components,
}

/// How well a kind of file is read here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Quality {
    /// Read, as well as lmgw reads it.
    Full,
    /// Read, with something left out (`said` names it).
    Partial,
    /// Not read (`said` says why, and what would read it).
    None,
}

/// Which kind of reader a handler is (design §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Tier {
    /// No model: a reader built into lmgw or an installed tool.
    Zero,
    /// A model reads it.
    Model,
}

/// One kind of file, as the page lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FormatRow {
    /// `text`, `office`, `pdf`, `scans`, `audio`.
    pub id: String,
    /// In plain words: "Word, PowerPoint, Excel, OpenDocument".
    pub label: String,
    /// The formats in it, by the names lmgw sniffs them as (`docx`, `pdf`).
    pub formats: Vec<String>,
    /// The handler that reads it here (`pdf.poppler`), `None` when nothing
    /// does.
    pub handler: Option<String>,
    pub tier: Option<Tier>,
    pub quality: Quality,
    /// What the person gets, or why nothing and what would change that.
    pub said: String,
    /// The tools ([`ToolRow::id`]) its handler needs that are missing or
    /// failed their probe.
    pub needs: Vec<String>,
}

/// A tool that is not installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MissingRow {
    /// [`ToolRow::id`].
    pub tool: String,
    pub label: String,
    /// How to install it here (one hint, for [`DocCaps::platform`]).
    pub install: Vec<InstallHint>,
    /// The kinds of file ([`FormatRow::label`]) installing it would let
    /// lmgw read. Empty: lmgw does not use it for converting yet.
    pub gives: Vec<String>,
    pub licence: String,
}

/// Where an install hint applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum InstallPlatform {
    /// Fedora (the RPM and the AppImage): a `dnf` line.
    Fedora,
    /// The server image: a derived image, since a `dnf` inside the container
    /// would not survive it being recreated (design §6.2).
    ServerImage,
    /// A Linux that is not Fedora-like (its `/etc/os-release` names neither
    /// `fedora`, `rhel` nor `centos` in `ID` or `ID_LIKE`, or is missing):
    /// lmgw has no package command for it, so the hint names the tool and
    /// leaves the install to the distribution's package manager.
    OtherLinux,
    Windows,
    Macos,
}

/// What an install hint is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum HintKind {
    /// A command to copy and run (`text` is the command, with newlines for
    /// several lines).
    Command,
    /// A web page to install it from (`text` is the address).
    Link,
    /// Nothing to install here yet (`text` says why).
    NotYet,
    /// A sentence naming the tool to install with the system's own package
    /// manager (`text` is the sentence; there is nothing to copy).
    Manual,
}

/// How to install one tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct InstallHint {
    pub platform: InstallPlatform,
    pub kind: HintKind,
    pub text: String,
    /// One more sentence, when there is one ("or ffmpeg from RPM Fusion").
    pub note: Option<String>,
}
