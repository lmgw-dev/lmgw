//! Recording a group and a run (labs-store design §1.1, §1.5): each one
//! `begin_write`, with the artifact files written before it and nothing slow
//! inside it. The new bytes are typed (sniffed, on the blocking pool) before
//! the artifacts lock is taken, so only the file writes run under it.

use std::path::Path;

use bytes::Bytes;
use lmgw_api_types::lab_store::{LabArtifactRole, LabCell, LabKind};
use sqlx::SqlitePool;

use super::artifacts;
use crate::store::{self, LabArtifactFile, LabArtifactRow, LabRunData};

/// One artifact a new group or run names.
#[derive(Debug)]
pub enum NewArtifact {
    /// New bytes: written to their file, typed by sniffing them.
    Bytes {
        name: String,
        bytes: Bytes,
        /// What the upload said it is, which stands in only where the bytes
        /// are of no kind lmgw recognises.
        declared_mime: Option<String>,
        /// The data URL's head as typed, when the bytes came as one: what a
        /// cell sends them back with.
        data_url_head: Option<String>,
    },
    /// New bytes already on the disk, spooled as they arrived (a speech
    /// stream): adopted under their digest, typed by sniffing their start.
    Spooled {
        name: String,
        file: artifacts::Spooled,
        declared_mime: Option<String>,
    },
    /// An artifact the store keeps already (a re-opened form's image): the
    /// new group gets a row of its own naming the same file, so deleting the
    /// old group does not take it.
    Existing { id: i64 },
}

/// A group to record.
#[derive(Debug)]
pub struct NewGroup {
    pub lab: LabKind,
    pub title: String,
    pub plan: Vec<LabCell>,
    /// The group's inputs, in order.
    pub inputs: Vec<NewArtifact>,
}

/// Why a record was refused or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// What the caller sent names something the store does not keep (an
    /// artifact deleted meanwhile): the caller's 400.
    Refused(String),
    /// The cell already has a run (`UNIQUE (group_id, cell)`): the caller's
    /// 409.
    Taken(String),
    /// The store failed: a full disk, a lock held past the busy timeout.
    Failed(String),
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(m) | Self::Taken(m) | Self::Failed(m) => f.write_str(m),
        }
    }
}

fn failed(e: impl std::fmt::Display) -> RecordError {
    RecordError::Failed(e.to_string())
}

/// A new artifact typed and ready for its file, or a re-used one.
enum Prepared {
    New {
        name: String,
        bytes: Bytes,
        mime: String,
        data_url_head: Option<String>,
    },
    Spooled {
        name: String,
        file: artifacts::Spooled,
        mime: String,
    },
    Existing(i64),
}

/// Type the new bytes among `items` (sniffed on the blocking pool), before
/// the artifacts lock is taken.
async fn prepare(items: Vec<NewArtifact>) -> Vec<Prepared> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(match item {
            NewArtifact::Bytes {
                name,
                bytes,
                declared_mime,
                data_url_head,
            } => {
                let mime = artifacts::mime_of(bytes.clone(), declared_mime.as_deref()).await;
                Prepared::New {
                    name,
                    bytes,
                    mime,
                    data_url_head,
                }
            }
            NewArtifact::Spooled {
                name,
                file,
                declared_mime,
            } => {
                let mime =
                    artifacts::mime_of_head(file.head.clone(), declared_mime.as_deref()).await;
                Prepared::Spooled { name, file, mime }
            }
            NewArtifact::Existing { id } => Prepared::Existing(id),
        });
    }
    out
}

