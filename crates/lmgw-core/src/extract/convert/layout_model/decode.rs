//! The model's answer as regions: post-processing as docling does it.
//!
//! The pinned export (`docling-project/docling-layout-heron-onnx`) has
//! RT-DETR's own post-processing in its graph: for the page it answers
//! `labels` (class ids), `boxes` (corner form, scaled to the size given as
//! `orig_target_sizes`) and `scores`, one of each for every query, already
//! the best by score. What is left is docling's own filter
//! (`docling.utils.layout_postprocessor`, read 2026-11-18): a region stays
//! when its score reaches its label's threshold ([`Label::threshold`]).
//! Overlaps are resolved where the regions meet the page's lines
//! ([`super::super::pdf_layout`]'s `regions`).
//!
//! The answer is checked like any input: a label outside the table, a
//! score or a corner that is no number, and a box with no area are left
//! out; corners are cut to the page.

use super::super::doc::Rect;

/// The classes, by the model's class id (`config.json` `id2label`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Label {
    Caption,
    Footnote,
    Formula,
    ListItem,
    PageFooter,
    PageHeader,
    Picture,
    SectionHeader,
    Table,
    Text,
    Title,
    DocumentIndex,
    Code,
    CheckboxSelected,
    CheckboxUnselected,
    Form,
    KeyValueRegion,
}

impl Label {
    /// In the order of the model's class ids.
    pub const ALL: [Label; 17] = [
        Self::Caption,
        Self::Footnote,
        Self::Formula,
        Self::ListItem,
        Self::PageFooter,
        Self::PageHeader,
        Self::Picture,
        Self::SectionHeader,
        Self::Table,
        Self::Text,
        Self::Title,
        Self::DocumentIndex,
        Self::Code,
        Self::CheckboxSelected,
        Self::CheckboxUnselected,
        Self::Form,
        Self::KeyValueRegion,
    ];

    pub fn of(id: i64) -> Option<Self> {
        usize::try_from(id)
            .ok()
            .and_then(|i| Self::ALL.get(i))
            .copied()
    }

    /// The name `config.json` gives it (and a recorded answer uses).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Caption => "caption",
            Self::Footnote => "footnote",
            Self::Formula => "formula",
            Self::ListItem => "list_item",
            Self::PageFooter => "page_footer",
            Self::PageHeader => "page_header",
            Self::Picture => "picture",
            Self::SectionHeader => "section_header",
            Self::Table => "table",
            Self::Text => "text",
            Self::Title => "title",
            Self::DocumentIndex => "document_index",
            Self::Code => "code",
            Self::CheckboxSelected => "checkbox_selected",
            Self::CheckboxUnselected => "checkbox_unselected",
            Self::Form => "form",
            Self::KeyValueRegion => "key_value_region",
        }
    }

    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|l| l.as_str() == name)
    }

    /// The least score a region of this label is kept at: docling's
    /// `CONFIDENCE_THRESHOLDS`.
    pub fn threshold(self) -> f32 {
        match self {
            Self::SectionHeader
            | Self::Title
            | Self::Code
            | Self::CheckboxSelected
            | Self::CheckboxUnselected
            | Self::Form
            | Self::KeyValueRegion
            | Self::DocumentIndex => 0.45,
            _ => 0.5,
        }
    }
}

/// One region of a page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    pub label: Label,
    pub score: f32,
    /// Its box as shares of the page's width and height, each from 0 to 1.
    pub rect: Rect,
}

impl Region {
    /// A region from anywhere (the model, a recorded answer), checked:
    /// `None` for a score or a corner that is no number, a score under the
    /// label's threshold, and a box without area inside the page.
    pub fn checked(label: Label, score: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> Option<Self> {
        let all_finite = [score, x0, y0, x1, y1].iter().all(|v| v.is_finite());
        if !all_finite || score < label.threshold() {
            return None;
        }
        let rect = Rect {
            x0: x0.min(x1).clamp(0.0, 1.0),
            y0: y0.min(y1).clamp(0.0, 1.0),
            x1: x0.max(x1).clamp(0.0, 1.0),
            y1: y0.max(y1).clamp(0.0, 1.0),
        };
        (rect.width() > 0.0 && rect.height() > 0.0).then_some(Self {
            label,
            score: score.min(1.0),
            rect,
        })
    }
}

/// The regions of one page from the model's three outputs, the boxes
/// scaled to `side` (what `orig_target_sizes` was given as). As many are
/// read as all three outputs hold.
pub fn decode(labels: &[i64], boxes: &[f32], scores: &[f32], side: f32) -> Vec<Region> {
    let mut out = Vec::new();
    if !(side.is_finite() && side > 0.0) {
        return out;
    }
    let answers = labels.iter().zip(scores).zip(boxes.as_chunks::<4>().0);
    for ((&id, &score), b) in answers {
        let Some(label) = Label::of(id) else {
            continue;
        };
        out.extend(Region::checked(
            label,
            score,
            b[0] / side,
            b[1] / side,
            b[2] / side,
            b[3] / side,
        ));
    }
    out
}
