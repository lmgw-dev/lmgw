//! The `model_get` job kind: one Get it, set up server-side (find-a-model
//! design §6.2, D25, D26, D27, D122).
//!
//! 1. **Wait** for its downloads. The job reads its download rows again on
//!    every frame of the jobs feed (a transfer's progress, start or end, a
//!    cancel, or a row's status written outside a transfer, which publishes
//!    a frame of its own: [`super::JobManager::publish`]), so it moves when
//!    they do and costs nothing while nothing moves. A failed download fails
//!    the get with that download's sentence, its "Retry" said as "Get it
//!    again" (which queues again only what isn't here); a cancelled one
//!    cancels it. A row that waits with no transfer (paused on a metered
//!    connection, or cut and not queued again) is said, and waited for.
//! 2. **Answer** a row that already serves the file (D87): the one this get
//!    created before a restart (its note written if the restart cut it
//!    off), or one someone else made. Nothing is created then.
//! 3. **Plan** the file on disk, now that every companion is there, and
//! 4. **fit** it to the measure the card showed, as the card is now
//!    ([`crate::modelinfo::refit::for_get`]): the largest context that fits,
//!    an aux row re-planned at it.
//! 5. **Create** the row, its id settled again against the names taken by
//!    now (D28), its note recorded on the get first and written after.
//! 6. **Done**, with the row. Nothing starts the model (D27): loading it is
//!    the Test button's job, or the first message's.
//!
//! The intent is the `model_gets` row. At boot, after the cut downloads went
//! on ([`super::hf_download::resume_at_boot`]), [`reattach_at_boot`] gives
//! every get still running its job again. A get's cancel stops the
//! downloads it started itself and no other get waits for; one it joined
//! (a projector another get or `hf_add` was fetching) goes on.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use lmgw_api_types::model_find::FitMeasure;
use lmgw_api_types::model_get::{
    GetWaiting, ModelGetDetail, ModelGetDone, ModelGetStage, WaitReason,
};

use super::{JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use crate::config::AuxKind;
use crate::erased::Erased;
use crate::modelinfo::refit;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::store::{self, ModelGetRow};

/// The kind's name on the jobs feed.
pub const JOB_KIND: &str = "model_get";

/// One live get per file.
pub fn job_key(repo: &str, file: &str) -> String {
    format!("model_get:{repo}/{file}")
}

/// Request payload of a `model_get` job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub get_id: i64,
}

/// Start the job of the get `get_id`, and record it on the get. A job of
/// the file already running is this get's when its input names it; one of
/// an earlier get of the file (ended, its job a moment from its end) is
/// waited out once and the start tried again, so a get never takes a job
/// that runs another.
pub async fn start(
    state: &SharedState,
    get_id: i64,
    repo: &str,
    file: &str,
) -> Result<i64, String> {
    let mut waited = false;
    loop {
        let spawned = super::spawn(
            state,
            JobKind::ModelGet,
            Some(job_key(repo, file)),
            format!("{repo}/{file}"),
            json!({ "get_id": get_id }),
        )
        .await?;
        let id = match spawned {
            Spawn::Started(id) => id,
            Spawn::AlreadyRunning(other) => {
                let input = store::get_job(&state.db, other)
                    .await
                    .map_err(|e| e.to_string())?
                    .and_then(|j| serde_json::from_str::<Input>(&j.input).ok());
                match input {
                    Some(i) if i.get_id == get_id => other,
                    _ if !waited && ended(state, other).await => {
                        waited = true;
                        continue;
                    }
                    _ => {
                        return Err(format!(
                            "an earlier get of {repo}/{file} still holds its job ({other}); \
                             Get it again in a moment"
                        ))
                    }
                }
            }
        };
        store::set_model_get_job(&state.db, get_id, id)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(id);
    }
}

/// Wait for the live job `id` to end; `false` when it isn't live here
/// (nothing will end it) or the feed closed.
async fn ended(state: &SharedState, id: i64) -> bool {
    use tokio::sync::broadcast::error::RecvError;
    let mut feed = state.telemetry.subscribe();
    if state.jobs.live_one(id).is_none() {
        return false;
    }
    loop {
        match feed.recv().await {
            Ok(crate::telemetry::Event::Jobs(_)) | Err(RecvError::Lagged(_)) => {
                if state.jobs.live_one(id).is_none() {
                    return true;
                }
            }
            Ok(_) => {}
            Err(RecvError::Closed) => return false,
        }
    }
}

