//! Whether a planned file fits the card, and with how long a conversation
//! (find-a-model design §3.1, D5, D6, D10, D120).
//!
//! One function over the plan's input ([`PlanFiles`]), the planned shape
//! (the plan's own parameters, through [`plan_fit::Sizer`], the footprint
//! admission charges) and a capacity, taken against two measures:
//! - **the whole card** (D5): what admission measures against, the declared
//!   GPU memory budget or else the devices' total, with the headroom added
//!   to every footprint and nothing else on the card, because admission
//!   unloads lmgw's other models when this one is asked for;
//! - **beside what runs now** (D120): the same card less what other
//!   programs hold on it now (the ledger's outside use) and less lmgw's own
//!   models on it now, each its charge plus what admission keeps free for
//!   it ([`crate::vram::VramScheduler::on_card_now`]).
//!
//! The context is the largest that fits, up to the trained one (D6), found
//! by the plan's own search ([`plan_fit::largest_fitting`]), so a fit and the
//! plan's warning never disagree about a number. The GPU hold changes no
//! mark (D10); it adds its line. With admission off "fits" still means
//! fits, and the note says nothing refuses the start.

use lmgw_api_types::model_find::{
    BesideTerms, CardMeasure, CardResident, FileFit, FitMark, FitMeasure, FitTerms,
};

use crate::config::Snapshot;
use crate::state::SharedState;

use super::plan_files::PlanFiles;
use super::plan_fit::{self, Search};
pub use super::plan_fit::{Card, Sizer};

/// The GPU hold's line on a fit (D10), shared with the finder's card.
pub use lmgw_api_types::model_find::HOLD_LINE;

/// The card now, from one ledger read: the whole card and what is beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct CardNow {
    pub card: Card,
    pub hold: bool,
    pub outside: Option<u64>,
    pub residents: Vec<CardResident>,
    /// lmgw's models on the card now that serve the planned file, left out
    /// of `residents` ([`Self::beside_itself`]).
    pub left_out: Vec<String>,
}

impl CardNow {
    /// The card of this install now, or `None` while admission has nothing
    /// to measure against (no GPU telemetry and no declared budget, D9).
    pub async fn of(state: &SharedState, snap: &Snapshot) -> Option<Self> {
        let now = state.vram.on_card_now(state, snap).await?;
        Some(Self {
            card: Card {
                capacity: now.capacity,
                headroom: now.headroom,
                budget: snap.settings.vram.budget_mb > 0,
                admission: snap.settings.vram.enabled,
            },
            hold: snap.settings.hold.active,
            outside: now.outside,
            residents: now
                .residents
                .into_iter()
                .map(|(class, model, bytes)| CardResident {
                    class: class.as_str().to_string(),
                    model,
                    bytes,
                })
                .collect(),
            left_out: Vec::new(),
        })
    }

    /// The card for a plan of the file `rel` in `class`'s models dir: lmgw's
    /// models on the card whose row serves that file are left out of what
    /// is beside it, so a re-plan of a loaded model (D26's "Fit again") is
    /// never fitted beside itself. Every row on that file goes: a start of
    /// the planned row replaces what its own container holds, and a second
    /// row on the same file is the same weights.
    pub fn beside_itself(
        mut self,
        snap: &Snapshot,
        class: crate::runtime::Class,
        rel: &str,
    ) -> Self {
        use crate::runtime::Class;
        let serving: Vec<&str> = match class {
            Class::Chat => snap
                .local_models
                .iter()
                .filter(|m| m.gguf_path == rel)
                .map(|m| m.model_id.as_str())
                .collect(),
            Class::Aux => snap
                .aux_models
                .iter()
                .filter(|m| m.gguf_path == rel)
                .map(|m| m.model_id.as_str())
                .collect(),
            _ => Vec::new(),
        };
        self.leave_out(class.as_str(), &serving);
        self
    }

    /// Leaves the residents of `class` named `models` out of what is beside
    /// the planned file, and notes them.
    pub fn leave_out(&mut self, class: &str, models: &[&str]) {
        let (out, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.residents)
            .into_iter()
            .partition(|r| r.class == class && models.contains(&r.model.as_str()));
        self.residents = kept;
        self.left_out.extend(out.into_iter().map(|r| r.model));
    }

    pub fn residents_bytes(&self) -> u64 {
        self.residents
            .iter()
            .map(|r| r.bytes)
            .fold(0, u64::saturating_add)
    }

