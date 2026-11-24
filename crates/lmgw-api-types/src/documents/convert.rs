//! `POST /v1/documents/convert` (document-conversion design §7.1, §7.2,
//! §7.6): one file or several in, each file's text out, with how it was read
//! and every gap named.
//!
//! The request is `multipart/form-data` with one `file` part per file and
//! the options as form fields, or JSON: [`ConvertRequest`], the files as
//! base64. The answer is always [`ConvertAnswer`], one entry per file in
//! request order, even for one file.

use serde::{Deserialize, Serialize};

/// The JSON form of the request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConvertRequest {
    /// The files, in the order the answer lists them. Names may repeat.
    pub files: Vec<ConvertFile>,
    #[serde(flatten)]
    pub options: ConvertOptions,
}

/// One file of the JSON form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConvertFile {
    /// The file's name, as the answer repeats it; the format is read from the
    /// bytes, never from the name.
    pub name: String,
    /// The file, base64 (standard alphabet, padding optional). It makes a
    /// file about a third larger, which counts against `max_body_mb`.
    pub data_base64: String,
}

/// What to make of the files: every option applies to every file. Fields
/// whose feature is not built yet are accepted at their default; another
/// value is answered with a [`WarningCode::NotYet`] or
/// [`WarningCode::ModelNotChosen`] warning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConvertOptions {
    pub output: Output,
    /// The pages to read, 1-based: `"3-7,10"`. Applies to each PDF; a file
    /// without pages is read whole, with a warning. Absent: every page.
    pub pages: Option<String>,
    /// Whether a model reads pages tier 0 reads badly. Absent: the setting
    /// (`missing_or_tables` once a page-reading model is chosen under
    /// Settings › Documents, off without one).
    pub model_help: Option<ModelHelp>,
    pub ocr: Ocr,
    /// tesseract language codes (`eng`, `deu`) text recognition reads: at
    /// most 32, each letters, digits and `_` (optionally after `script/`).
    /// Absent: the installed packs of the desktop's language and English.
    pub ocr_languages: Option<Vec<String>>,
    /// The most seconds one file's conversion may take, counted from its
    /// first step on the tool pool (waiting for a slot does not count). A
    /// file past it fails with `deadline_exceeded` and its tools are
    /// stopped; the other files go on. Absent: no deadline per file (each
    /// tool run still has its own named timeout).
    pub document_timeout: Option<f64>,
    /// `false` writes tables as plain lines of cells, separated by tabs.
    pub tables: bool,
    pub pictures: Pictures,
    /// A picture's least share of its page's area: smaller pictures are left
    /// out of the export, counted in a `pictures_left_out` warning (design
    /// §5.3; docling's `picture_area_threshold` default).
    pub picture_min_area: f64,
    /// The scale PDF pictures are cropped at: `72 × images_scale` dpi
    /// (docling's `images_scale`, default 2.0, so 144 dpi). More than 0.
    pub images_scale: f64,
    /// Describe each exported picture with the picture-description model
    /// (Settings › Documents, "Describe pictures with"): one call per
    /// picture, as the request's key. No model chosen: the pictures stay
    /// without a description and a `model_not_chosen` note says so.
    pub describe_pictures: bool,
    /// The prompt picture descriptions are asked with; absent: docling's
    /// default, "Describe this image in a few sentences.". The picture's
    /// caption and the paragraph before it go with it as context.
    pub picture_prompt: Option<String>,
    /// A picture's least share of its page's area for `describe_pictures`
    /// to describe it (docling's `picture_description_area_threshold`,
    /// default 0.05): a smaller exported picture is kept without a
    /// description and counted in `stats.pictures_below_describe_min_area`.
    /// It never leaves a picture out of the export; `picture_min_area` does.
    /// From 0 to 1.
    pub describe_min_area: f64,
    /// A line written between two pages of a PDF (markdown and text).
    pub page_break: Option<String>,
    /// Markdown tables without the space around each cell.
    pub compact_tables: bool,
    /// Answer 202 with a job ([`super::jobs::JobAccepted`]) instead of
    /// waiting for every file; the job's state and result are under
    /// `/v1/documents/jobs/{id}`.
    #[serde(rename = "async")]
    pub run_async: bool,
}