/// Write the new bytes among `items` to their files (the caller holds the
/// artifacts lock): each one's file facts, the id for a re-used artifact.
/// A write that fails part way unlinks the files this call wrote before it
/// that no row names, then answers the failure.
async fn write_files(
    pool: &SqlitePool,
    data_dir: &Path,
    items: Vec<Prepared>,
) -> Result<Vec<Result<LabArtifactFile, i64>>, RecordError> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let written = match item {
            Prepared::New {
                name,
                bytes,
                mime,
                data_url_head,
            } => {
                let size = bytes.len() as i64;
                artifacts::write(data_dir, bytes)
                    .await
                    .map(|sha256| LabArtifactFile {
                        name,
                        mime,
                        size,
                        sha256,
                        data_url_head,
                    })
            }
            Prepared::Spooled { name, file, mime } => {
                let size = file.size as i64;
                artifacts::adopt(data_dir, file)
                    .await
                    .map(|sha256| LabArtifactFile {
                        name,
                        mime,
                        size,
                        sha256,
                        data_url_head: None,
                    })
            }
            Prepared::Existing(id) => {
                out.push(Err(id));
                continue;
            }
        };
        match written {
            Ok(f) => out.push(Ok(f)),
            Err(e) => {
                discard(pool, data_dir, &new_shas(&out)).await;
                return Err(RecordError::Failed(e));
            }
        }
    }
    Ok(out)
}

/// Record `g`: its input files first, then in one transaction the group, a
/// row per input, and the form and override `finish` writes once the
/// inputs have ids (it gets them in input order). The group's id and its
/// inputs' ids. A refusal or failure after the files were written unlinks
/// those of them no row names.
pub async fn group(
    pool: &SqlitePool,
    data_dir: &Path,
    g: NewGroup,
    finish: impl FnOnce(&[i64]) -> Result<(String, Option<String>), String>,
) -> Result<(i64, Vec<i64>), RecordError> {
    let plan = serde_json::to_string(&g.plan).map_err(failed)?;
    let inputs = prepare(g.inputs).await;
    let _held = artifacts::hold().await;
    let files = write_files(pool, data_dir, inputs).await?;
    let written = new_shas(&files);
    let recorded = insert_group(pool, g.lab, &g.title, &plan, files, finish).await;
    if recorded.is_err() {
        discard(pool, data_dir, &written).await;
    }
    recorded
}

async fn insert_group(
    pool: &SqlitePool,
    lab: LabKind,
    title: &str,
    plan: &str,
    files: Vec<Result<LabArtifactFile, i64>>,
    finish: impl FnOnce(&[i64]) -> Result<(String, Option<String>), String>,
) -> Result<(i64, Vec<i64>), RecordError> {
    let mut tx = store::begin_write(pool).await.map_err(failed)?;
    let id = store::insert_lab_group(&mut tx, lab, title, "{}", plan, None)
        .await
        .map_err(failed)?;
    let mut ids = Vec::with_capacity(files.len());
    for (ord, file) in files.into_iter().enumerate() {
        let file = match file {
            Ok(f) => f,
            Err(existing) => {
                let Some(row) = store::lab_artifact_in(&mut tx, existing)
                    .await
                    .map_err(failed)?
                else {
                    return Err(RecordError::Refused(format!(
                        "the image artifact {existing} is not kept any more: add the image again"
                    )));
                };
                LabArtifactFile {
                    name: row.name,
                    mime: row.mime,
                    size: row.size,
                    sha256: row.sha256,
                    data_url_head: row.data_url_head,
                }
            }
        };
        let aid = store::insert_lab_artifact(
            &mut tx,
            id,
            None,
            LabArtifactRole::Input,
            ord as i64,
            &file,
        )
        .await
        .map_err(failed)?;
        ids.push(aid);
    }
    let (form, override_text) = finish(&ids).map_err(RecordError::Refused)?;
    store::set_lab_group_texts(&mut tx, id, &form, override_text.as_deref())
        .await
        .map_err(failed)?;
    tx.commit().await.map_err(failed)?;
    Ok((id, ids))
}

/// The digests of the files [`write_files`] wrote.
fn new_shas(files: &[Result<LabArtifactFile, i64>]) -> Vec<String> {
    files
        .iter()
        .filter_map(|f| f.as_ref().ok().map(|f| f.sha256.clone()))
        .collect()
}

