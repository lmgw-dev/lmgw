//! A job's stored results (document-conversion design §4.4): each file's
//! entry — the same JSON as its entry in a sync answer, its converted text
//! included — at `<data dir>/convert/<job id>/<index>/entry.json`. A file's
//! pictures whose bytes the answer carries (`referenced`, or `embedded`
//! with `output: json`) are files beside it, `<index>/artifacts/<name>`
//! under the name the answer gives them, and the stored entry holds their
//! names only; reading the entry puts the bytes back. They count in the
//! "Stored conversions" figure ([`usage`]) and go with the job.
//!
//! They are document content, kept in the data dir only: deleted with their
//! job (cancel, retention, a docling client's fetched result), and a
//! directory without a job row is swept ([`super::sweep`]).

use std::path::{Path, PathBuf};

use lmgw_api_types::documents::convert::{ConvertedDocument, ErrorCode};

use super::secure;
use crate::extract::convert::batch::failed_file;
use crate::extract::convert::file::{Done, Input};
use crate::extract::convert::pictures::is_artifact;

/// The directory of a job's kept uploads, beside the files' own
/// (numbered) directories.
const INPUT_DIR: &str = "input";

/// Where every job's results live.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("convert")
}

/// A job id as lmgw makes them (`cj_` and hex), so no id from a path can
/// name a directory outside [`dir`].
pub fn is_job_id(id: &str) -> bool {
    id.strip_prefix("cj_")
        .is_some_and(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn job_dir(data_dir: &Path, id: &str) -> Option<PathBuf> {
    is_job_id(id).then(|| dir(data_dir).join(id))
}

fn entry_path(data_dir: &Path, id: &str, index: u32) -> Option<PathBuf> {
    job_dir(data_dir, id).map(|d| d.join(index.to_string()).join("entry.json"))
}

/// Store file `doc.index`'s entry of job `id`. Its pictures' base64 is
/// decoded and the entry serialised on a blocking thread (both as large as
/// the answer), never on the async runtime.
pub async fn write(data_dir: &Path, id: &str, doc: ConvertedDocument) -> std::io::Result<()> {
    write_within(data_dir, id, doc, |_| true, &Landed::default())
        .await
        .map(|_| ())
}

/// What a [`write_within`] has put on disk so far: each file counted once
/// its write has finished (security round A verification, N-a). A write cut
/// off midway by its job's cancel leaves the file it was writing partly on
/// disk and uncounted; the cancel deletes the job's directory with it, and
/// what that deletion takes off the storage count can then pass what was
/// counted by at most that part, until the hourly resync.
#[derive(Debug, Default)]
pub struct Landed(std::sync::atomic::AtomicU64);

impl Landed {
    /// The bytes counted.
    pub fn bytes(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn add(&self, n: u64) {
        self.0.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The error [`write_within`] answers when `room` refused the entry: the
/// jobs' storage bound (`super::quota`).
#[derive(Debug)]
pub struct NoRoom;

impl std::fmt::Display for NoRoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the conversion jobs' storage bound is reached")
    }
}

impl std::error::Error for NoRoom {}

/// Whether `e` is [`NoRoom`].
pub fn is_no_room(e: &std::io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<NoRoom>())
}

/// [`write`] once `room` has allowed the bytes it takes (its entry and its
/// pictures, known once serialised); the bytes written. `landed` counts
/// what it has put on disk so far ([`Landed`]).
pub async fn write_within(
    data_dir: &Path,
    id: &str,
    doc: ConvertedDocument,
    room: impl FnOnce(u64) -> bool,
    landed: &Landed,
) -> std::io::Result<u64> {
    let path =
        entry_path(data_dir, id, doc.index).ok_or_else(|| std::io::Error::other("not a job id"))?;
    let Some(dir) = path.parent().map(Path::to_path_buf) else {
        return Err(std::io::Error::other("not a job id"));
    };
    secure::create_dir_all(&dir).await?;
    let (bytes, files) = tokio::task::spawn_blocking(move || {
        let mut doc = doc;
        let files = pictures_out(&mut doc)?;
        let bytes = serde_json::to_vec(&doc).map_err(std::io::Error::other)?;
        Ok::<_, std::io::Error>((bytes, files))
    })
    .await
    .map_err(std::io::Error::other)??;
    let size = files.iter().fold(bytes.len() as u64, |n, (_, d)| {
        n.saturating_add(d.len() as u64)
    });
    if !room(size) {
        return Err(std::io::Error::other(NoRoom));
    }
    for (name, data) in files {
        let at = dir.join(name);
        if let Some(parent) = at.parent() {
            secure::create_dir_all(parent).await?;
        }
        secure::write(&at, &data).await?;
        landed.add(data.len() as u64);
    }
    // Written beside and renamed, so a reader never sees half an entry.
    let tmp = path.with_extension("json.part");
    let written = match secure::write(&tmp, &bytes).await {
        Ok(()) => tokio::fs::rename(&tmp, &path).await,
        Err(e) => Err(e),
    };
    if let Err(e) = &written {
        let _ = tokio::fs::remove_file(&tmp).await;
        if secure::disk_full(e) {
            tracing::warn!(
                "the disk holding the data dir is full: a conversion job's result was not \
                 stored (Settings › Retention shows how much stored jobs take)"
            );
        }
    }
    if written.is_ok() {
        landed.add(bytes.len() as u64);
    }
    written.map(|()| size)
}

fn input_path(data_dir: &Path, id: &str, index: u32) -> Option<PathBuf> {
    job_dir(data_dir, id).map(|d| d.join(INPUT_DIR).join(index.to_string()))
}

/// Keep file `index`'s upload on disk for job `id` until its file is
/// converted: a queued job holds no upload in memory.
pub async fn write_input(
    data_dir: &Path,
    id: &str,
    index: u32,
    bytes: &[u8],
) -> std::io::Result<()> {
    let path =
        input_path(data_dir, id, index).ok_or_else(|| std::io::Error::other("not a job id"))?;
    if let Some(parent) = path.parent() {
        secure::create_dir_all(parent).await?;
    }
    let written = secure::write(&path, bytes).await;
    if let Err(e) = &written {
        if secure::disk_full(e) {
            tracing::warn!(
                "the disk holding the data dir is full: a conversion job's upload was not \
                 kept (Settings › Retention shows how much stored jobs take)"
            );
        }
    }
    written
}

/// File `index`'s kept upload of job `id`.
pub async fn read_input(data_dir: &Path, id: &str, index: u32) -> std::io::Result<Vec<u8>> {
    let path =
        input_path(data_dir, id, index).ok_or_else(|| std::io::Error::other("not a job id"))?;
    tokio::fs::read(path).await
}

/// Delete file `index`'s kept upload (its file is done); the bytes it took
/// (0 when it was not there).
pub async fn remove_input(data_dir: &Path, id: &str, index: u32) -> u64 {
    let Some(path) = input_path(data_dir, id, index) else {
        return 0;
    };
    let size = tokio::fs::metadata(&path).await.map_or(0, |m| m.len());
    match tokio::fs::remove_file(path).await {
        Ok(()) => size,
        Err(_) => 0,
    }
}

/// A job's kept uploads, read file by file as [`super::super::batch`] admits
/// each.
#[derive(Debug, Clone)]
pub struct Spool {
    pub data_dir: PathBuf,
    pub job: String,
    /// The jobs' storage count, which each upload deleted leaves (review S2).
    pub stored: super::quota::Stored,
}

impl Spool {
    /// `f` with its kept bytes. A file the request already failed (base64
    /// that did not decode) has none kept and passes as it is; a kept
    /// upload that cannot be read is the file's own failed entry.
    pub async fn load(&self, mut f: Input) -> Result<Input, Box<Done>> {
        if f.bytes.is_err() {
            return Ok(f);
        }
        match read_input(&self.data_dir, &self.job, f.index).await {
            Ok(bytes) => {
                f.bytes = Ok(bytes.into());
                Ok(f)
            }
            Err(e) => Err(Box::new(failed_file(
                f.index,
                f.name,
                ErrorCode::Unreadable,
                format!(
                    "the upload kept for this job could not be read back ({}); send the file \
                     again",
                    e.kind()
                ),
            ))),
        }
    }

    /// File `index` is done: its kept upload goes, and leaves the count.
    pub async fn done(&self, index: u32) {
        let gone = remove_input(&self.data_dir, &self.job, index).await;
        self.stored.sub(gone);
    }
}

/// Delete all of job `id`'s kept uploads; the bytes they took.
pub async fn remove_inputs(data_dir: &Path, id: &str) -> u64 {
    let Some(d) = job_dir(data_dir, id).map(|d| d.join(INPUT_DIR)) else {
        return 0;
    };
    let size = size_of(d.clone()).await;
    let _ = tokio::fs::remove_dir_all(d).await;
    size
}

/// The bytes the files under `dir` take, walked on a blocking thread.
async fn size_of(dir: PathBuf) -> u64 {
    tokio::task::spawn_blocking(move || tree_size(&dir))
        .await
        .unwrap_or(0)
}

fn tree_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => tree_size(&e.path()),
            Ok(_) => e.metadata().map_or(0, |m| m.len()),
            Err(_) => 0,
        })
        .sum()
}