/// docling's default `picture_area_threshold`, so the adapter and the native
/// route agree (design §5.3).
pub const PICTURE_MIN_AREA_DEFAULT: f64 = 0.05;

/// docling's default `picture_description_area_threshold`: pictures
/// smaller than this share of their page are not described.
pub const DESCRIBE_MIN_AREA_DEFAULT: f64 = 0.05;

/// docling's default `images_scale`: PDF pictures are cropped at 144 dpi.
pub const IMAGES_SCALE_DEFAULT: f64 = 2.0;

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            output: Output::Markdown,
            pages: None,
            model_help: None,
            ocr: Ocr::Auto,
            ocr_languages: None,
            document_timeout: None,
            tables: true,
            pictures: Pictures::Placeholder,
            picture_min_area: PICTURE_MIN_AREA_DEFAULT,
            images_scale: IMAGES_SCALE_DEFAULT,
            describe_pictures: false,
            picture_prompt: None,
            describe_min_area: DESCRIBE_MIN_AREA_DEFAULT,
            page_break: None,
            compact_tables: false,
            run_async: false,
        }
    }
}

/// The text's form.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Output {
    /// Markdown: headings, lists and tables kept.
    #[default]
    Markdown,
    /// The structure: parts holding blocks in reading order, with their
    /// boxes and how each was read ([`ConvertedDocument::structure`]).
    Json,
    /// Plain text: the markdown's markup left out.
    Text,
}

/// When a model reads pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ModelHelp {
    Off,
    /// Only pages nothing could read (no text layer, no text recognition).
    Missing,
    /// Pages without text, and table-like pages.
    MissingOrTables,
    EveryPage,
}

/// Text recognition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Ocr {
    Off,
    /// Pages without text.
    #[default]
    Auto,
    /// Every page, replacing the text layer.
    Force,
}

/// What a picture becomes in the text (design §5.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Pictures {
    /// An `<!-- image -->` line where the picture was, as docling writes it.
    #[default]
    Placeholder,
    /// The picture in the markdown as a `data:` URI; with `output: json`
    /// its bytes are in the file's `pictures` list.
    Embedded,
    /// A relative link (`artifacts/image_000000_<sha256>.png`) to the
    /// picture's entry in the file's `pictures` list, which carries its
    /// bytes; the docling routes' zip holds them as files under
    /// `artifacts/`.
    Referenced,
}

/// The answer, whatever the files' outcomes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConvertAnswer {
    /// One entry per file, in request order.
    pub documents: Vec<ConvertedDocument>,
    pub summary: ConvertSummary,
    /// The tools the conversions ran, with their versions.
    pub tools: Vec<ToolUsed>,
    /// Notes about the request's options, for every file alike (an option
    /// whose feature is not built yet).
    pub warnings: Vec<ConvertWarning>,
}

/// One file's outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConvertedDocument {
    /// Its position in the request, 0-based.
    pub index: u32,
    pub name: String,
    pub status: DocStatus,
    /// The format read from its bytes (`pdf`, `docx`, `text`); `None` when
    /// it could not be told.
    pub format: Option<String>,
    /// The parts it has: a PDF's pages (those asked for), a deck's slides,
    /// a workbook's sheets, 1 for a document or a text.
    pub parts: u32,
    /// The text, as markdown or plain text; `None` when the file failed
    /// and for `output: json`.
    pub content: Option<String>,
    /// `output: json`: the parts and their blocks; `None` otherwise and
    /// when the file failed.
    pub structure: Option<Vec<super::structure::DocPart>>,
    /// The pictures exported, in reading order; each `figure` block and
    /// each picture line in the text names one by its `id`.
    #[serde(default)]
    pub pictures: Vec<super::pictures::DocPicture>,
    /// Only the docling-serve routes set it, when they were asked for both
    /// forms: `content` is the markdown and this is the same document as
    /// `output: text` writes it. Left out of the answer otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub warnings: Vec<ConvertWarning>,
    pub stats: ConvertStats,
    /// Why it failed (status `failed`).
    pub error: Option<ConvertError>,
}