/// The boot's pass (D122's Get-it half): every get still running gets its
/// job again; its downloads went on first. The ids of the gets started.
pub async fn reattach_at_boot(state: &SharedState) -> Vec<i64> {
    let rows = match store::running_model_gets(&state.db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("reading the gets the last shutdown cut: {e}");
            return Vec::new();
        }
    };
    let mut started = Vec::new();
    for row in rows {
        match start(state, row.id, &row.repo, &row.file).await {
            Ok(_) => started.push(row.id),
            Err(e) => {
                tracing::warn!("the get of {}/{} could not go on: {e}", row.repo, row.file);
                let _ = store::finish_model_get(&state.db, row.id, "failed", Some(&e)).await;
            }
        }
    }
    if !started.is_empty() {
        tracing::info!("{} get(s) the last shutdown cut go on", started.len());
    }
    started
}

/// How a get stopped before its row.
enum Stop {
    Canceled,
    Failed(String),
    /// The gateway is shutting down: the get stays `running`, and the next
    /// boot gives it its job again.
    ShuttingDown(String),
}

pub struct ModelGetExecutor;

impl JobExecutor for ModelGetExecutor {
    fn kind(&self) -> JobKind {
        JobKind::ModelGet
    }

    fn run<'a>(&'a self, ctx: JobCtx, input: Value) -> Erased<'a, Result<JobOutcome, String>> {
        Box::pin(async move {
            let input: Input =
                serde_json::from_value(input).map_err(|e| format!("model_get input: {e}"))?;
            let row = store::get_model_get(&ctx.state.db, input.get_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("get {} no longer exists", input.get_id))?;
            let db = ctx.state.db.clone();
            match run(&ctx, &row).await {
                Ok(done) => {
                    let _ = store::finish_model_get(&db, row.id, "done", None).await;
                    Ok(JobOutcome::Done(json!(done)))
                }
                Err(Stop::Canceled) => {
                    cancel_own_downloads(&ctx.state, row.id).await;
                    let _ = store::finish_model_get(&db, row.id, "canceled", None).await;
                    Ok(JobOutcome::Canceled)
                }
                Err(Stop::Failed(e)) => {
                    let _ = store::finish_model_get(&db, row.id, "failed", Some(&e)).await;
                    Err(e)
                }
                Err(Stop::ShuttingDown(e)) => Err(e),
            }
        })
    }
}

/// A cancelled get's downloads that are still on their way stop too: those
/// it started itself and no other running get waits for. One it joined
/// (another get's, or `hf_add`'s) goes on.
async fn cancel_own_downloads(state: &SharedState, get_id: i64) {
    let Ok(Some(row)) = store::get_model_get(&state.db, get_id).await else {
        return;
    };
    let others: Vec<i64> = store::running_model_gets(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|g| g.id != get_id)
        .flat_map(|g| g.download_ids)
        .collect();
    for id in row.queued_ids.iter().filter(|id| !others.contains(id)) {
        if let Ok(Some(d)) = store::get_hf_model(&state.db, *id).await {
            if matches!(d.status.as_str(), "queued" | "downloading") {
                let _ = super::hf_download::cancel_for_row(state, *id).await;
            }
        }
    }
}

/// The class a get sets up.
pub fn class_of(row: &ModelGetRow) -> Class {
    match row.target.as_str() {
        "aux" => Class::Aux,
        _ => Class::Chat,
    }
}

/// An aux get's kind, as its header said it (`embed` for chat, where it
/// names nothing).
pub fn kind_of(row: &ModelGetRow) -> AuxKind {
    row.kind
        .as_deref()
        .and_then(AuxKind::parse)
        .unwrap_or(AuxKind::Embed)
}

fn detail(row: &ModelGetRow, stage: ModelGetStage, waiting: Vec<GetWaiting>) -> Value {
    serde_json::to_value(ModelGetDetail {
        get_id: row.id,
        repo: row.repo.clone(),
        file: row.file.clone(),
        target: row.target.clone(),
        model_id: row.model_id.clone(),
        stage,
        downloads: row.download_ids.clone(),
        waiting,
    })
    .unwrap_or(Value::Null)
}