/// Unlink those of `shas` no row names: what a record that did not commit
/// wrote. Under the artifacts lock, which the caller holds. A failure to
/// tell leaves them, as orphans the usage read counts.
async fn discard(pool: &SqlitePool, data_dir: &Path, shas: &[String]) {
    if shas.is_empty() {
        return;
    }
    let unnamed = match pool.acquire().await {
        Ok(mut c) => store::unnamed_lab_shas(&mut c, shas).await,
        Err(e) => Err(e.into()),
    };
    match unnamed {
        Ok(gone) => artifacts::unlink(data_dir, &gone).await,
        Err(e) => tracing::warn!("lab store: leaving the files of a failed record: {e}"),
    }
}

/// A run to record, with what it produced.
#[derive(Debug)]
pub struct NewRun {
    pub data: LabRunData,
    /// The run's outputs, in order: always new bytes.
    pub outputs: Vec<NewArtifact>,
    /// The cell's failed run (no 2xx), if it has one, goes in the same
    /// transaction, and this one takes its place: a set run's Resume with
    /// `retry_failed`. A 2xx run stays and this one is
    /// [`RecordError::Taken`], as without it.
    pub replace_failed: bool,
}

/// The run's request and response as stored, written once its new
/// artifacts have ids (each `lab-artifact:<id>` where its bytes were).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTexts {
    pub request: String,
    pub response: Option<String>,
}

/// A recorded run: its id, its time, its output rows, the input rows it
/// brought, and its texts when [`run_with`]'s `finish` rewrote them.
#[derive(Debug, Clone)]
pub struct RecordedRun {
    pub id: i64,
    pub created_at: String,
    pub outputs: Vec<LabArtifactRow>,
    pub inputs: Vec<LabArtifactRow>,
    pub texts: Option<RunTexts>,
}

/// Record `r`: its output files first, then in one transaction the run and
/// a row per output (and with `replace_failed`, the failed run it replaces
/// goes, its files after the commit when no row names them). A failure
/// after the files were written unlinks those of them no row names; a cell
/// that has a run already is [`RecordError::Taken`].
pub async fn run(
    pool: &SqlitePool,
    data_dir: &Path,
    r: NewRun,
) -> Result<RecordedRun, RecordError> {
    run_with(pool, data_dir, r, Vec::new(), |_, _| None).await
}

/// [`run`], with the inputs the run brings to its group (an upload sent
/// with the request rather than with the group: input rows after the
/// group's own, in order) and `finish`, which gets the new inputs' and the
/// outputs' ids in order and may rewrite the stored texts to name them, in
/// the same transaction.
pub async fn run_with(
    pool: &SqlitePool,
    data_dir: &Path,
    r: NewRun,
    inputs: Vec<NewArtifact>,
    finish: impl FnOnce(&[i64], &[i64]) -> Option<RunTexts>,
) -> Result<RecordedRun, RecordError> {
    let outputs = prepare(r.outputs).await;
    let inputs = prepare(inputs).await;
    let _held = artifacts::hold().await;
    let out_files = write_files(pool, data_dir, outputs).await?;
    let in_files = match write_files(pool, data_dir, inputs).await {
        Ok(f) => f,
        Err(e) => {
            discard(pool, data_dir, &new_shas(&out_files)).await;
            return Err(e);
        }
    };
    let mut written = new_shas(&out_files);
    written.extend(new_shas(&in_files));
    let recorded = insert_run(pool, &r.data, out_files, in_files, r.replace_failed, finish).await;
    match recorded {
        Ok((run, replaced)) => {
            discard(pool, data_dir, &replaced).await;
            Ok(run)
        }
        Err(e) => {
            discard(pool, data_dir, &written).await;
            Err(e)
        }
    }
}