/// A file's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DocStatus {
    /// Read whole.
    Converted,
    /// Read, with parts left out; `warnings` names them.
    Partial,
    Failed,
    /// Not read, because the request asked to leave it out.
    Skipped,
}

/// How a file was read (what the comparison script counts, design §10.1).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConvertStats {
    pub parts_by_source: PartsBySource,
    /// Headings per level, `#` to `######`, as the markdown has them.
    pub headings: [u32; 6],
    /// Tables in the markdown.
    pub tables: u32,
    /// Their cells, header cells included.
    pub table_cells: u32,
    pub pictures: u32,
    /// Model calls made for the file.
    pub model_calls: u32,
    /// The models that answered them, each with its calls and the pages it
    /// read; left out when no model was called.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<super::model_help::ModelCalls>,
    /// Exported pictures not sent for a description because they are
    /// smaller than `describe_min_area` of their page.
    #[serde(default)]
    pub pictures_below_describe_min_area: u32,
    /// The file's own conversion time, in milliseconds.
    pub time_ms: u64,
}

/// How many parts each source read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PartsBySource {
    /// A PDF page's own text.
    pub text_layer: u32,
    pub ocr: u32,
    pub vision: u32,
    /// lmgw's own readers (text, office files).
    pub native: u32,
    /// A speech-to-text model's transcript of an audio or video file
    /// (document-conversion design §4.2, WP7).
    #[serde(default)]
    pub asr: u32,
}

/// A gap in a file's text, or a note about an option: never silent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConvertWarning {
    pub code: WarningCode,
    /// What happened and what would change it, in plain words.
    pub message: String,
    /// The tool it concerns (`poppler`, `tesseract`).
    pub tool: Option<String>,
    /// The parts it concerns, 1-based (pages); empty for the whole file.
    pub parts: Vec<u32>,
    /// The request option it concerns (a [`WarningCode::NotYet`] note).
    pub option: Option<String>,
}

