//! `model_get` (find-a-model design §6, §9.1, WP5): Get it, server-side.
//!
//! The op checks the file is in the repo and loadable by its class, as
//! `hf_add` does, answers a row that already serves the file on disk
//! (D87), settles the model's id (D28, pass-through names included),
//! checks the disk's free space against the files `hf_add` would fetch less
//! those on disk (D125), then, a task of its own from here on, records the
//! intent (`model_gets`), queues the files and records them on it, and
//! starts the `model_get` job ([`crate::jobs::model_get`]), which waits,
//! plans, fits and creates the row and never starts it (D27). A second Get
//! it of the same file joins the first. [`model_get_status`] reads a get.
//!
//! "Fit again" (D26) is [`model_fit_again`], the model editor's.

use lmgw_api_types as dto;
use lmgw_api_types::model_find::FitMeasure;
use lmgw_api_types::model_get::{
    DiskRoom, ModelFitAgain, ModelFitAgainArgs, ModelGetArgs, ModelGetDetail, ModelGetDone,
    ModelGetStarted, ModelGetState, ModelGetStatus, ModelGetStatusArgs,
};

use crate::config::{AuxKind, Snapshot};
use crate::hf;
use crate::jobs::{model_get as job, JobKind};
use crate::modelinfo::refit;
use crate::runtime::Class;
use crate::state::SharedState;
use crate::store;

use super::candidate_alias_name_taken;

/// The id a get derives from the repo when none is given (D28): the repo's
/// name in lower case, less a trailing `-GGUF` (a generic rule for the
/// uploads that name themselves after the original with it).
pub fn id_of_repo(repo: &str) -> String {
    let name = repo.rsplit('/').next().unwrap_or(repo).to_ascii_lowercase();
    for suffix in ["-gguf", "_gguf", ".gguf"] {
        if let Some(stem) = name.strip_suffix(suffix).filter(|s| !s.is_empty()) {
            return stem.to_string();
        }
    }
    name
}

/// The name a row called `id` of `class` (and, for aux, `kind`) answers to
/// on `/v1`.
pub fn public_name_of(snap: &Snapshot, class: Class, kind: AuxKind, id: &str) -> String {
    match class {
        Class::Aux => snap.aux_name_for(kind, id),
        _ => snap.local_public_name(id),
    }
}

/// The names on `/v1` that no row or alias holds and a new row's public
/// name would still shadow (D28, revised): the
/// upstreams that pass their whole catalog through (`expose_all`). A local
/// row wins the name's resolve ([`Snapshot::resolve`]), so a row named
/// like one of their models would take that model's requests.
#[derive(Debug, Default)]
pub struct PassThrough {
    /// `(prefix, upstream)`: every name under `<prefix>/` goes to that
    /// upstream, listed or not.
    prefixes: Vec<(String, String)>,
    /// `(name, upstream)`: the models a bare upstream lists now.
    bare: Vec<(String, String)>,
}

impl PassThrough {
    /// The enabled pass-through upstreams of `snap`, a bare one's catalog
    /// read through the catalog cache. A catalog that can't be read is
    /// logged and counts as listing nothing.
    pub async fn of(state: &SharedState, snap: &Snapshot) -> Self {
        let mut out = Self::default();
        for u in snap
            .upstreams
            .values()
            .filter(|u| u.enabled && u.expose_all)
        {
            match u.prefix() {
                "" => match crate::catalog::upstream_models(state, u).await {
                    Ok(models) => out.bare.extend(
                        models
                            .into_iter()
                            .filter(|m| !snap.hidden_passthrough.contains(&(u.id, m.id.clone())))
                            .map(|m| (m.id, u.name.clone())),
                    ),
                    Err(e) => tracing::warn!(
                        "Get it couldn't read upstream {}'s models, so a name it serves isn't \
                         checked against them: {e}",
                        u.name
                    ),
                },
                p => out.prefixes.push((p.to_string(), u.name.clone())),
            }
        }
        out
    }

