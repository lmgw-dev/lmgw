//! The Decision lab's server, `/decision-lab/api/*` (labs-store design §2):
//! the models that answer `/v1/systemone` with their facts and limits, the
//! token gauge, and Ask as a stored group whose cells the page posts one at
//! a time. Every cell is dispatched in process through the real
//! `/v1/systemone` handler ([`crate::proxy::systemone::dispatch`]), so the
//! hold, admission, fallbacks and the `systemone` log row are real; only the
//! auth middleware and the HTTP hop are skipped, as in the other labs. The
//! history, export and deletes are the lab store's shared routes
//! (`labs_store`).
//!
//! The request itself is built by `lmgw-api-types`'
//! `DecisionForm::body`, the function the page's Request panel shows.
//!
//! Set runs (§3.3) are a question set over a file's rows as a job
//! (`labs::batch`), each item answered through the cell's path.

/// Cancel, resume and delete a set run.
pub(super) mod batch_control;
/// A set run's metrics, and "Use as threshold".
pub(super) mod batch_metrics;
/// Set runs: preview, start, the list, one run and its items.
pub(super) mod batches;
/// One cell: build, dispatch, record. `pub(crate)` for the set-run job,
/// which answers its items through it.
pub(crate) mod cells;
/// The token gauge.
pub(super) mod count;
/// "Save as labelled set": a batch agent's apply run as a set run (§4).
pub(super) mod from_agent;
/// Ask: a stored group and its plan.
pub(super) mod groups;
/// An override's image data URLs as artifact refs and back.
mod images;
/// An override with no `model`: the cell's inserted.
mod model_member;
/// The model list.
pub(super) mod models;