/// A warning's code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum WarningCode {
    /// A tool that would read these parts is not installed.
    ToolMissing,
    /// These parts have no reader yet (a scanned page).
    NoReader,
    /// No model is chosen for what the request asked a model to do.
    ModelNotChosen,
    /// The GPU hold paused the model; tier 0's text is returned.
    ModelHeld,
    /// The key may not use the chosen model.
    ModelNotAllowed,
    /// One part's tool run or model call failed; the rest is returned.
    PartFailed,
    /// The page-reading model cannot run now and the fallback that answers
    /// in its place takes no pictures, so it cannot read a page; tier 0's
    /// text is returned. The message names the fallback.
    ModelCannotSee,
    /// The page-reading model's answer was not in the form asked for (DocTags
    /// without its tags, or not well-formed); the text it holds was kept.
    ModelAnswerMalformed,
    /// A page larger than `PAGE_MAX_PIXELS` at the render resolution was
    /// sent to the page-reading model scaled down; the message names both.
    PageScaled,
    HeaderRemoved,
    EquationsDropped,
    /// Pictures that are in the file but not exported whole: a figure
    /// caption with no picture found for it (a vector drawing on a page the
    /// layout model did not read), a picture poppler could not render, one
    /// past a named bound, or in a format lmgw does not export.
    PicturesPartial,
    /// Text inside pictures (a chart's labels) was left out of the text;
    /// `json` keeps it in the figure blocks.
    PictureText,
    /// Pictures left out of the export on purpose, each kind counted:
    /// smaller than `picture_min_area`, repeated at the top or bottom of the
    /// pages (a logo), covering the page (its background or scan).
    PicturesLeftOut,
    /// The document text a picture's description call carries as context
    /// (its caption, the paragraph before it) was longer than
    /// `PICTURE_CONTEXT_MAX_BYTES` and was sent cut; the message names the
    /// pictures and the bound.
    PictureContextCut,
    FormulasPartial,
    OcrLanguageMissing,
    /// tesseract was unsure of a page's words (their mean confidence is
    /// below `OCR_LOW_CONFIDENCE`): a page on its side, a poor scan.
    OcrLowConfidence,
    /// A page too large for `MAX_OCR_PIXELS` at `OCR_DPI` was rendered at a
    /// lower resolution for text recognition.
    OcrPageReduced,
    /// A scanned page was turned upright before its text was recognised:
    /// by tesseract's orientation data, or (without it) because its lines
    /// read as vertical. The message says which and by how much.
    OcrPageTurned,
    /// An option whose feature is not built yet; the file was converted
    /// without it.
    NotYet,
    /// Pages the request asked for that the file does not have.
    PagesOutOfRange,
    /// `pages` was given for a file that has no pages; it was read whole.
    PagesNotApplicable,
    /// An option that changes nothing for this file or request (`page_break`
    /// on a file without pages, `compact_tables` where no table is written);
    /// the note names it.
    OptionNotApplicable,
    /// A poppler run needed more than its own memory limit
    /// (`POPPLER_TEXT_MEMORY_LIMIT_BYTES`, `POPPLER_IMAGE_MEMORY_LIMIT_BYTES`,
    /// `OCR_RENDER_MEMORY_LIMIT_BYTES`) and was run again under
    /// `POPPLER_WIDE_MEMORY_LIMIT_BYTES`, and what that run gave is what was
    /// used: it fit, or a JPEG 2000 image that did not decode within the
    /// first limit did there. An image it still could not decode is an
    /// `image_not_decoded` warning of its own. The message names the tool,
    /// both limits and the pages (the page, or the run over several pages).
    MemoryLimitWidened,
    /// poppler could not decode a JPEG 2000 image on these pages and drew
    /// it blank (in a page's render, a picture, a crop, text recognition's
    /// render): a damaged image, or one in a form openjpeg does not read
    /// (not run again: more memory decodes nothing); one larger than about
    /// 340 million pixels, past `POPPLER_WIDE_MEMORY_LIMIT_BYTES` too; or,
    /// within its own limit, when the file's deadline left no time to run
    /// it again or the run again ended without an answer (the first run's
    /// output is kept). The message says which, and the limit.
    ImageNotDecoded,
}

/// Why a file failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConvertError {
    pub code: ErrorCode,
    pub message: String,
    /// The missing tool, for [`ErrorCode::ToolMissing`].
    pub tool: Option<String>,
}

