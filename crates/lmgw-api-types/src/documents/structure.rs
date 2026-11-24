//! `output: "json"` of `POST /v1/documents/convert` (document-conversion
//! design §7.1): a file's structure as parts (pages, slides, sheets, or one
//! document) holding blocks in reading order, each with its kind, its text,
//! its box on the page where the file has pages, and how it was read.
//!
//! Boxes are in PDF points (1/72 inch) from the page's top-left corner:
//! `[x0, y0, x1, y1]`. Parts without geometry (a Word document, a slide or a
//! sheet read from its XML, a text file) have `bbox: null`.

use serde::{Deserialize, Serialize};

/// One part of a file: a page, a slide, a sheet, or the whole document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocPart {
    pub kind: PartKind,
    /// 1-based: the page's number in the file (also when `pages` picked a
    /// few), the slide's or sheet's position.
    pub number: u32,
    /// A sheet's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The page's width in points, where the part is a page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width_pt: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height_pt: Option<f64>,
    /// How the part was read; `null` for a part that was not read (a PDF
    /// page without a text layer, named in the file's warnings).
    pub source: Option<BlockSource>,
    /// In reading order.
    pub blocks: Vec<DocBlock>,
}

/// What a part is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PartKind {
    Page,
    Slide,
    Sheet,
    /// A file without pages (Word, OpenDocument text, a text file): one part.
    Document,
}

/// One block of a part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocBlock {
    pub kind: BlockKind,
    /// A heading's level, 1 to 6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    /// The block's text; absent for a table (its cells hold the text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `[x0, y0, x1, y1]` in points from the page's top-left corner; `null`
    /// where the part has no geometry.
    pub bbox: Option<[f64; 4]>,
    pub source: BlockSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<DocTable>,
    /// A `figure`'s picture: its `id` in the file's `pictures` list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    /// A `figure`'s caption, when one sits right below or above it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// A `figure`'s description by the picture-description model, when the
    /// request asked for descriptions (`describe_pictures`) and the picture
    /// was described.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// What a block is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BlockKind {
    Heading,
    Paragraph,
    ListItem,
    Table,
    /// A picture: `picture` names it in the file's `pictures` list.
    Figure,
    Caption,
    Code,
    Formula,
    /// A line repeated at the top of the pages; left out of markdown and
    /// text, kept here.
    PageHeader,
    /// A line repeated at the bottom of the pages; left out of markdown and
    /// text, kept here.
    PageFooter,
}

/// How a block (or a part) was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BlockSource {
    /// lmgw's own readers (text, office files).
    Native,
    /// A PDF page's own text.
    TextLayer,
    /// Text recognition (not built yet).
    Ocr,
    /// A vision model (not built yet).
    Vision,
    /// A layout or table model on the CPU (not built yet).
    Onnx,
    /// A speech-to-text model's transcript (an audio or video file).
    Asr,
}

/// A table's grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocTable {
    pub rows: u32,
    pub cols: u32,
    /// The cells that have text, row by row.
    pub cells: Vec<DocCell>,
}

/// One cell of a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocCell {
    /// 0-based.
    pub row: u32,
    /// 0-based.
    pub col: u32,
    pub row_span: u32,
    pub col_span: u32,
    /// A header cell (the table's first row).
    pub header: bool,
    pub text: String,
}