    /// The sentence of what the public name `name` would shadow under a
    /// prefix; a suffix doesn't get it out from under it.
    fn under_prefix(&self, name: &str) -> Option<String> {
        self.prefixes.iter().find_map(|(p, u)| {
            name.strip_prefix(p.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
                .then(|| {
                    format!(
                        "'{name}' is under '{p}/', the names upstream '{u}' passes through to \
                         its models: a local model of that name would take their requests"
                    )
                })
        })
    }

    /// The sentence of the model a bare upstream lists as `name`.
    fn listed(&self, name: &str) -> Option<String> {
        self.bare
            .iter()
            .find(|(m, _)| m.eq_ignore_ascii_case(name))
            .map(|(m, u)| {
                format!(
                    "'{m}' is a model upstream '{u}' passes through on /v1: a local model of \
                     that name would take its requests"
                )
            })
    }
}

/// Whether `id` is taken for a new row of `class` (D28): a row of that
/// class has it, a running get will create it (`pending`), its public name
/// is a name already served on `/v1` (a plain alias, a candidate alias, or
/// any class's row, case aside), or a pass-through upstream lists it.
fn taken(
    snap: &Snapshot,
    pt: &PassThrough,
    class: Class,
    kind: AuxKind,
    id: &str,
    pending: &[String],
) -> bool {
    let in_class = match class {
        Class::Aux => snap.aux_models.iter().any(|m| m.model_id == id),
        _ => snap.local_models.iter().any(|m| m.model_id == id),
    };
    let public = public_name_of(snap, class, kind, id);
    in_class
        || pending.iter().any(|p| p.eq_ignore_ascii_case(id))
        || candidate_alias_name_taken(snap, &public, None).is_some()
        || pt.listed(&public).is_some()
}

/// `base`, or the first of `base-2`, `base-3`, … that isn't taken (D28).
/// There are finitely many names, so one is free. A name under a
/// pass-through upstream's prefix is refused: no suffix gets out from
/// under it.
pub fn free_model_id(
    snap: &Snapshot,
    pt: &PassThrough,
    class: Class,
    kind: AuxKind,
    base: &str,
    pending: &[String],
) -> Result<String, String> {
    if let Some(why) = pt.under_prefix(&public_name_of(snap, class, kind, base)) {
        return Err(format!("{why}; choose another model_id"));
    }
    if !taken(snap, pt, class, kind, base, pending) {
        return Ok(base.to_string());
    }
    Ok((2u64..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !taken(snap, pt, class, kind, id, pending))
        .expect("an unbounded range has a free name"))
}

/// A `model_id` given to Get it that a pass-through upstream serves now:
/// the refusal names what it would shadow (D28, revised).
fn given_shadows(
    snap: &Snapshot,
    pt: &PassThrough,
    class: Class,
    kind: AuxKind,
    id: &str,
) -> Option<String> {
    let public = public_name_of(snap, class, kind, id);
    pt.under_prefix(&public)
        .or_else(|| pt.listed(&public))
        .map(|why| format!("{why}; choose another model_id"))
}

/// The class and (for aux) the kind of `file` in `repo`: `target` as asked,
/// the header's otherwise; an aux file's kind is its header's either way.
async fn class_of(
    state: &SharedState,
    repo: &str,
    file: &hf::HfFile,
    target: Option<&str>,
) -> Result<(Class, AuxKind), String> {
    let asked = super::parse_class_target(target)?;
    if matches!(asked, Some(Class::Image | Class::Audio)) {
        return Err(format!(
            "Get it sets up a chat or an aux model from a GGUF; the {} class has its own \
             catalog (lmgw__image_recipes, lmgw__audio_catalog)",
            asked.map(Class::as_str).unwrap_or_default()
        ));
    }
    if asked == Some(Class::Chat) {
        return Ok((Class::Chat, AuxKind::Embed));
    }
    let header = hf::remote_header(state, repo, file).await?;
    let (serve_as, kind) = crate::modelinfo::class_for(&header.summary);
    let class = match asked {
        Some(c) => c,
        None if serve_as == "aux" => Class::Aux,
        None => Class::Chat,
    };
    Ok((class, kind.unwrap_or(AuxKind::Embed)))
}

/// `model_get` (module doc).
pub async fn model_get(state: &SharedState, a: ModelGetArgs) -> Result<ModelGetStarted, String> {
    let repo = a.repo.trim().to_string();
    let file = a.file.trim().to_string();
    hf::validate_repo(&repo)?;
    if file.is_empty() {
        return Err("name the weights file in the repo (file)".into());
    }
    let measure = a.measure.unwrap_or(FitMeasure::Beside);
    // A get of this file under way: this is that one.
    if let Some(row) = store::running_model_get(&state.db, &repo, &file)
        .await
        .map_err(|e| e.to_string())?
    {
        return joined(state, &row).await;
    }
    let snap = state.snapshot();
    let listing = hf::list_repo_files(&state.http, &snap.settings.hf_token, &repo).await?;
    let weights = listing
        .iter()
        .find(|f| f.path == file)
        .ok_or_else(|| format!("{file} not found in {repo} — call lmgw__hf_repo to list it"))?;
    if !file.to_ascii_lowercase().ends_with(".gguf") {
        return Err(format!(
            "{file} is not a GGUF: Get it sets up a llama.cpp model from a .gguf file"
        ));
    }
    let (class, kind) = class_of(state, &repo, weights, a.target.as_deref()).await?;
    let target = class.as_str();
    let dir = hf::models_dir_to_write(state, target)?;
    if super::huggingface::classify_repo_file(&file, target) != "weights" {
        return Err(format!(
            "{file} looks like a {} file, not model weights: name the weights file, and its \
             projector and drafter come with it",
            super::huggingface::classify_repo_file(&file, target)
        ));
    }

    // A row that serves the file already is the answer (D87), when the
    // file is on disk: nothing is queued and no get runs.
    let rel = hf::dest_rel_path(&repo, &file)?;
    if std::path::Path::new(&dir).join(&rel).is_file() {
        if let Some((row_id, model_id, row_kind)) = serving(&snap, class, &rel) {
            let public_name = public_name_of(&snap, class, row_kind.unwrap_or(kind), &model_id);
            return Ok(ModelGetStarted {
                row_id: Some(row_id),
                message: format!(
                    "'{public_name}' already serves {file}: nothing was downloaded or created, \
                     and nothing is started"
                ),
                model_id,
                public_name,
                target: target.to_string(),
                measure,
                repo,
                file,
                ..ModelGetStarted::default()
            });
        }
    }

    // The id (D28): the one asked for, else the repo's; taken, suffixed. A
    // given one a pass-through upstream serves is refused.
    let pt = PassThrough::of(state, &snap).await;
    let pending = store::running_model_get_ids(&state.db, target)
        .await
        .map_err(|e| e.to_string())?;
    let base = match a.model_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => {
            if let Some(why) = given_shadows(&snap, &pt, class, kind, id) {
                return Err(why);
            }
            id.to_string()
        }
        _ => id_of_repo(&repo),
    };
    let model_id = free_model_id(&snap, &pt, class, kind, &base, &pending)?;