    /// The room `measure` leaves, before the headroom.
    pub fn room(&self, measure: FitMeasure) -> u64 {
        match measure {
            FitMeasure::WholeCard => self.card.capacity,
            FitMeasure::Beside => self
                .card
                .capacity
                .saturating_sub(self.outside.unwrap_or(0))
                .saturating_sub(self.residents_bytes()),
        }
    }

    /// The card as a measure on the wire.
    pub fn dto(&self) -> CardMeasure {
        CardMeasure {
            capacity_bytes: self.card.capacity,
            headroom_bytes: self.card.headroom,
            budget: self.card.budget,
            admission: self.card.admission,
            hold: self.hold,
            outside_bytes: self.outside,
            residents: self.residents.clone(),
        }
    }

    /// The notes every fit on this card carries for `measure`.
    pub(crate) fn notes(&self, measure: FitMeasure) -> Vec<String> {
        let mut notes = Vec::new();
        if self.hold {
            notes.push(HOLD_LINE.to_string());
        }
        if !self.card.admission {
            notes.push(format!(
                "admission is off ({}), so nothing refuses a start that doesn't fit; \"fits\" \
                 still means the model fits",
                crate::config::ADMISSION_LABEL
            ));
        }
        if measure == FitMeasure::Beside && !self.left_out.is_empty() {
            notes.push(format!(
                "{} on the graphics card now {} this file, so {} not counted beside it: the \
                 fit is what the file takes in {} place",
                self.left_out.join(", "),
                if self.left_out.len() == 1 {
                    "serves"
                } else {
                    "serve"
                },
                if self.left_out.len() == 1 {
                    "it is"
                } else {
                    "they are"
                },
                if self.left_out.len() == 1 {
                    "its"
                } else {
                    "their"
                },
            ));
        }
        if measure == FitMeasure::Beside && self.outside.is_none() {
            notes.push(
                "lmgw can't tell what other programs use on the graphics card now (it has no \
                 device reading, or one of its own models on the card has no figure), so \
                 \"beside what runs now\" leaves out only lmgw's own models"
                    .into(),
            );
        }
        notes
    }

    fn beside(&self, measure: FitMeasure) -> Option<BesideTerms> {
        (measure == FitMeasure::Beside).then(|| BesideTerms {
            outside_bytes: self.outside,
            residents_bytes: self.residents_bytes(),
        })
    }
}

/// The fit of a planned file (module doc). `card` is `None` without a card
/// to measure against, which is the `NoCard` mark.
pub fn fit(
    files: &PlanFiles,
    sizer: &Sizer,
    card: Option<&CardNow>,
    measure: FitMeasure,
) -> FileFit {
    let trained = files.summary.context_length;
    let mut fit = FileFit {
        measure,
        trained_ctx: trained,
        bits_per_weight: files.layer_bits,
        parameters: files.parameters,
        ..FileFit::default()
    };
    if let Some(why) = &files.size_only {
        let mut by = by_size(files.listed_bytes, card, measure, why.clone());
        by.trained_ctx = trained;
        by.bits_per_weight = files.layer_bits;
        by.parameters = files.parameters;
        return by;
    }
    if files.layer_bits_estimated {
        fit.notes.push(
            "the bits per weight are the first part's alone: a later part's header couldn't \
             be read, and a file that mixes widths by layer can be wider or narrower over the \
             whole model"
                .into(),
        );
    }
    if sizer.on_cpu {
        // Nothing on the card, with a card to measure or without (D57).
        fit.mark = FitMark::Fits;
        fit.ctx = sizer.ctx;
        if let Some(card) = card {
            fit.notes.extend(card.notes(measure));
            fit.capacity_bytes = card.room(measure);
            fit.headroom_bytes = card.card.headroom;
            fit.beside = card.beside(measure);
        }
        fit.notes
            .push("planned on the CPU: it takes nothing on the graphics card".into());
        return fit;
    }
    let Some(card) = card else {
        fit.mark = FitMark::NoCard;
        fit.notes.push(
            "lmgw can't measure the graphics card yet: it has no reading of the card's memory \
             and no GPU memory budget is set"
                .into(),
        );
        return fit;
    };
    fit.notes.extend(card.notes(measure));
    fit.capacity_bytes = card.room(measure);
    fit.headroom_bytes = card.card.headroom;
    fit.beside = card.beside(measure);
    let room = Card {
        capacity: fit.capacity_bytes,
        ..card.card
    };
    let take = |fit: &mut FileFit, fp: crate::vram::Footprint| {
        fit.uses_bytes = fp.total_bytes;
        fit.terms = FitTerms {
            weights_bytes: fp.weights_bytes,
            kv_cache_bytes: fp.kv_cache_bytes,
            compute_bytes: fp.compute_bytes,
        };
        fit.notes.extend(fp.note);
    };
    let Some(ctx) = sizer.ctx else {
        // No context to size the conversation by: the weights alone, and
        // the footprint's note says the rest isn't sized.
        let fp = sizer.at_planned();
        fit.mark = match room.fits(&fp) {
            true => FitMark::Fits,
            false => FitMark::DoesNotFit,
        };
        take(&mut fit, fp);
        return fit;
    };
    match plan_fit::largest_fitting(&room, ctx, |c| sizer.at(c)) {
        Search::All => {
            fit.mark = if trained.is_none_or(|t| ctx >= t) {
                FitMark::Fits
            } else {
                FitMark::FitsShorter
            };
            fit.ctx = Some(ctx.max(1));
            take(&mut fit, sizer.at(ctx.max(1)));
        }
        Search::Upto(n) => {
            fit.mark = FitMark::FitsShorter;
            fit.ctx = Some(n);
            take(&mut fit, sizer.at(n));
        }
        Search::None => {
            fit.mark = FitMark::DoesNotFit;
            take(&mut fit, sizer.at(1));
        }
    }
    fit
}

