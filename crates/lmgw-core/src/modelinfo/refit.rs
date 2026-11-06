//! A row's context fitted to the card (find-a-model design D26, as the
//! owner ruled it on 2026-11-04): the writer of WP2's
//! [`fit::fitted_params`] and [`fit::note`].
//!
//! - **Get it** ([`for_get`], §6.2 steps 2 and 3): the downloaded file is
//!   planned from disk, its plan is fitted to the measure the card showed,
//!   and when the card fits a shorter conversation than the trained one the
//!   row takes the largest that fits. A chat row's context moves alone,
//!   its batches where the plan put them. An aux row's plan is made again at that
//!   context, since an encoder's plan ties its ubatch and batch to it and a
//!   projector's ubatch floor is judged against them; the re-plan is sized
//!   again, and when its own figures fit less (a projector floor above the
//!   context), planned again at what they fit, until a plan fits whole.
//! - **Fit again** ([`for_row`], the model editor's): the row's own
//!   parameters, searched up to the trained context, against the card as it
//!   is then, with the row left out of what is beside it
//!   ([`fit::CardNow::beside_itself`], D55). The context moves, and an aux
//!   row's ubatch and batch move with it where the row ties them to it (an
//!   encoder reads an input in one ubatch); a chat row's batches stay, as
//!   llama.cpp doesn't tie them to the context.
//!
//! A confirmed Fit again saves the context it was shown when that context
//! still fits the card as it is then ([`Refit::confirmable`]), not only
//! when the fit is the same to the token: the measure beside what runs
//! reads the card's live bytes, which move a little all the time.
//!
//! The note says what the context was fitted to (D26); a row whose fit is
//! the whole trained context keeps none.

use serde_json::{json, Map, Value};

use lmgw_api_types::model_find::{FileFit, FitMark, FitMeasure, FitNote, PlanFit};
use lmgw_api_types::model_get::FitChange;

use super::fit::{self, CardNow};
use super::plan_files::PlanFiles;
use super::plan_fit::{self, Sizer};
use super::{class_for, models_dir_of};
use crate::runtime::Class;
use crate::state::SharedState;

/// The server's time as a note records it (`YYYY-MM-DD HH:MM:SS`, UTC).
pub fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// A plan fitted to a measure.
#[derive(Debug, Clone)]
pub struct Fitted {
    /// The plan as `local_model_plan` answers it for the file on disk.
    pub plan: Value,
    /// The parameters the row takes: the plan's, at the fitted context.
    pub params: Map<String, Value>,
    /// The fit those parameters come from, by the measure asked for (the
    /// shorter context's, with the model's trained context).
    pub fit: FileFit,
    /// The note the row keeps (D26); `None` when it keeps the plan's
    /// context.
    pub note: Option<FitNote>,
    /// The plan's warnings, less the one that says the planned context
    /// doesn't fit when the fitted one does.
    pub warnings: Vec<String>,
}

/// The plan's input of `rel` in `class`'s models dir, read from disk.
async fn files_of(state: &SharedState, class: Class, rel: &str) -> Result<PlanFiles, String> {
    let snap = state.snapshot();
    let dir = models_dir_of(&snap, class);
    let (rel, _) = super::resolve(&dir, rel, class)?;
    let aux_dir = models_dir_of(&snap, Class::Aux);
    tokio::task::spawn_blocking(move || PlanFiles::from_disk(&dir, &rel, class, &aux_dir))
        .await
        .map_err(|e| format!("gguf read panicked: {e}"))?
}

/// Whether `files` plan as the aux class (as `plan_from` decides it).
fn plans_as_aux(class: Class, files: &PlanFiles) -> bool {
    class == Class::Aux || class_for(&files.summary).0 == "aux"
}