/// Steps 1 to 6 (module doc).
async fn run(ctx: &JobCtx, row: &ModelGetRow) -> Result<ModelGetDone, Stop> {
    wait_for_downloads(ctx, row).await?;
    if ctx.canceled() {
        return Err(Stop::Canceled);
    }
    ctx.progress_now(JobProgress {
        stage: "setting up".into(),
        detail: detail(row, ModelGetStage::SettingUp, Vec::new()),
        ..current(ctx)
    })
    .await;
    let done = set_up(&ctx.state, row).await.map_err(Stop::Failed)?;
    ctx.progress_now(JobProgress {
        stage: "ready".into(),
        detail: detail(row, ModelGetStage::Ready, Vec::new()),
        ..current(ctx)
    })
    .await;
    Ok(done)
}

/// The job's counters as they are, for a report that changes the stage.
fn current(ctx: &JobCtx) -> JobProgress {
    ctx.state
        .jobs
        .live_one(ctx.id)
        .map(|v| JobProgress {
            done: v.done,
            total: v.total,
            stage: v.stage,
            detail: v.detail,
        })
        .unwrap_or_default()
}

/// Step 1 (module doc).
async fn wait_for_downloads(ctx: &JobCtx, row: &ModelGetRow) -> Result<(), Stop> {
    use tokio::sync::broadcast::error::RecvError;
    let state = &ctx.state;
    if row.download_ids.is_empty() {
        // Every get has its weights among its downloads: none recorded is
        // a get cut before the op queued them.
        return Err(Stop::Failed(format!(
            "lmgw stopped before the downloads of {} were queued; Get it again",
            row.file
        )));
    }
    // Subscribed before the first read, so no frame between the two is
    // missed.
    let mut feed = state.telemetry.subscribe();
    let mut said: Option<(ModelGetStage, Vec<GetWaiting>)> = None;
    loop {
        if ctx.canceled() {
            return Err(Stop::Canceled);
        }
        let live = state.jobs.live_of_kind(JobKind::HfDownload);
        let (mut done, mut total, mut total_known) = (0u64, 0u64, true);
        let (mut all_done, mut moving) = (true, false);
        let mut waiting: Vec<GetWaiting> = Vec::new();
        for id in &row.download_ids {
            let d = store::get_hf_model(&state.db, *id)
                .await
                .map_err(|e| Stop::Failed(e.to_string()))?
                .ok_or_else(|| {
                    Stop::Failed(format!(
                        "a download of this get (entry {id}) was removed from the Downloads \
                         page; Get it again to fetch it"
                    ))
                })?;
            let size = d.size_bytes.map(|b| b.max(0) as u64);
            match d.status.as_str() {
                "done" | "update_available" => {
                    done += size.unwrap_or(0);
                    total += size.unwrap_or(0);
                }
                "failed" if d.error.as_deref() == Some(super::hf_download::CANCELED_NOTE) => {
                    return Err(Stop::Canceled)
                }
                "failed" => {
                    return Err(Stop::Failed(failed_download(
                        &d.file,
                        d.error.as_deref().unwrap_or("the download failed"),
                    )))
                }
                _ => {
                    all_done = false;
                    let key = super::hf_download::job_key(d.id);
                    match live.iter().find(|j| j.key.as_deref() == Some(key.as_str())) {
                        Some(j) => {
                            moving = true;
                            done += j.done;
                            match j.total.or(size) {
                                Some(t) => total += t,
                                None => total_known = false,
                            }
                        }
                        None => {
                            match size {
                                Some(t) => total += t,
                                None => total_known = false,
                            }
                            waiting.push(GetWaiting {
                                reason: wait_reason(d.error.as_deref()),
                                note: d.error.clone(),
                                file: d.file.clone(),
                            });
                        }
                    }
                }
            }
        }
        if all_done {
            return Ok(());
        }
        let (stage, label) = match (moving, waiting.is_empty()) {
            (false, false) => (ModelGetStage::Paused, "paused"),
            _ => (ModelGetStage::Downloading, "downloading"),
        };
        let report = JobProgress {
            done,
            total: total_known.then_some(total),
            stage: label.into(),
            detail: detail(row, stage, waiting.clone()),
        };
        // A new stage, or a new reason to wait, goes out at once: the
        // throttle would drop it when nothing reports after it (a paused
        // row sends no bytes).
        let now = Some((stage, waiting));
        match said == now {
            true => ctx.progress(report).await,
            false => ctx.progress_now(report).await,
        }
        said = now;
        // The next frame of the jobs feed, whatever moved.
        loop {
            match feed.recv().await {
                Ok(crate::telemetry::Event::Jobs(_)) | Err(RecvError::Lagged(_)) => break,
                Ok(_) => continue,
                Err(RecvError::Closed) => {
                    return Err(Stop::ShuttingDown(
                        "the gateway is shutting down; the get goes on at its next start".into(),
                    ))
                }
            }
        }
    }
}