/// How many jobs have stored files, and how many bytes those take together
/// (their uploads while they run, their results).
pub async fn usage(data_dir: &Path) -> (u64, u64) {
    let root = dir(data_dir);
    tokio::task::spawn_blocking(move || {
        let size = tree_size;
        let Ok(rd) = std::fs::read_dir(&root) else {
            return (0, 0);
        };
        let mut jobs = 0;
        let mut bytes = 0;
        for e in rd.flatten() {
            if e.file_name().to_str().is_some_and(is_job_id) {
                jobs += 1;
                bytes += size(&e.path());
            }
        }
        (jobs, bytes)
    })
    .await
    .unwrap_or((0, 0))
}

/// File `index`'s entry of job `id`, when it is stored, its pictures'
/// bytes read back (the entry parsed and the bytes encoded on a blocking
/// thread).
pub async fn read(data_dir: &Path, id: &str, index: u32) -> Option<ConvertedDocument> {
    let path = entry_path(data_dir, id, index)?;
    let bytes = tokio::fs::read(&path).await.ok()?;
    let dir = path.parent()?.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut doc: ConvertedDocument = serde_json::from_slice(&bytes).ok()?;
        pictures_in(&dir, &mut doc);
        Some(doc)
    })
    .await
    .ok()
    .flatten()
}