    // The files `hf_add` fetches; those already on disk aren't fetched again.
    let (wanted, companions) = super::huggingface::with_companions(&listing, &file, target);
    let tracked = store::list_hf_models(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let mut fetch: Vec<String> = Vec::new();
    let mut here: Vec<dto::QueuedDownload> = Vec::new();
    let mut disk = DiskRoom::default();
    for w in &wanted {
        let parts = hf::expand_parts(w, &listing);
        let mut rows = Vec::new();
        for p in &parts {
            let on_disk = tracked.iter().find(|r| {
                r.repo == repo
                    && r.file == *p
                    && r.target == target
                    && matches!(r.status.as_str(), "done" | "update_available")
                    && std::path::Path::new(&dir).join(&r.dest_path).is_file()
            });
            rows.push(on_disk);
        }
        if rows.iter().all(Option::is_some) {
            here.extend(rows.into_iter().flatten().map(|r| dto::QueuedDownload {
                id: r.id,
                file: r.file.clone(),
                dest_path: r.dest_path.clone(),
                status: r.status.clone(),
            }));
            continue;
        }
        for p in &parts {
            match listing.iter().find(|f| f.path == *p).and_then(|f| f.size) {
                Some(size) => {
                    let kept = kept_part(&dir, &repo, p).await;
                    disk.needs_bytes += size.saturating_sub(kept);
                }
                None => disk.notes.push(format!(
                    "{p}'s size isn't in the repo's listing, so it isn't counted"
                )),
            }
        }
        fetch.push(w.clone());
    }
    disk.free_bytes = crate::backends::paths::free_bytes(std::path::Path::new(&dir));
    disk.short = disk.free_bytes.is_some_and(|f| f < disk.needs_bytes);
    if disk.short {
        disk.sentence = Some(format!(
            "Needs {} on the disk, {} is free",
            hf::fmt_bytes(disk.needs_bytes),
            hf::fmt_bytes(disk.free_bytes.unwrap_or(0))
        ));
    }
    if disk.free_bytes.is_none() {
        disk.notes.push(format!(
            "the free space of {dir} couldn't be read, so lmgw can't tell whether the files fit"
        ));
    }
    let public_name = public_name_of(&snap, class, kind, &model_id);
    let prepared = Prepared {
        started: ModelGetStarted {
            model_id,
            public_name,
            target: target.to_string(),
            measure,
            repo,
            file,
            companions,
            disk,
            ..ModelGetStarted::default()
        },
        kind: (class == Class::Aux).then_some(kind),
        fetch,
        here,
    };
    // From the intent's write on, a task of its own (see
    // [`crate::detached::detached`]): a dropped request would leave
    // downloads with no get, or a get with no job.
    let st = state.clone();
    crate::detached::detached("starting the get", async move {
        record_and_start(&st, prepared).await
    })
    .await
    .map_err(|r| r.message)?
}

/// What [`model_get`] settled before it writes anything.
struct Prepared {
    /// The answer so far: the id, the class, the companions, the disk.
    started: ModelGetStarted,
    kind: Option<AuxKind>,
    /// The files to queue, as `hf_add` names them.
    fetch: Vec<String>,
    /// The files already on disk, with their rows.
    here: Vec<dto::QueuedDownload>,
}

/// [`model_get`] from the intent's write on: the `model_gets` row first,
/// then the downloads it waits for, recorded on it, then its job.
async fn record_and_start(state: &SharedState, p: Prepared) -> Result<ModelGetStarted, String> {
    let Prepared {
        mut started,
        kind,
        fetch,
        here,
    } = p;
    let inserted = store::insert_model_get(
        &state.db,
        &store::NewModelGet {
            repo: &started.repo,
            file: &started.file,
            target: &started.target,
            measure: measure_key(started.measure),
            model_id: &started.model_id,
            kind: kind.map(|k| k.as_str()),
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    let Some(get_id) = inserted else {
        // Another Get it of the file won the row meanwhile: join it.
        let row = store::running_model_get(&state.db, &started.repo, &started.file)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "a get of {}/{} was under way and ended meanwhile",
                    started.repo, started.file
                )
            })?;
        return joined(state, &row).await;
    };
    let failed = |e: String| async move {
        let _ = store::finish_model_get(&state.db, get_id, "failed", Some(&e)).await;
        e
    };
    let queued = match fetch.is_empty() {
        true => Vec::new(),
        false => {
            match crate::web::hf::queue_files_owned(state, &started.repo, &fetch, &started.target)
                .await
            {
                Ok(q) => q,
                Err(e) => return Err(failed(e).await),
            }
        }
    };
    let own: Vec<i64> = queued
        .iter()
        .filter(|(_, s)| *s)
        .map(|(d, _)| d.id)
        .collect();
    let mut downloads: Vec<dto::QueuedDownload> = queued.into_iter().map(|(d, _)| d).collect();
    downloads.extend(here);
    let ids: Vec<i64> = downloads.iter().map(|d| d.id).collect();
    if let Err(e) = store::set_model_get_downloads(&state.db, get_id, &ids, &own).await {
        return Err(failed(e.to_string()).await);
    }
    let engine = crate::web::hf::engine::fetch(state, &started.target).await;
    if engine.is_some() {
        started.disk.notes.push(
            "the engine image Podman keeps isn't counted: its storage can be on another disk"
                .into(),
        );
    }
    let job_id = match job::start(state, get_id, &started.repo, &started.file).await {
        Ok(id) => id,
        Err(e) => return Err(failed(e).await),
    };
    started.message = format!(
        "Getting {}: {} file(s), set up as '{}' once they are here; the model is not started. \
         lmgw__model_get_status get_id={get_id} says how it stands and, at the end, the row it \
         made (lmgw__hf_downloads with ids = the downloads' ids for each file's bytes).",
        started.file,
        downloads.len(),
        started.public_name
    );
    started.job_id = Some(job_id);
    started.get_id = Some(get_id);
    started.downloads = downloads;
    started.engine = engine;
    Ok(started)
}