/// Why a download row that nothing transfers waits, by the note it keeps
/// (WP7, D108): the boot's metered pause, none, or another.
pub fn wait_reason(note: Option<&str>) -> WaitReason {
    match note {
        Some(n) if n == super::hf_download::PAUSED_METERED_NOTE => WaitReason::Metered,
        Some(_) => WaitReason::Other,
        None => WaitReason::NotStarted,
    }
}

/// The sentence a get fails with when one of its downloads failed: the
/// download's own, whose "Retry" is the download's alone, said as Get it
/// again, which queues again what isn't here and sets the model up after.
fn failed_download(file: &str, why: &str) -> String {
    let said = why.replace("Retry", "Get it again");
    match said == why {
        true => format!(
            "{file} didn't download: {why}. Get it again to try once more: it fetches only \
             what isn't here yet"
        ),
        false => format!("{file} didn't download: {said}"),
    }
}

/// A row of the get's class that serves its file: its id, its model id,
/// its kind (aux), and whether it is the row this get created.
struct Found {
    row_id: i64,
    model_id: String,
    kind: Option<AuxKind>,
    own: bool,
}

/// Step 2's lookup (module doc): the row the get recorded, else a row of
/// the class with its model id on that path, else any row on that path.
fn found(snap: &crate::config::Snapshot, row: &ModelGetRow, rel: &str) -> Option<Found> {
    let serving: Vec<(i64, String, Option<AuxKind>)> = match class_of(row) {
        Class::Aux => snap
            .aux_models
            .iter()
            .filter(|m| m.gguf_path == rel)
            .map(|m| (m.id, m.model_id.clone(), Some(m.kind)))
            .collect(),
        _ => snap
            .local_models
            .iter()
            .filter(|m| m.gguf_path == rel)
            .map(|m| (m.id, m.model_id.clone(), None))
            .collect(),
    };
    let (row_id, model_id, kind) = serving
        .iter()
        .find(|r| Some(r.0) == row.row_id)
        .or_else(|| serving.iter().find(|r| r.1 == row.model_id))
        .or_else(|| serving.first())?
        .clone();
    // The id was free when the get took it (D28), so a row with it on this
    // path is the one the get created, even when a restart cut it off
    // before it recorded the row.
    let own = Some(row_id) == row.row_id || model_id == row.model_id;
    Some(Found {
        row_id,
        model_id,
        kind,
        own,
    })
}

/// Steps 2 to 5 (module doc).
async fn set_up(state: &SharedState, row: &ModelGetRow) -> Result<ModelGetDone, String> {
    let class = class_of(row);
    let rel = crate::hf::dest_rel_path(&row.repo, &row.file)?;
    let snap = state.snapshot();
    if let Some(f) = found(&snap, row, &rel) {
        return answer_found(state, &snap, row, f, &rel).await;
    }
    let measure = match row.measure.as_str() {
        "whole_card" => FitMeasure::WholeCard,
        _ => FitMeasure::Beside,
    };
    let fitted = refit::for_get(state, class, &rel, measure).await?;
    if fitted.plan["apply_ready"] == Value::Bool(false) {
        return Err(format!(
            "{} can't be set up as it is: {}",
            row.file,
            fitted.warnings.join("; ")
        ));
    }
    let kind = fitted.params.get("kind").and_then(Value::as_str);
    let aux_kind = kind
        .and_then(AuxKind::parse)
        .unwrap_or_else(|| kind_of(row));

    // The id again, against the names taken by now (D28).
    let snap = state.snapshot();
    let pt = crate::ops::PassThrough::of(state, &snap).await;
    let others: Vec<String> = store::running_model_gets(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|g| g.id != row.id && g.target == row.target)
        .map(|g| g.model_id)
        .collect();
    let model_id = crate::ops::free_model_id(&snap, &pt, class, aux_kind, &row.model_id, &others)?;
    if model_id != row.model_id {
        store::set_model_get_model_id(&state.db, row.id, &model_id)
            .await
            .map_err(|e| e.to_string())?;
    }
    // The note goes on the get before the row exists: a get cut between
    // the create and the note's write writes it when it re-attaches.
    let note_text = fitted
        .note
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| e.to_string())?;
    store::set_model_get_note(&state.db, row.id, note_text.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    let mut patch: Map<String, Value> = fitted.params.clone();
    patch.insert("action".into(), json!("create"));
    patch.insert("model_id".into(), json!(model_id));
    let mut warnings = fitted.warnings.clone();
    let row_id = match class {
        Class::Aux => {
            let saved =
                crate::ops::aux_model_set(state, crate::ops::patch_from_args(Some(patch))?).await?;
            warnings.extend(saved.warnings.unwrap_or_default());
            saved.id
        }
        _ => {
            let saved =
                crate::ops::local_model_set(state, crate::ops::patch_from_args(Some(patch))?)
                    .await?;
            warnings.extend(saved.warnings.unwrap_or_default());
            saved.id
        }
    };
    store::set_model_get_row(&state.db, row.id, row_id)
        .await
        .map_err(|e| e.to_string())?;
    refit::write_note(state, class, row_id, fitted.note.as_ref()).await?;
    let snap = state.snapshot();
    let public_name = crate::ops::public_name_of(&snap, class, aux_kind, &model_id);
    Ok(ModelGetDone {
        get_id: row.id,
        next_step: next_step(class, kind, &public_name),
        public_name,
        model_id,
        target: row.target.clone(),
        row_id,
        created: true,
        ctx_size: fitted.params.get("ctx_size").and_then(Value::as_u64),
        fit: Some(fitted.fit),
        note: fitted.note,
        warnings,
    })
}

