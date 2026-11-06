//! What a `/v1/responses` request may carry that only an upstream's own
//! `/v1/responses` can serve (openai egress design §6.1, WP5b): a hosted
//! tool (`web_search`, `file_search`, `code_interpreter`, `image_generation`,
//! `computer_use_preview`, …), a `tool_choice` that names one, an input item
//! lmgw's loop has no reading for (`item_reference`, a hosted tool's
//! `*_call`), a content part it has no reading for (`input_file`, an
//! `input_image` by `file_id`), and `truncation: "auto"`.
//!
//! The parser accepts each of them and records it here instead of refusing
//! it: a request forwarded natively reaches the upstream with its body as
//! sent, so the upstream runs what it has natively ("a gateway translates,
//! never refuses or drops what an upstream has natively"). The one refusal
//! is on the loop path (`responses::path`), with the error the parser used
//! to answer at once, so a loop upstream's client sees what it always saw.

use crate::candidates::{Facet, FacetSet};
use crate::error::GatewayError;

/// One part of a request only a native upstream can serve.
#[derive(Debug, Clone)]
pub struct NativeOnly {
    /// What it is, for a message naming it: `tool type 'web_search'`.
    pub what: String,
    /// The loop's refusal of it: the parser's own error before WP5b.
    pub error: GatewayError,
    /// The capability facet it uses (candidate-aliases §4.6): a hosted tool
    /// or a hosted tool's record is a tool call, an image by `file_id` is
    /// vision. Left out of the IR, it is in no facet the IR gives, so a
    /// candidate without the facet would otherwise be offered it.
    pub facet: Option<Facet>,
}

/// Every [`NativeOnly`] part of one request, in request order, each named
/// once.
#[derive(Debug, Clone, Default)]
pub struct NativeOnlyParts(Vec<NativeOnly>);

impl NativeOnlyParts {
    /// Note `what`, refused on the loop path with `error`. A second part of
    /// the same kind adds nothing: the first already names it.
    pub(super) fn note(&mut self, what: String, error: GatewayError) {
        self.note_using(what, None, error);
    }

    /// [`note`](Self::note) for a part that uses `facet`.
    pub(super) fn note_using(&mut self, what: String, facet: Option<Facet>, error: GatewayError) {
        if !self.0.iter().any(|n| n.what == what) {
            self.0.push(NativeOnly { what, error, facet });
        }
    }

    /// The facets the parts use, for the run's own (`responses::uses`).
    pub fn facets(&self) -> FacetSet {
        self.0
            .iter()
            .filter_map(|n| n.facet)
            .fold(FacetSet::default(), FacetSet::insert)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The first part noted: the one the loop's refusal answers with.
    pub fn first(&self) -> Option<&NativeOnly> {
        self.0.first()
    }

    /// Every part's name, joined for a message: `tool type 'web_search',
    /// input item type 'item_reference'`.
    pub fn named(&self) -> String {
        self.0
            .iter()
            .map(|n| n.what.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}
