//! The recommended file among a repo's weights files (find-a-model design
//! §3.2, D7 as the owner ruled it on 2026-11-04).
//!
//! For one measure (the whole card or beside what runs now, D120), in order:
//! 1. the largest file that fits with the model's whole trained context;
//! 2. else the largest that fits a conversation of at least 32 768 tokens,
//!    or the trained context when that is smaller;
//! 3. else none, and the line says so: every file stays choosable.
//!
//! Only a file whose header was read takes part (D8): its fit is the
//! header's, its parameter count is the header's descriptors' (D7), and its
//! width is bits per weight over the repeating layers, every part of a split
//! file's (D131). A file narrower than 4 bits per weight is never the pick,
//! and neither is one wider than the Q8 class (8.5 bits, Q8_0's own width),
//! so a small model that fits in BF16 or F32 gets its Q8_0. Both numbers are
//! this design's choices, not limits: they decide which file gets the mark,
//! and every file keeps its own mark in the list. Equal sizes go by name, so
//! the pick is stable.
//!
//! **The fold.** Under these rules the pick itself never fits only a shorter
//! conversation while a file of 4 to 8.5 bits keeps the whole one: rule 1
//! would have taken that file. The line "Q4_K_S remembers all 262 144 tokens,
//! at 4.6 bits per weight" is for a file the card shows that isn't the pick:
//! one the person chose themselves (D34), or the whole-card pick shown beside
//! the beside one (D120). [`full_context_for`] names it (D53).

use lmgw_api_types::model_find::{
    FileFit, FilePick, FitMark, FitMeasure, FullContextFile, PickRule,
};

use crate::hf::fmt_bytes;

/// No file narrower than this is recommended (D7).
pub const FLOOR_BITS: f64 = 4.0;
/// No file wider than the Q8 class is recommended (D7): Q8_0's own width.
pub const CAP_BITS: f64 = 8.5;
/// Rule 2's conversation, when the trained context is longer (D7).
pub const SHORTER_TOKENS: u64 = 32_768;

/// One weights file of a repo (a split file being one), with its fit by the
/// measure the pick is for.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Its path in the repo (the first part of a split file).
    pub file: String,
    /// The quant label its name carries, for the lines.
    pub quant: Option<String>,
    /// Every part's bytes.
    pub bytes: u64,
    pub fit: FileFit,
}

impl Candidate {
    fn name(&self) -> &str {
        self.quant.as_deref().unwrap_or(&self.file)
    }

    /// Within the width the pick takes (module doc), its header read.
    fn eligible(&self) -> bool {
        matches!(self.fit.mark, FitMark::Fits | FitMark::FitsShorter)
            && self
                .fit
                .bits_per_weight
                .is_some_and(|b| (FLOOR_BITS..=CAP_BITS).contains(&b))
    }

    fn fits_whole(&self) -> bool {
        self.fit.mark == FitMark::Fits
    }

    /// Fits at least `n` tokens.
    fn fits_at_least(&self, n: u64) -> bool {
        self.fits_whole() || (self.fit.mark == FitMark::FitsShorter && self.fit.ctx >= Some(n))
    }
}

/// `n` with its thousands apart: "262 144".
pub fn tokens(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Rule 2's conversation for a model trained on `trained` tokens.
pub fn shorter_target(trained: Option<u64>) -> u64 {
    trained.map_or(SHORTER_TOKENS, |t| t.min(SHORTER_TOKENS))
}

/// The largest of `cands` by bytes, equal sizes by name.
fn largest<'a>(cands: impl Iterator<Item = &'a Candidate>) -> Option<&'a Candidate> {
    cands.max_by(|a, b| a.bytes.cmp(&b.bytes).then_with(|| b.file.cmp(&a.file)))
}

/// The pick among `cands` by `measure` (module doc). `card_bytes` is the
/// whole card's capacity, for the line.
pub fn pick(cands: &[Candidate], measure: FitMeasure, card_bytes: u64) -> FilePick {
    let trained = cands.iter().find_map(|c| c.fit.trained_ctx);
    let target = shorter_target(trained);
    let on = match measure {
        FitMeasure::WholeCard => format!("your graphics card ({})", fmt_bytes(card_bytes)),
        FitMeasure::Beside => format!(
            "beside what's on your graphics card ({}) now",
            fmt_bytes(card_bytes)
        ),
    };
    let eligible = || cands.iter().filter(|c| c.eligible());
    if let Some(c) = largest(eligible().filter(|c| c.fits_whole())) {
        return FilePick {
            file: Some(c.file.clone()),
            rule: PickRule::WholeContext,
            measure,
            why: match c.fit.ctx.or(trained) {
                Some(n) => format!(
                    "The largest file that fits {on} with the whole conversation length the \
                     model can take ({} tokens).",
                    tokens(n)
                ),
                // A header that states no context: fitted by its weights
                // (D57), with no length to name.
                None => format!(
                    "The largest file that fits {on}. Its header doesn't say how long a \
                     conversation the model can take, so only its weights were measured."
                ),
            },
            full_context: None,
        };
    }
    if let Some(c) = largest(eligible().filter(|c| c.fits_at_least(target))) {
        return FilePick {
            file: Some(c.file.clone()),
            rule: PickRule::ShorterContext,
            measure,
            why: format!(
                "{} {}, the largest file that still remembers {} is this one: it fits {} \
                 tokens.",
                match trained {
                    Some(t) => format!("The model can remember {} tokens.", tokens(t)),
                    None => "The model's header doesn't say how long a conversation it can take."
                        .to_string(),
                },
                match measure {
                    FitMeasure::WholeCard => "On your graphics card".to_string(),
                    FitMeasure::Beside => "Beside what's on your graphics card now".to_string(),
                },
                tokens(target),
                tokens(c.fit.ctx.unwrap_or(0))
            ),
            full_context: full_context_for(c, cands),
        };
    }
    FilePick {
        file: None,
        rule: PickRule::None,
        measure,
        why: format!(
            "No file of this model fits {on} at {FLOOR_BITS} to {CAP_BITS} bits per weight with \
             a conversation of at least {} tokens. Every file is still in the list to choose \
             by hand.",
            tokens(target)
        ),
        full_context: None,
    }
}

/// When `chosen` fits only a shorter conversation, the largest smaller file
/// of `cands` of 4 to 8.5 bits per weight that keeps the whole trained
/// context, with the fold's line (module doc, D7).
pub fn full_context_for(chosen: &Candidate, cands: &[Candidate]) -> Option<FullContextFile> {
    if chosen.fits_whole() {
        return None;
    }
    let c = largest(
        cands
            .iter()
            .filter(|c| c.bytes < chosen.bytes && c.eligible() && c.fits_whole()),
    )?;
    let trained = c.fit.ctx.or(c.fit.trained_ctx)?;
    let bits = c.fit.bits_per_weight;
    Some(FullContextFile {
        file: c.file.clone(),
        quant: c.quant.clone(),
        trained_ctx: trained,
        bits_per_weight: bits,
        sentence: format!(
            "{} remembers all {} tokens, at {:.1} bits per weight.",
            c.name(),
            tokens(trained),
            bits.unwrap_or_default()
        ),
    })
}

#[cfg(test)]
mod tests;
