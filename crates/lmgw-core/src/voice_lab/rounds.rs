//! The rounds on disk: `<data_dir>/voice-lab/rounds/<id>/`, a `round.json`
//! (the [`VoiceRound`] as answered) and one file per candidate
//! (`1.wav`, `2.wav`, …).
//!
//! A round is written as it renders, each candidate as its audio and a
//! small record of its own (`1.json`: seed, length, type), then the round's
//! own `round.json` — what was asked and who answered, without the
//! candidates — once its first candidate is in, and again when it ends. A
//! reader puts the candidates together from their records, so a candidate
//! costs one file's write however long the round runs, and a round the page
//! stopped half-way keeps what it made. Rounds stay until the owner
//! discards them (one, or all); the samples saved from them are copies in
//! the voice library and do not go with them.
//!
//! A round being rendered is **active** ([`Active`]): discarding it stops it
//! first and waits for it to end, then deletes it, so a candidate in flight
//! never lands in a directory that is gone, and nothing brings the
//! directory back (the writers never create one, [`super::write_atomic`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use lmgw_api_types::voice_lab::{VoiceCandidate, VoiceRound, VoiceRounds};
use tokio::sync::watch;

use super::LabFail;
use crate::proxy::{stop_pair, StopHandle, StopSignal};

const ROUND_FILE: &str = "round.json";

/// The rounds' directory under the lab's.
pub fn dir(lab_dir: &Path) -> PathBuf {
    lab_dir.join("rounds")
}

/// A new round id: its UTC time and a random tail, so ids sort by time and
/// two rounds in one millisecond do not meet (`20260101-120000-000-abcdef`).
pub fn new_id() -> String {
    let now = chrono::Utc::now();
    format!(
        "{}-{}",
        now.format("%Y%m%d-%H%M%S-%3f"),
        hex::encode(rand::random::<[u8; 3]>())
    )
}

/// An id is what [`new_id`] makes: ASCII letters, digits and `-`, so it is
/// a plain directory name and never a path.
pub fn check_id(id: &str) -> Result<&str, LabFail> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if ok {
        Ok(id)
    } else {
        Err(LabFail::bad(format!("'{id}' is not a round id")))
    }
}

/// Where a candidate plays: the lab route serving its file.
pub fn candidate_url(id: &str, index: u32) -> String {
    format!("/voice-lab/api/rounds/{id}/candidates/{index}")
}

/// A candidate's file name: its number and the extension of its type.
pub fn candidate_file(index: u32, mime: &str) -> String {
    let ext = match mime.split(';').next().unwrap_or_default().trim() {
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/flac" => "flac",
        "audio/aac" => "aac",
        _ => "bin",
    };
    format!("{index}.{ext}")
}

/// Round `id`'s directory, which must be a round id.
pub fn round_dir(lab_dir: &Path, id: &str) -> Result<PathBuf, LabFail> {
    Ok(dir(lab_dir).join(check_id(id)?))
}

/// Make round `id`'s directory: once, when the round's alias answered.
pub fn create(lab_dir: &Path, id: &str) -> Result<(), LabFail> {
    let path = round_dir(lab_dir, id)?;
    std::fs::create_dir_all(dir(lab_dir))
        .and_then(|()| std::fs::create_dir(&path))
        .map_err(|e| LabFail::internal(format!("creating {}: {e}", path.display())))
}

/// Write `round` as its directory's `round.json`, without its candidates
/// (each has its own record, [`write_candidate`]). A directory that is gone
/// is not brought back: the round was discarded.
pub fn write_round(lab_dir: &Path, round: &VoiceRound) -> Result<(), LabFail> {
    let path = round_dir(lab_dir, &round.id)?.join(ROUND_FILE);
    let head = VoiceRound {
        candidates: Vec::new(),
        ..round.clone()
    };
    let body = serde_json::to_vec_pretty(&head).map_err(|e| LabFail::internal(e.to_string()))?;
    super::write_atomic(&path, &body)
        .map_err(|e| LabFail::internal(format!("writing {}: {e}", path.display())))
}