/// The mark of a file whose header wasn't read, couldn't be, or doesn't
/// size the file (D8): its bytes plus the headroom against the room
/// `measure` leaves. `why` says why, for the fold. Bytes that aren't known
/// (`None`: a file the listing gives no size, a split file with a part
/// missing) are never a fit: the mark is `Unknown`.
pub fn by_size(
    bytes: Option<u64>,
    card: Option<&CardNow>,
    measure: FitMeasure,
    why: String,
) -> FileFit {
    let mut fit = FileFit {
        measure,
        uses_bytes: bytes.unwrap_or(0),
        terms: FitTerms {
            weights_bytes: bytes.unwrap_or(0),
            ..FitTerms::default()
        },
        ..FileFit::default()
    };
    let Some(card) = card else {
        fit.mark = FitMark::NoCard;
        fit.notes.push(why);
        return fit;
    };
    fit.notes.extend(card.notes(measure));
    fit.notes.push(why);
    fit.capacity_bytes = card.room(measure);
    fit.headroom_bytes = card.card.headroom;
    fit.beside = card.beside(measure);
    fit.mark = match bytes {
        None => {
            fit.notes.push(
                "the file's size isn't known either, so lmgw can't tell whether it fits".into(),
            );
            FitMark::Unknown
        }
        Some(b) if b.saturating_add(card.card.headroom) <= fit.capacity_bytes => {
            FitMark::FitsBySize
        }
        Some(_) => FitMark::DoesNotFitBySize,
    };
    fit
}

/// The plan's parameters at a fit's context (D26, the data WP5 writes): the
/// fit's context as `ctx_size`, and for a plan that ties its ubatch and
/// batch to the context (an aux row that reads an input in one ubatch,
/// `aux_plan.rs`), those two with it. `None` when the fit has no context or
/// keeps the planned one.
pub fn fitted_params(
    params: &serde_json::Map<String, serde_json::Value>,
    fit: &FileFit,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    use serde_json::{json, Value};
    let ctx = fit.ctx?;
    let planned = params.get("ctx_size").and_then(Value::as_u64);
    if planned == Some(ctx) || fit.mark != FitMark::FitsShorter {
        return None;
    }
    let mut out = params.clone();
    let tied = |k: &str| planned.is_some() && params.get(k).and_then(Value::as_u64) == planned;
    let (ub, b) = (tied("ubatch_size"), tied("batch_size"));
    out.insert("ctx_size".into(), json!(ctx));
    if ub {
        out.insert("ubatch_size".into(), json!(ctx));
    }
    if b {
        out.insert("batch_size".into(), json!(ctx));
    }
    Some(out)
}

/// The note a row keeps when its context is a fit's shorter one (D26), as
/// of `at`; `None` when the fit kept the planned context.
pub fn note(fit: &FileFit, at: &str) -> Option<lmgw_api_types::model_find::FitNote> {
    (fit.mark == FitMark::FitsShorter && fit.ctx != fit.trained_ctx).then(|| {
        lmgw_api_types::model_find::FitNote {
            ctx: fit.ctx.unwrap_or(0),
            trained_ctx: fit.trained_ctx,
            measure: fit.measure,
            capacity_bytes: fit.capacity_bytes,
            headroom_bytes: fit.headroom_bytes,
            at: at.to_string(),
        }
    })
}

#[cfg(test)]
mod tests;