/// The row of `class` that serves `rel`, if one does: its id, its model
/// id and (aux) its kind.
fn serving(snap: &Snapshot, class: Class, rel: &str) -> Option<(i64, String, Option<AuxKind>)> {
    match class {
        Class::Aux => snap
            .aux_models
            .iter()
            .find(|m| m.gguf_path == rel)
            .map(|m| (m.id, m.model_id.clone(), Some(m.kind))),
        _ => snap
            .local_models
            .iter()
            .find(|m| m.gguf_path == rel)
            .map(|m| (m.id, m.model_id.clone(), None)),
    }
}

/// The bytes a kept `.part` of `repo`'s `file` holds.
async fn kept_part(dir: &str, repo: &str, file: &str) -> u64 {
    let Ok(rel) = hf::dest_rel_path(repo, file) else {
        return 0;
    };
    let part = std::path::Path::new(dir).join(format!("{rel}.part"));
    tokio::fs::metadata(part)
        .await
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .unwrap_or(0)
}

/// `whole_card | beside`, as the row stores a measure.
pub fn measure_key(m: FitMeasure) -> &'static str {
    match m {
        FitMeasure::WholeCard => "whole_card",
        FitMeasure::Beside => "beside",
    }
}

/// The running get `row` as a fresh `model_get` answers it.
async fn joined(state: &SharedState, row: &store::ModelGetRow) -> Result<ModelGetStarted, String> {
    let key = job::job_key(&row.repo, &row.file);
    let job_id = state
        .jobs
        .live_by_key(JobKind::ModelGet, &key)
        .map(|j| j.id)
        .or(row.job_id);
    let snap = state.snapshot();
    let class = job::class_of(row);
    let mut downloads = Vec::new();
    for id in &row.download_ids {
        if let Some(d) = store::get_hf_model(&state.db, *id)
            .await
            .map_err(|e| e.to_string())?
        {
            downloads.push(dto::QueuedDownload {
                id: d.id,
                file: d.file,
                dest_path: d.dest_path,
                status: d.status,
            });
        }
    }
    let public_name = public_name_of(&snap, class, job::kind_of(row), &row.model_id);
    Ok(ModelGetStarted {
        job_id,
        get_id: Some(row.id),
        model_id: row.model_id.clone(),
        public_name,
        target: row.target.clone(),
        measure: match row.measure.as_str() {
            "whole_card" => FitMeasure::WholeCard,
            _ => FitMeasure::Beside,
        },
        repo: row.repo.clone(),
        file: row.file.clone(),
        downloads,
        joined: true,
        message: format!(
            "{} was already being got (get {}); this is that get, and \
             lmgw__model_get_status get_id={} says how it stands",
            row.file, row.id, row.id
        ),
        ..ModelGetStarted::default()
    })
}