/// Get it's plan and fit (module doc) of the downloaded `rel` in `class`'s
/// models dir, by `measure`.
pub async fn for_get(
    state: &SharedState,
    class: Class,
    rel: &str,
    measure: FitMeasure,
) -> Result<Fitted, String> {
    let snap = state.snapshot();
    let files = files_of(state, class, rel).await?;
    let plan = super::plan_from(state, &snap, class, &files).await?;
    let params = plan["params"].as_object().cloned().unwrap_or_default();
    let fits: PlanFit = serde_json::from_value(plan["fit"].clone())
        .map_err(|e| format!("the plan's fit didn't read: {e}"))?;
    let fit = match measure {
        FitMeasure::WholeCard => fits.whole_card,
        FitMeasure::Beside => fits.beside,
    };
    let mut warnings: Vec<String> = plan["warnings"]
        .as_array()
        .map(|w| {
            w.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if fit.mark != FitMark::FitsShorter {
        return Ok(Fitted {
            plan,
            params,
            note: None,
            fit,
            warnings,
        });
    }
    let aux = plans_as_aux(class, &files);
    // The plan's own warning that the trained context doesn't fit: the row
    // doesn't take that context, so it isn't the row's.
    let sizer = match aux {
        true => Sizer::aux(&files, &params),
        false => Sizer::chat(&files, &params),
    };
    if let Some(w) = plan_fit::warning(state, &snap, &sizer).await {
        warnings.retain(|x| *x != w);
    }
    let (params, fit) = match aux {
        false => (ctx_only(&params, &fit), fit.clone()),
        true => {
            let card = CardNow::of(state, &snap)
                .await
                .map(|c| c.beside_itself(&snap, files.found_in, &files.rel));
            aux_replanned(state, &files, card.as_ref(), measure, fit, params).await?
        }
    };
    let note = fit::note(&fit, &now());
    Ok(Fitted {
        plan,
        params,
        fit,
        note,
        warnings,
    })
}

/// The aux plan made again at the fitted context (module doc): the
/// parameters and the fit they take, with the model's trained context.
/// Every round plans at a context below the last one's, so it ends.
async fn aux_replanned(
    state: &SharedState,
    files: &PlanFiles,
    card: Option<&CardNow>,
    measure: FitMeasure,
    fit: FileFit,
    planned: Map<String, Value>,
) -> Result<(Map<String, Value>, FileFit), String> {
    let snap = state.snapshot();
    let kind = class_for(&files.summary).1;
    let trained = files.summary.context_length;
    let Some(mut ctx) = fit.ctx else {
        return Ok((planned, fit));
    };
    loop {
        let mut at = files.clone();
        at.summary.context_length = Some(ctx);
        let replan = super::aux_plan(state, &snap, &at, kind).await?;
        let params = replan["params"].as_object().cloned().unwrap_or_default();
        let mut again = fit::fit(&at, &Sizer::aux(&at, &params), card, measure);
        again.trained_ctx = trained;
        match (again.mark, again.ctx) {
            // At the context it was planned at: the row's.
            (FitMark::Fits | FitMark::FitsShorter, Some(c)) if c >= ctx => {
                again.mark = FitMark::FitsShorter;
                again.ctx = Some(ctx);
                return Ok((params, again));
            }
            (FitMark::FitsShorter, Some(c)) if c < ctx => ctx = c,
            // Nothing fits the re-plan at all: the first fit's parameters,
            // tied as the fit sized them.
            _ => return Ok((fit::fitted_params(&planned, &fit).unwrap_or(planned), fit)),
        }
    }
}

/// A chat plan's parameters at a shorter fit's context: the context alone
/// moves (D26; a chat row's batches aren't tied to it).
fn ctx_only(params: &Map<String, Value>, fit: &FileFit) -> Map<String, Value> {
    let mut out = params.clone();
    if let (FitMark::FitsShorter, Some(ctx)) = (fit.mark, fit.ctx) {
        out.insert("ctx_size".into(), json!(ctx));
    }
    out
}

/// What "Fit again" finds for a row (module doc).
#[derive(Debug, Clone)]
pub struct Refit {
    pub row_id: i64,
    pub class: Class,
    /// The row's own parameters as they were read (the keys a fit moves
    /// among them).
    pub params: Map<String, Value>,
    pub measure: FitMeasure,
    /// The fit of the row's parameters on the card now.
    pub fit: FileFit,
    /// What a save changes, in key order (`ctx_size`, `ubatch_size`,
    /// `batch_size`).
    pub changes: Vec<FitChange>,
    /// The note a save writes; `None` when the whole trained context fits.
    pub note: Option<FitNote>,
    /// The row's note as it is now.
    pub current: Option<FitNote>,
}

/// The keys a fit moves.
const MOVED: [&str; 3] = ["ctx_size", "ubatch_size", "batch_size"];

/// "Fit again" of the `class` row `model_id` (module doc).
pub async fn for_row(state: &SharedState, class: Class, model_id: &str) -> Result<Refit, String> {
    let snap = state.snapshot();
    let (row_id, rel, mut params) = match class {
        Class::Chat => {
            let row = snap
                .local_models
                .iter()
                .find(|m| m.model_id == model_id)
                .ok_or_else(|| format!("no chat model with model_id '{model_id}'"))?;
            let params = serde_json::to_value(&row.params)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            (row.id, row.gguf_path.clone(), params)
        }
        Class::Aux => {
            let row = snap
                .aux_models
                .iter()
                .find(|m| m.model_id == model_id)
                .ok_or_else(|| format!("no aux model with model_id '{model_id}'"))?;
            let mut params = Map::new();
            for (k, v) in [
                ("ctx_size", row.ctx_size),
                ("ubatch_size", row.ubatch_size),
                ("batch_size", row.batch_size),
            ] {
                if let Some(v) = v {
                    params.insert(k.into(), json!(v));
                }
            }
            if let Some(p) = &row.mmproj_path {
                params.insert("mmproj_path".into(), json!(p));
            }
            if let Some(b) = row.backend {
                params.insert("backend".into(), json!(b.as_str()));
            }
            (row.id, row.gguf_path.clone(), params)
        }
        other => {
            return Err(format!(
                "Fit again fits a chat or an aux row's context; the {} class has none",
                other.as_str()
            ))
        }
    };
    params.retain(|_, v| !v.is_null());
    let current = note_of(state, class, row_id).await;
    let measure = current
        .as_ref()
        .map(|n| n.measure)
        .unwrap_or(FitMeasure::Beside);
    let files = files_of(state, class, &rel).await?;
    let card = CardNow::of(state, &snap)
        .await
        .map(|c| c.beside_itself(&snap, class, &files.rel));
    // Searched up to the trained context, whatever the row has now: a card
    // with more room gives the context back.
    let mut sized = params.clone();
    if let Some(t) = files.summary.context_length {
        // Every tie at the trained context, so the sizer ties them at
        // each context it tries (a floor above a context costs nothing
        // more: the compute buffer is charged at the ubatch the context
        // clamps it to).
        for c in changes_at(class, &params, t) {
            sized.insert(c.key, json!(t));
        }
        sized.insert("ctx_size".into(), json!(t));
    }
    let sizer = match plans_as_aux(class, &files) {
        true => Sizer::aux(&files, &sized),
        false => Sizer::chat(&files, &sized),
    };
    let fit = fit::fit(&files, &sizer, card.as_ref(), measure);
    let (changes, note) = match (fit.mark, fit.ctx) {
        (FitMark::Fits | FitMark::FitsShorter, Some(ctx)) => {
            (changes_at(class, &params, ctx), note_at(&fit, ctx))
        }
        // Doesn't fit, can't be measured, or fitted by size: nothing to
        // save, and the note stays as it is.
        _ => (Vec::new(), current.clone()),
    };
    Ok(Refit {
        row_id,
        class,
        params,
        measure,
        fit,
        changes,
        note,
        current,
    })
}

/// The changes that set the `class` row with `params` to `ctx` tokens of
/// context: the context, and for an aux row that reads an input in one
/// ubatch (an ubatch and a batch at least its context now, the tie an
/// encoder's plan makes, D29) those two with it. One equal to the context
/// follows it; one above it (a projector's floor, D86) stays, until the
/// context grows past it. A chat row's batches aren't tied to its context.
/// In [`MOVED`]'s order; empty when the row is at that context already.
pub fn changes_at(class: Class, params: &Map<String, Value>, ctx: u64) -> Vec<FitChange> {
    let row_ctx = params.get("ctx_size").and_then(Value::as_u64);
    MOVED
        .into_iter()
        .filter_map(|k| {
            let from = params.get(k).and_then(Value::as_u64);
            let to = match (k, class, from, row_ctx) {
                ("ctx_size", ..) => ctx,
                (_, Class::Aux, Some(f), Some(r)) if f == r => ctx,
                (_, Class::Aux, Some(f), Some(r)) if f > r => f.max(ctx),
                _ => return None,
            };
            (from != Some(to)).then(|| FitChange {
                key: k.into(),
                from,
                to: Some(to),
            })
        })
        .collect()
}

/// The note a row at `ctx` tokens keeps, from a fit on the card now whose
/// largest context is at least `ctx`; `None` at the trained context.
fn note_at(fit: &FileFit, ctx: u64) -> Option<FitNote> {
    let mut at = fit.clone();
    at.ctx = Some(ctx);
    at.mark = match fit.trained_ctx {
        Some(t) if ctx >= t => FitMark::Fits,
        _ => FitMark::FitsShorter,
    };
    fit::note(&at, &now())
}

/// Why a confirmed Fit again can't be saved as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outdated {
    /// The row changed since: the confirmed changes aren't what its
    /// parameters make of that context now.
    RowChanged,
    /// The confirmed context doesn't fit the card any more.
    NoLongerFits,
}

impl Refit {
    /// The changes and the note a confirm of `confirmed` saves (module
    /// doc): the context it sets (the row's own when it sets none) has to
    /// fit the card now, and the changes have to be what the row as it is
    /// now makes of that context.
    pub fn confirmable(
        &self,
        confirmed: &[FitChange],
    ) -> Result<(Vec<FitChange>, Option<FitNote>), Outdated> {
        let row_ctx = self.params.get("ctx_size").and_then(Value::as_u64);
        let ctx = match confirmed.iter().find(|c| c.key == "ctx_size") {
            Some(c) => c.to.ok_or(Outdated::RowChanged)?,
            None => match row_ctx {
                Some(c) => c,
                // A row with no context set, confirmed with nothing to
                // change: the note stands as the fit makes it.
                None if self.changes.is_empty() && confirmed.is_empty() => {
                    return Ok((Vec::new(), self.note.clone()))
                }
                None => return Err(Outdated::RowChanged),
            },
        };
        let changes = changes_at(self.class, &self.params, ctx);
        if changes != confirmed {
            return Err(Outdated::RowChanged);
        }
        let fits = matches!(self.fit.mark, FitMark::Fits | FitMark::FitsShorter)
            && self.fit.ctx.is_some_and(|largest| ctx <= largest);
        if !fits {
            return Err(Outdated::NoLongerFits);
        }
        Ok((changes, note_at(&self.fit, ctx)))
    }
}

/// The `class` row `row_id`'s note, as stored.
pub async fn note_of(state: &SharedState, class: Class, row_id: i64) -> Option<FitNote> {
    let text = crate::store::fit_note(&state.db, class.as_str(), row_id)
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&text).ok()
}

/// Write the `class` row `row_id`'s note, or remove it.
pub async fn write_note(
    state: &SharedState,
    class: Class,
    row_id: i64,
    note: Option<&FitNote>,
) -> Result<(), String> {
    let text = note
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| e.to_string())?;
    crate::store::set_fit_note(&state.db, class.as_str(), row_id, text.as_deref())
        .await
        .map_err(|e| e.to_string())
}
