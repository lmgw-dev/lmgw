//! What a poppler run run again under the wide limit said ([`Note`]), kept
//! for the conversion it is inside: the file's scope ([`scope`], opened by
//! `convert::file::convert`) collects every run's notes, its lanes beside
//! each other included (they are polled in the file's task), and the file
//! turns them into its warnings (`convert::file::memory`). Outside a scope
//! (Chat, knowledge bases) nothing is kept here; those callers read the
//! notes a run hands back.

use std::sync::{Arc, Mutex};

use super::super::limit::Limit;

/// Why a run was run again under the wide limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// It ran out of memory.
    Memory,
    /// openjpeg ran out of memory decoding a JPEG 2000 image, and poppler
    /// drew it blank (`super::jpx`).
    Jpx,
}

/// Why a JPEG 2000 image was drawn blank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blank {
    /// It is damaged or in a form openjpeg does not read: not run again,
    /// as more memory decodes nothing (`super::jpx`).
    Damaged,
    /// Its decode did not fit the wide limit either.
    TooLarge,
    /// Its decode did not fit the run's own limit, and the file's deadline
    /// left no time to run it again.
    Deadline,
    /// Its decode did not fit the run's own limit, and the run again under
    /// the wide one ended without an answer (in words: "ran out of
    /// memory", "did not finish in time", "failed"); the first run's
    /// output is kept.
    AgainFailed(&'static str),
}

/// What a poppler run said of its memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// It needed more than `from`, was run again under the wide limit, and
    /// that run's output is what was used: it fit, or (`Why::Jpx`) at least
    /// one more image decoded there (`memory_limit_widened`).
    Widened {
        tool: &'static str,
        from: Limit,
        why: Why,
        span: Option<(u32, u32)>,
    },
    /// It drew a JPEG 2000 image blank under `limit`, for `why`
    /// (`image_not_decoded`).
    Undecoded {
        tool: &'static str,
        limit: Limit,
        why: Blank,
        span: Option<(u32, u32)>,
    },
}

impl Note {
    /// The pages of the run it concerns, 1-based; empty for the file.
    pub fn pages(&self) -> Vec<u32> {
        self.span()
            .map_or_else(Vec::new, |(a, b)| (a..=b).collect())
    }

    /// The pages of the run it concerns: one page, or a run over several
    /// (which of them holds the image is not said by poppler).
    pub fn span(&self) -> Option<(u32, u32)> {
        let (Self::Widened { span, .. } | Self::Undecoded { span, .. }) = self;
        *span
    }

    /// Where it was said, in words: "on page 3", "in the run over pages
    /// 1–64", "for the file".
    pub fn where_(&self) -> String {
        where_said(self.span())
    }

    /// The note in words, for `where_` ([`Note::where_`], or the pages of
    /// several alike notes).
    pub fn said(&self, where_: &str) -> String {
        let wide = Limit::Wide.said();
        match self {
            Self::Widened {
                tool,
                from,
                why: Why::Memory,
                ..
            } => format!(
                "{tool} needed more than {} of memory {where_}, so it was run again under \
                 {wide}, which it fit",
                from.said()
            ),
            Self::Widened {
                tool,
                from,
                why: Why::Jpx,
                ..
            } => format!(
                "{tool} could not decode a JPEG 2000 image {where_} within {}, so it was run \
                 again under {wide}, where it did",
                from.said()
            ),
            Self::Undecoded {
                tool,
                why: Blank::Damaged,
                ..
            } => format!(
                "{tool} could not decode a JPEG 2000 image {where_} and drew it blank: the \
                 image is damaged or in a form openjpeg does not read (not a matter of memory, \
                 so it was not run again)"
            ),
            Self::Undecoded {
                tool,
                why: Blank::TooLarge,
                ..
            } => format!(
                "{tool} could not decode a JPEG 2000 image {where_} within {wide} either, and \
                 drew it blank: the image is larger than about 340 million pixels"
            ),
            Self::Undecoded {
                tool,
                limit,
                why: Blank::Deadline,
                ..
            } => format!(
                "{tool} could not decode a JPEG 2000 image {where_} within {}, and drew it \
                 blank; it was not run again under {wide}, as the file's deadline left no time",
                limit.said()
            ),
            Self::Undecoded {
                tool,
                limit,
                why: Blank::AgainFailed(how),
                ..
            } => format!(
                "{tool} could not decode a JPEG 2000 image {where_} within {}, and drew it \
                 blank; run again under {wide} it {how}, so the first run's output is kept",
                limit.said()
            ),
        }
    }
}

/// "on page 3", "in the run over pages 1–64", "for the file".
pub fn where_said(span: Option<(u32, u32)>) -> String {
    match span {
        None => "for the file".to_string(),
        Some((a, b)) if a == b => format!("on page {a}"),
        Some((a, b)) => format!("in the run over pages {a}–{b}"),
    }
}

tokio::task_local! {
    static NOTES: Arc<Notes>;
}

/// The notes of the runs inside one scope.
#[derive(Debug, Default)]
pub struct Notes(Mutex<Vec<Note>>);

impl Notes {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// What was recorded, in order; the scope keeps nothing of it.
    pub fn take(&self) -> Vec<Note> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// `f` with `notes` keeping what its runs say.
pub async fn scope<F: std::future::Future>(notes: Arc<Notes>, f: F) -> F::Output {
    NOTES.scope(notes, f).await
}

/// `note` to the scope it is said in; outside one, nothing.
pub fn record(note: Note) {
    let _ = NOTES.try_with(|n| n.0.lock().unwrap_or_else(|e| e.into_inner()).push(note));
}