/// Write candidate `c`'s audio into round `id`, then its record: a reader
/// sees a candidate only once its audio is whole.
pub fn write_candidate(
    lab_dir: &Path,
    id: &str,
    c: &VoiceCandidate,
    bytes: &[u8],
) -> Result<(), LabFail> {
    let dir = round_dir(lab_dir, id)?;
    let audio = dir.join(candidate_file(c.index, &c.mime));
    super::write_atomic(&audio, bytes)
        .map_err(|e| LabFail::internal(format!("writing {}: {e}", audio.display())))?;
    let record = dir.join(format!("{}.json", c.index));
    let body = serde_json::to_vec_pretty(c).map_err(|e| LabFail::internal(e.to_string()))?;
    super::write_atomic(&record, &body)
        .map_err(|e| LabFail::internal(format!("writing {}: {e}", record.display())))
}

/// The candidates' records in round directory `dir`, by index. A record
/// that cannot be read is skipped (and logged).
fn candidates_in(dir: &Path) -> Vec<VoiceCandidate> {
    let mut out: Vec<VoiceCandidate> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let index = name.strip_suffix(".json")?.parse::<u32>().ok()?;
                    let read = std::fs::read(e.path())
                        .map_err(|e| e.to_string())
                        .and_then(|b| {
                            serde_json::from_slice::<VoiceCandidate>(&b).map_err(|e| e.to_string())
                        });
                    match read {
                        Ok(c) if c.index == index => Some(c),
                        Ok(_) => None,
                        Err(why) => {
                            tracing::warn!(
                                "voice lab: candidate {} skipped: {why}",
                                e.path().display()
                            );
                            None
                        }
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|c| c.index);
    out
}

/// Round `id` as it was written, its candidates read from their records.
pub fn read_round(lab_dir: &Path, id: &str) -> Result<VoiceRound, LabFail> {
    let dir = round_dir(lab_dir, id)?;
    let path = dir.join(ROUND_FILE);
    let bytes = std::fs::read(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => LabFail::not_found(format!("no round '{id}'")),
        _ => LabFail::internal(format!("reading {}: {e}", path.display())),
    })?;
    let mut round: VoiceRound = serde_json::from_slice(&bytes)
        .map_err(|e| LabFail::internal(format!("round '{id}' is unreadable: {e}")))?;
    round.candidates = candidates_in(&dir);
    Ok(round)
}

/// Candidate `index` of round `id`: its bytes and type.
pub fn read_candidate(lab_dir: &Path, id: &str, index: u32) -> Result<(Vec<u8>, String), LabFail> {
    let (path, mime) = candidate_path(lab_dir, id, index)?;
    let bytes = std::fs::read(&path)
        .map_err(|e| LabFail::not_found(format!("reading {}: {e}", path.display())))?;
    Ok((bytes, mime))
}

/// Candidate `index` of round `id`: where its file is and its type, for a
/// reader that streams it.
pub fn candidate_path(lab_dir: &Path, id: &str, index: u32) -> Result<(PathBuf, String), LabFail> {
    let round = read_round(lab_dir, id)?;
    let c = round
        .candidates
        .iter()
        .find(|c| c.index == index)
        .ok_or_else(|| {
            LabFail::not_found(format!(
                "round '{id}' has no candidate {index} (it has {})",
                round.candidates.len()
            ))
        })?;
    let path = dir(lab_dir).join(id).join(candidate_file(index, &c.mime));
    Ok((path, c.mime.clone()))
}

/// Every round, newest first, and what their files take on disk — the
/// orphans' too, which [`discard_all`] removes. A directory without a
/// readable `round.json` is not a round the lab can show: an active round
/// before its first candidate (skipped), else an orphan (skipped, logged).
pub fn list(lab_dir: &Path) -> VoiceRounds {
    let root = dir(lab_dir);
    let mut rounds = Vec::new();
    let mut bytes = 0u64;
    for (name, path) in round_dirs(lab_dir) {
        bytes += dir_bytes(&path);
        match read_round(lab_dir, &name) {
            Ok(r) => rounds.push(r),
            Err(_) if is_active(lab_dir, &name) => {}
            Err(err) => tracing::warn!(
                "voice lab: {} is no round (Clear removes it): {err}",
                path.display()
            ),
        }
    }
    rounds.sort_by(|a, b| b.id.cmp(&a.id));
    VoiceRounds {
        rounds,
        bytes,
        dir: root.display().to_string(),
    }
}

/// The directories under the rounds' whose names are round ids.
fn round_dirs(lab_dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir(lab_dir)) else {
        return Vec::new();
    };
    rd.filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (check_id(&name).is_ok() && e.path().is_dir()).then(|| (name, e.path()))
        })
        .collect()
}

fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// Delete round `id`'s files. Not for an active round: [`discard`] stops
/// it first.
pub fn delete(lab_dir: &Path, id: &str) -> Result<(), LabFail> {
    let path = round_dir(lab_dir, id)?;
    match std::fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(LabFail::not_found(format!("no round '{id}'")))
        }
        Err(e) => Err(LabFail::internal(format!(
            "removing {}: {e}",
            path.display()
        ))),
    }
}

/// Discard round `id`: a round still rendering is stopped and waited for
/// first ([`Active`]), then its directory goes.
pub async fn discard(lab_dir: &Path, id: &str) -> Result<(), LabFail> {
    check_id(id)?;
    let was_active = stop_and_wait(lab_dir, id).await;
    let (lab, id) = (lab_dir.to_path_buf(), id.to_string());
    let gone = tokio::task::spawn_blocking(move || delete(&lab, &id))
        .await
        .map_err(|e| LabFail::internal(e.to_string()))?;
    match gone {
        // A round stopped before it made anything leaves nothing behind.
        Err(f) if was_active && f.status == axum::http::StatusCode::NOT_FOUND => Ok(()),
        other => other,
    }
}

/// Discard every round — active ones stopped and waited for first — and
/// every orphan directory [`list`] counts; how many directories went.
pub async fn discard_all(lab_dir: &Path) -> Result<usize, LabFail> {
    for id in active_ids(lab_dir) {
        stop_and_wait(lab_dir, &id).await;
    }
    let lab = lab_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let dirs = round_dirs(&lab);
        for (_, path) in &dirs {
            match std::fs::remove_dir_all(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(LabFail::internal(format!(
                        "removing {}: {e}",
                        path.display()
                    )))
                }
                _ => {}
            }
        }
        Ok(dirs.len())
    })
    .await
    .map_err(|e| LabFail::internal(e.to_string()))?
}

// ---------------------------------------------------------------------------
// The rounds being rendered
// ---------------------------------------------------------------------------

/// A round being rendered: how to stop it, and how to hear it ended.
pub struct Active {
    stop: StopHandle,
    /// Its sender is the [`Rendering`] guard: closed when the round ended.
    ended: watch::Receiver<()>,
}

/// The active rounds, by directory.
static ACTIVE: LazyLock<Mutex<HashMap<PathBuf, Arc<Active>>>> = LazyLock::new(Default::default);

fn active_map() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Arc<Active>>> {
    ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The renderer's half: while it lives the round is active; dropping it —
/// after the round's last write — ends that.
pub struct Rendering {
    key: PathBuf,
    active: Arc<Active>,
    _ended: watch::Sender<()>,
}

impl Rendering {
    /// A handle that raises the stop, for whoever waits on the renderer.
    pub fn stopper(&self) -> Arc<Active> {
        self.active.clone()
    }
}

impl Active {
    pub fn stop(&self) {
        self.stop.stop();
    }
}

impl Drop for Rendering {
    fn drop(&mut self) {
        let mut map = active_map();
        if map
            .get(&self.key)
            .is_some_and(|a| Arc::ptr_eq(a, &self.active))
        {
            map.remove(&self.key);
        }
    }
}

/// Mark round `id` active; the signal its rendering stops on.
pub fn start(lab_dir: &Path, id: &str) -> Result<(Rendering, StopSignal), LabFail> {
    let key = round_dir(lab_dir, id)?;
    let (stop, signal) = stop_pair();
    let (tx, ended) = watch::channel(());
    let active = Arc::new(Active { stop, ended });
    active_map().insert(key.clone(), active.clone());
    Ok((
        Rendering {
            key,
            active,
            _ended: tx,
        },
        signal,
    ))
}

/// Whether round `id` is being rendered.
pub fn is_active(lab_dir: &Path, id: &str) -> bool {
    round_dir(lab_dir, id).is_ok_and(|k| active_map().contains_key(&k))
}

fn active_ids(lab_dir: &Path) -> Vec<String> {
    let root = dir(lab_dir);
    active_map()
        .keys()
        .filter(|k| k.parent() == Some(root.as_path()))
        .filter_map(|k| k.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect()
}

/// Stop round `id` if it is active and wait until it ended; whether it was.
async fn stop_and_wait(lab_dir: &Path, id: &str) -> bool {
    let Ok(key) = round_dir(lab_dir, id) else {
        return false;
    };
    let Some(active) = active_map().get(&key).cloned() else {
        return false;
    };
    active.stop();
    let mut ended = active.ended.clone();
    // `Err` once the renderer's guard is gone: the round ended.
    while ended.changed().await.is_ok() {}
    true
}