/// `doc`'s picture bytes taken out as the private files to write (under
/// the names lmgw gave them; any other name is not written), its entry
/// left with their names. CPU-bound: runs on a blocking thread.
fn pictures_out(doc: &mut ConvertedDocument) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    use base64::Engine as _;
    let mut out = Vec::new();
    for p in &mut doc.pictures {
        let (Some(file), Some(data)) = (&p.file, p.data_base64.take()) else {
            continue;
        };
        if !is_artifact(file) {
            continue;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(std::io::Error::other)?;
        out.push((file.clone(), bytes));
    }
    Ok(out)
}

/// The stored pictures of `doc` read back from `dir`. A picture whose file
/// is gone keeps its name without bytes. On a blocking thread.
fn pictures_in(dir: &Path, doc: &mut ConvertedDocument) {
    use base64::Engine as _;
    for p in &mut doc.pictures {
        let Some(file) = p.file.as_ref().filter(|f| is_artifact(f)) else {
            continue;
        };
        if p.data_base64.is_some() {
            continue;
        }
        if let Ok(bytes) = std::fs::read(dir.join(file)) {
            p.data_base64 = Some(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
    }
}

/// Delete job `id`'s results; the bytes they took.
pub async fn remove(data_dir: &Path, id: &str) -> u64 {
    let Some(d) = job_dir(data_dir, id) else {
        return 0;
    };
    let size = size_of(d.clone()).await;
    match tokio::fs::remove_dir_all(&d).await {
        Ok(()) => size,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => {
            tracing::warn!("deleting the results of conversion job {id}: {e}");
            0
        }
    }
}

/// The job ids that have a results directory.
pub async fn stored(data_dir: &Path) -> Vec<String> {
    let Ok(mut entries) = tokio::fs::read_dir(dir(data_dir)).await else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    while let Ok(Some(e)) = entries.next_entry().await {
        if let Some(name) = e.file_name().to_str().filter(|n| is_job_id(n)) {
            ids.push(name.to_string());
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pictured(data: &[u8]) -> ConvertedDocument {
        use base64::Engine as _;
        let file = format!("artifacts/image_000000_{}.png", "ab".repeat(32));
        let picture = |file: Option<String>, data: Option<String>| {
            lmgw_api_types::documents::pictures::DocPicture {
                id: "p1".into(),
                part: 1,
                bbox: None,
                caption: None,
                mime: Some("image/png".into()),
                width_px: None,
                height_px: None,
                bytes: Some(data.as_ref().map_or(0, |d| d.len() as u64)),
                file,
                data_base64: data,
                description: None,
            }
        };
        let enc = base64::engine::general_purpose::STANDARD.encode(data);
        let mut d = crate::extract::convert::batch::failed_file(
            2,
            "example.pdf".into(),
            ErrorCode::Unreadable,
            String::new(),
        )
        .doc;
        d.pictures = vec![
            picture(Some(file), Some(enc.clone())),
            // A name lmgw would not make is never a path.
            picture(Some("../../escape.png".into()), Some(enc)),
            picture(None, None),
        ];
        d
    }

    #[tokio::test]
    async fn pictures_are_private_files_beside_the_entry_and_come_back_with_it() {
        let root = tempfile::tempdir().unwrap();
        let doc = pictured(b"\x89PNG example");
        write(root.path(), "cj_0a", doc.clone()).await.unwrap();
        let entry = dir(root.path()).join("cj_0a").join("2");
        let stored: ConvertedDocument =
            serde_json::from_slice(&std::fs::read(entry.join("entry.json")).unwrap()).unwrap();
        assert!(stored.pictures.iter().all(|p| p.data_base64.is_none()));
        let file = entry.join(doc.pictures[0].file.as_ref().unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), b"\x89PNG example");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!root.path().join("convert").join("escape.png").exists());
        let back = read(root.path(), "cj_0a", 2).await.unwrap();
        assert_eq!(back.pictures[0].data_base64, doc.pictures[0].data_base64);
        assert!(back.pictures[1].data_base64.is_none());
        let (jobs, bytes) = usage(root.path()).await;
        assert_eq!(jobs, 1);
        assert!(bytes >= 13);
        remove(root.path(), "cj_0a").await;
        assert!(!file.exists());
    }

    #[test]
    fn only_lmgw_job_ids_name_a_directory() {
        assert!(is_job_id("cj_0123abcd"));
        for bad in ["cj_", "cj_../x", "../cj_00", "job", "cj_00/1", "cj_zz"] {
            assert!(!is_job_id(bad), "{bad}");
        }
    }
}