/// Step 2's answer (module doc): the row `f` serves the file. The row this
/// get created keeps the note the get recorded for it, when a restart cut
/// the get off before it wrote it and the row's context is still the one
/// it records.
async fn answer_found(
    state: &SharedState,
    snap: &crate::config::Snapshot,
    row: &ModelGetRow,
    f: Found,
    rel: &str,
) -> Result<ModelGetDone, String> {
    let class = class_of(row);
    store::set_model_get_row(&state.db, row.id, f.row_id)
        .await
        .map_err(|e| e.to_string())?;
    let ctx_size = ctx_of(class, snap, f.row_id);
    let mut note = refit::note_of(state, class, f.row_id).await;
    if f.own && note.is_none() {
        let recorded: Option<lmgw_api_types::model_find::FitNote> = row
            .note
            .as_deref()
            .and_then(|t| serde_json::from_str(t).ok());
        if let Some(n) = recorded.filter(|n| Some(n.ctx) == ctx_size) {
            refit::write_note(state, class, f.row_id, Some(&n)).await?;
            note = Some(n);
        }
    }
    let kind = f.kind.unwrap_or_else(|| kind_of(row));
    let public_name = crate::ops::public_name_of(snap, class, kind, &f.model_id);
    let warning = match f.own {
        true => format!(
            "{} is the model this get created before lmgw stopped; it wasn't created again",
            f.model_id
        ),
        false => format!(
            "{} already serves {rel}, so no second model was created",
            f.model_id
        ),
    };
    Ok(ModelGetDone {
        get_id: row.id,
        next_step: next_step(class, f.kind.map(|k| k.as_str()), &public_name),
        public_name,
        model_id: f.model_id,
        target: row.target.clone(),
        row_id: f.row_id,
        created: false,
        ctx_size,
        note,
        fit: None,
        warnings: vec![warning],
    })
}

/// The context the `class` row `row_id` has.
fn ctx_of(class: Class, snap: &crate::config::Snapshot, row_id: i64) -> Option<u64> {
    let ctx = match class {
        Class::Aux => snap
            .aux_models
            .iter()
            .find(|m| m.id == row_id)
            .and_then(|m| m.ctx_size),
        _ => snap
            .local_models
            .iter()
            .find(|m| m.id == row_id)
            .and_then(|m| m.params.ctx_size),
    };
    ctx.and_then(|c| u64::try_from(c).ok())
}

/// What to do once the row is there (§6.3, D29).
fn next_step(class: Class, kind: Option<&str>, public_name: &str) -> String {
    match (class, kind) {
        (Class::Aux, Some("decision")) => format!(
            "Ready for decisions: '{public_name}' answers on /v1/systemone (the Decision lab \
             tries it); lmgw__local_model_test target=aux checks that it loads"
        ),
        (Class::Aux, _) => format!(
            "Ready for Knowledge: make a knowledge base with '{public_name}' as its embedder; \
             lmgw__local_model_test target=aux checks that it loads"
        ),
        _ => format!(
            "Say hello: open a chat with '{public_name}' — the first message loads it; \
             lmgw__local_model_test checks that it loads and answers"
        ),
    }
}