/// A failed file's code. A request whose files all failed answers the one
/// file's status ([`ErrorCode::status`]: 415 for `tool_missing`, `no_reader`
/// and `unsupported`, 413 for `output_too_large`, 503 for `model_held`, 403
/// for `model_not_allowed`, 502 for `model_failed`, 422 for an unreadable
/// file), or 422 for several.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ErrorCode {
    /// The tool that reads this format is not installed.
    ToolMissing,
    /// lmgw recognises the format and has no reader for it yet.
    NoReader,
    /// Not a format lmgw recognises.
    Unsupported,
    /// The file is damaged, or its tool could not read it.
    Unreadable,
    /// `data_base64` is not base64.
    InvalidBase64,
    /// None of the requested pages is in the file.
    PagesOutOfRange,
    /// The request's converted output passed `MAX_UNCOMPRESSED_BYTES`, the
    /// most all its files may produce together.
    OutputTooLarge,
    /// The file was not converted within the deadline the caller set
    /// (`document_timeout`, the native option and docling's); the message
    /// names it.
    DeadlineExceeded,
    /// An async job's file that had no outcome when the gateway stopped.
    Interrupted,
    /// A docling `http` source the fetch rules refused: an address in a
    /// range the allowed sites do not name, or a redirect they refuse
    /// (document-conversion design §7.4.1). The message starts with the rule's
    /// name (`source_address_refused`, `source_redirect_refused`).
    SourceRefused,
    /// A docling `http` source whose host did not resolve or could not be
    /// reached (`source_unreachable`).
    SourceUnreachable,
    /// A docling `http` source not fetched within `SOURCE_FETCH_TIMEOUT` or
    /// `SOURCE_FETCH_CONNECT_TIMEOUT`, which the message names
    /// (`source_timeout`).
    SourceTimeout,
    /// A docling `http` source larger than `max_body_mb`
    /// (`source_too_large`).
    SourceTooLarge,
    /// A docling `http` source whose host answered an HTTP error; the message
    /// has the status only (`source_http_status`).
    SourceHttpStatus,
    /// The file needs a model and none is chosen under Settings › Documents
    /// (an audio or video file without a speech-to-text model, WP7). 415,
    /// as a format with no tool is.
    ModelNotChosen,
    /// Nothing but the page-reading model could read the file's pages, and
    /// it is paused (the GPU hold, a benchmark's lease) or cannot run now
    /// while nothing that takes pictures answers for it; or the speech-to-text
    /// model of an audio or video file is paused and has no fallback. 503, as
    /// a held model's chat request is: not an outage, and it clears when the
    /// hold is released.
    ModelHeld,
    /// Nothing but the page-reading model could read the file's pages, or a
    /// file needs the speech-to-text model, and the key may not use it (its
    /// scope, its budget, its tokens-per-minute window, an expired or
    /// disabled key).
    ModelNotAllowed,
    /// Nothing but the page-reading model could read the file's pages, and
    /// its calls failed (the model or its upstream answered an error, ran
    /// to its token limit, refused, or answered nothing readable); or the
    /// speech-to-text model's call failed (the message has the upstream's
    /// status).
    ModelFailed,
    /// lmgw could not start the file's tool as it runs it: its document
    /// reader, which starts every tool, could not confine it or set its
    /// limits (the kernel refused the rules, a resource ran out), so the
    /// tool was not started; or LibreOffice could not run here. lmgw's own
    /// failure, not the file's: 500; the gateway's log has the detail.
    ToolNotStarted,
    /// The file nests deeper than a named bound its reader allows before it
    /// is read (pandoc's DocBook and FictionBook input past
    /// `PANDOC_MAX_XML_DEPTH`, security round A): its reader's time grows
    /// with the square of the nesting. 422, as an unreadable file is.
    TooDeep,
    /// The file's tool needed more memory than its named limit allows, the
    /// wide one it is run again under included
    /// (`POPPLER_WIDE_MEMORY_LIMIT_BYTES`), or more than its own when the
    /// file's deadline left no time to run it again; the message names the
    /// limit. 422, as an unreadable file is.
    MemoryLimit,
}

impl ErrorCode {
    /// The HTTP status a one-file request answers when this file failed.
    pub fn status(self) -> u16 {
        match self {
            Self::ToolMissing | Self::NoReader | Self::Unsupported => 415,
            Self::Unreadable | Self::InvalidBase64 | Self::PagesOutOfRange => 422,
            Self::OutputTooLarge => 413,
            Self::DeadlineExceeded => 504,
            Self::Interrupted => 503,
            Self::SourceRefused => 403,
            Self::SourceUnreachable | Self::SourceHttpStatus => 502,
            Self::SourceTimeout => 504,
            Self::SourceTooLarge => 413,
            Self::ModelNotChosen => 415,
            Self::ModelHeld => 503,
            Self::ModelNotAllowed => 403,
            Self::ModelFailed => 502,
            Self::ToolNotStarted => 500,
            Self::TooDeep | Self::MemoryLimit => 422,
        }
    }
}

/// The files' outcomes, counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConvertSummary {
    pub files: u32,
    pub converted: u32,
    pub partial: u32,
    pub failed: u32,
    pub skipped: u32,
}

/// A tool a conversion ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolUsed {
    /// `poppler`.
    pub id: String,
    pub version: String,
}