/// An inserted artifact row, as [`RecordedRun`] lists it.
fn inserted(
    id: i64,
    data: &LabRunData,
    run_id: Option<i64>,
    role: LabArtifactRole,
    ord: i64,
    file: LabArtifactFile,
    created_at: &str,
) -> LabArtifactRow {
    LabArtifactRow {
        id,
        group_id: data.group_id,
        run_id,
        role: role.as_str().into(),
        ord,
        name: file.name,
        mime: file.mime,
        size: file.size,
        sha256: file.sha256,
        data_url_head: file.data_url_head,
        created_at: created_at.to_string(),
    }
}

/// The run recorded, and the files of a failed run it replaced.
async fn insert_run(
    pool: &SqlitePool,
    data: &LabRunData,
    out_files: Vec<Result<LabArtifactFile, i64>>,
    in_files: Vec<Result<LabArtifactFile, i64>>,
    replace_failed: bool,
    finish: impl FnOnce(&[i64], &[i64]) -> Option<RunTexts>,
) -> Result<(RecordedRun, Vec<String>), RecordError> {
    let mut tx = store::begin_write(pool).await.map_err(failed)?;
    if !store::lab_group_in(&mut tx, data.group_id)
        .await
        .map_err(failed)?
    {
        return Err(RecordError::Failed(format!(
            "group {} was deleted before its run was in",
            data.group_id
        )));
    }
    let replaced = if replace_failed {
        store::delete_failed_lab_run(&mut tx, data.group_id, data.cell)
            .await
            .map_err(failed)?
    } else {
        Vec::new()
    };
    let Some((id, created_at)) = store::insert_lab_run(&mut tx, data).await.map_err(failed)? else {
        return Err(RecordError::Taken(format!(
            "cell {} of group {} has a run already; a re-ask is a new group",
            data.cell, data.group_id
        )));
    };
    let mut outputs = Vec::with_capacity(out_files.len());
    for (ord, file) in out_files.into_iter().enumerate() {
        let Ok(file) = file else {
            return Err(RecordError::Refused(
                "a run's outputs are new bytes, not a kept artifact".into(),
            ));
        };
        let role = LabArtifactRole::Output;
        let ord = ord as i64;
        let aid = store::insert_lab_artifact(&mut tx, data.group_id, Some(id), role, ord, &file)
            .await
            .map_err(failed)?;
        outputs.push(inserted(aid, data, Some(id), role, ord, file, &created_at));
    }
    let mut inputs = Vec::with_capacity(in_files.len());
    let first = if in_files.is_empty() {
        0
    } else {
        store::next_lab_input_ord(&mut tx, data.group_id)
            .await
            .map_err(failed)?
    };
    for (k, file) in in_files.into_iter().enumerate() {
        let file = match file {
            Ok(f) => f,
            Err(existing) => {
                let Some(row) = store::lab_artifact_in(&mut tx, existing)
                    .await
                    .map_err(failed)?
                else {
                    return Err(RecordError::Refused(format!(
                        "the artifact {existing} is not kept any more: send the file again"
                    )));
                };
                LabArtifactFile {
                    name: row.name,
                    mime: row.mime,
                    size: row.size,
                    sha256: row.sha256,
                    data_url_head: row.data_url_head,
                }
            }
        };
        let role = LabArtifactRole::Input;
        let ord = first + k as i64;
        let aid = store::insert_lab_artifact(&mut tx, data.group_id, None, role, ord, &file)
            .await
            .map_err(failed)?;
        inputs.push(inserted(aid, data, None, role, ord, file, &created_at));
    }
    let in_ids: Vec<i64> = inputs.iter().map(|a| a.id).collect();
    let out_ids: Vec<i64> = outputs.iter().map(|a| a.id).collect();
    let texts = finish(&in_ids, &out_ids);
    if let Some(t) = &texts {
        store::set_lab_run_texts(&mut tx, id, &t.request, t.response.as_deref())
            .await
            .map_err(failed)?;
    }
    tx.commit().await.map_err(failed)?;
    Ok((
        RecordedRun {
            id,
            created_at,
            outputs,
            inputs,
            texts,
        },
        replaced,
    ))
}