/// `model_get_status`: one get as it stands, from its row, its job and
/// its download rows (find-a-model design §9.1).
pub async fn model_get_status(
    state: &SharedState,
    a: ModelGetStatusArgs,
) -> Result<ModelGetStatus, String> {
    let row = store::get_model_get(&state.db, a.get_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no get {} — model_get answers a get_id", a.get_id))?;
    let snap = state.snapshot();
    let class = job::class_of(&row);
    let mut out = ModelGetStatus {
        get_id: row.id,
        public_name: public_name_of(&snap, class, job::kind_of(&row), &row.model_id),
        repo: row.repo.clone(),
        file: row.file.clone(),
        target: row.target.clone(),
        model_id: row.model_id.clone(),
        state: match row.status.as_str() {
            "done" => ModelGetState::Done,
            "failed" => ModelGetState::Failed,
            "canceled" => ModelGetState::Canceled,
            _ => ModelGetState::Running,
        },
        error: row.error.clone(),
        row_id: row.row_id,
        job_id: row.job_id,
        ..ModelGetStatus::default()
    };
    for id in &row.download_ids {
        if let Some(d) = store::get_hf_model(&state.db, *id)
            .await
            .map_err(|e| e.to_string())?
        {
            out.downloads.push(dto::QueuedDownload {
                id: d.id,
                file: d.file,
                dest_path: d.dest_path,
                status: d.status,
            });
        }
    }
    let Some(job_id) = row.job_id else {
        return Ok(out);
    };
    if let Some(live) = state.jobs.live_one(job_id) {
        if out.state == ModelGetState::Running {
            if let Ok(d) = serde_json::from_value::<ModelGetDetail>(live.detail.clone()) {
                out.stage = Some(d.stage);
                out.waiting = d.waiting;
            }
            out.done_bytes = live.done;
            out.total_bytes = live.total;
        }
        return Ok(out);
    }
    if out.state == ModelGetState::Done {
        let job = store::get_job(&state.db, job_id)
            .await
            .map_err(|e| e.to_string())?;
        out.result = job
            .and_then(|j| j.result)
            .and_then(|r| serde_json::from_str::<ModelGetDone>(&r).ok())
            .filter(|d| d.get_id == row.id);
    }
    Ok(out)
}

/// "Fit again" (D26): the fit of a row's own parameters on the card now,
/// and with `confirm` the save of what it changes (the context, and for an
/// aux row an ubatch and a batch tied to it), with the new note. A confirm
/// saves the context it names when that still fits the card now and the
/// row is as it was ([`refit::Refit::confirmable`]); else nothing is
/// written and the answer is `outdated`.
pub async fn model_fit_again(
    state: &SharedState,
    a: ModelFitAgainArgs,
) -> Result<ModelFitAgain, String> {
    let class = match super::parse_class_target(Some(&a.target))? {
        Some(c @ (Class::Chat | Class::Aux)) => c,
        _ => return Err("Fit again fits a chat or an aux row (target chat or aux)".into()),
    };
    let model_id = a.model_id.trim();
    let refit = refit::for_row(state, class, model_id).await?;
    let mut answer = ModelFitAgain {
        model_id: model_id.to_string(),
        target: class.as_str().to_string(),
        measure: refit.measure,
        note: refit.current.clone(),
        fit: refit.fit.clone(),
        changes: refit.changes.clone(),
        ..ModelFitAgain::default()
    };
    let Some(confirmed) = a.confirm else {
        answer.sentence = sentence(&refit);
        return Ok(answer);
    };
    let (changes, note) = match refit.confirmable(&confirmed) {
        Ok(saved) => saved,
        Err(why) => {
            answer.outdated = true;
            let what = match why {
                refit::Outdated::RowChanged => "The model's settings changed since",
                refit::Outdated::NoLongerFits => {
                    "The graphics card changed since, and that context no longer fits"
                }
            };
            answer.sentence = format!("{what}: nothing was saved. {}", sentence(&refit));
            return Ok(answer);
        }
    };
    // The row's update and its note, a task of their own (see
    // [`crate::detached::detached`]): a dropped request would leave the
    // context changed with no note, or the old note on a new context.
    let st = state.clone();
    let (row_id, to_save) = (refit.row_id, changes.clone());
    let note_saved = note.clone();
    crate::detached::detached("saving Fit again", async move {
        let state = &st;
        if !to_save.is_empty() {
            let mut patch = serde_json::Map::new();
            patch.insert("action".into(), "update".into());
            patch.insert("id".into(), row_id.into());
            for c in &to_save {
                patch.insert(c.key.clone(), c.to.into());
            }
            match class {
                Class::Aux => {
                    super::aux_model_set(state, super::patch_from_args(Some(patch))?).await?;
                }
                _ => {
                    super::local_model_set(state, super::patch_from_args(Some(patch))?).await?;
                }
            }
        }
        refit::write_note(state, class, row_id, note_saved.as_ref()).await
    })
    .await
    .map_err(|r| r.message)??;
    let ctx = changes
        .iter()
        .find(|c| c.key == "ctx_size")
        .and_then(|c| c.to)
        .or_else(|| {
            refit
                .params
                .get("ctx_size")
                .and_then(serde_json::Value::as_u64)
        });
    answer.note = note.clone();
    answer.changes = changes;
    answer.saved = true;
    answer.sentence = match (ctx, note.is_none()) {
        (Some(ctx), true) => format!(
            "Saved: the model now remembers {} tokens, the model's whole context.",
            crate::modelinfo::pick::tokens(ctx)
        ),
        (Some(ctx), false) => format!(
            "Saved: the model now remembers {} tokens.",
            crate::modelinfo::pick::tokens(ctx)
        ),
        (None, _) => "Saved.".into(),
    };
    Ok(answer)
}

/// What "Fit again" says.
fn sentence(r: &refit::Refit) -> String {
    use lmgw_api_types::model_find::FitMark;
    let ctx = r.fit.ctx.map(crate::modelinfo::pick::tokens);
    let measure = match r.measure {
        FitMeasure::WholeCard => "the whole graphics card",
        FitMeasure::Beside => "the graphics card beside what runs now",
    };
    match (r.fit.mark, ctx) {
        (FitMark::Fits | FitMark::FitsShorter, Some(ctx)) if r.changes.is_empty() => format!(
            "The model already takes the largest conversation that fits {measure}: {ctx} tokens."
        ),
        (FitMark::Fits | FitMark::FitsShorter, Some(ctx)) => {
            let whole = match r.fit.mark {
                FitMark::Fits => ", the model's whole context",
                _ => "",
            };
            format!("On {measure} the model can remember {ctx} tokens{whole}. Save to set it.")
        }
        (FitMark::NoCard, _) => "lmgw can't measure the graphics card yet, so there is \
                                nothing to fit to: no reading of the card's memory and no GPU \
                                memory budget is set."
            .into(),
        _ => format!(
            "The model doesn't fit {measure}, not even a short conversation; nothing changes."
        ),
    }
}
