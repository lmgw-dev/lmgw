//! The artifact files (labs-store design §1.4): media bytes under
//! `<data_dir>/labs/artifacts/`, the directory 0700 and each file 0600, named
//! by the sha256 of its bytes, so one image sent with fifty groups is one
//! file. The rows that name them are `store::lab_artifacts`.
//!
//! A file is written before the write transaction that inserts its row, and
//! unlinked after the transaction that deleted the last row naming it
//! committed: file work never runs inside a write transaction. Both hold
//! [`hold`], so a delete cannot unlink a file an ingest is about to name.
//! The gateway is its data dir's only writer. A crash between a write and
//! its commit, or a commit and its unlink, leaves a file no row names: an
//! **orphan**, counted by [`orphans`] and removed by [`remove_orphans`] when
//! the owner asks. Nothing sweeps on its own.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, MutexGuard};

/// Held across a file write and the transaction that names it, and across a
/// delete's transaction and its unlinks.
static LOCK: Mutex<()> = Mutex::const_new(());

/// Take the artifacts lock.
pub async fn hold() -> MutexGuard<'static, ()> {
    LOCK.lock().await
}

/// The directory under the data dir.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("labs").join("artifacts")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Where the file with this digest lives. The digest is checked to be hex
/// first: it names a file, and nothing else may.
pub fn path(data_dir: &Path, sha: &str) -> Result<PathBuf, String> {
    if !is_sha(sha) {
        return Err(format!("'{sha}' is not a sha256 digest"));
    }
    Ok(dir(data_dir).join(sha))
}

#[cfg(unix)]
fn set_mode(p: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_p: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// Store `bytes` under their digest and return the digest. Written to
/// `<sha256>.tmp-<pid>-<n>`, synced to the disk and renamed, the directory
/// synced after (best effort), so a crash never leaves a short file under a
/// real digest. A file already there is kept when its length is the bytes'
/// and written again when it is not: a short file from before a power cut
/// would otherwise stand for its digest for good. The caller holds [`hold`]
/// until the row that names it is committed.
pub async fn write(data_dir: &Path, bytes: Bytes) -> Result<String, String> {
    let sha = sha256_hex(&bytes);
    let target = path(data_dir, &sha)?;
    let d = dir(data_dir);
    let name = sha.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::create_dir_all(&d)?;
        set_mode(&d, 0o700)?;
        match std::fs::metadata(&target) {
            Ok(meta) if meta.len() == bytes.len() as u64 => return Ok(()),
            Ok(meta) => tracing::warn!(
                "lab artifact {} holds {} of its {} bytes: written again",
                target.display(),
                meta.len(),
                bytes.len()
            ),
            Err(_) => {}
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = d.join(format!(
            "{name}.tmp-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let written = write_synced(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &target));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written?;
        // The rename's directory entry. Best effort: not every filesystem
        // lets a directory be opened for a sync, and the file itself is.
        if let Ok(dir) = std::fs::File::open(&d) {
            let _ = dir.sync_all();
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("writing the lab artifact: {e}"))?;
    Ok(sha)
}

/// `bytes` into a new file at `p`, 0600, on the disk before it returns.
fn write_synced(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::File::create(p)?;
    set_mode(p, 0o600)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// The spool files being written now: not orphans while their writer runs,
/// whatever the scan finds.
static SPOOLING: std::sync::Mutex<BTreeSet<PathBuf>> = std::sync::Mutex::new(BTreeSet::new());

fn spooling() -> std::sync::MutexGuard<'static, BTreeSet<PathBuf>> {
    SPOOLING.lock().unwrap_or_else(|e| e.into_inner())
}

/// How much of a spooled file's start is kept to sniff its type: the magic
/// numbers sit in its first bytes, and a text test over this much tells an
/// event stream from raw PCM. It bounds nothing of the file.
const SNIFF_HEAD: usize = 64 * 1024;

/// A spool file's path. Dropped before it is adopted, the file goes.
#[derive(Debug)]
struct SpoolPath(Option<PathBuf>);

impl SpoolPath {
    /// The path, no longer removed on drop nor spooling.
    fn release(&mut self) -> Option<PathBuf> {
        let p = self.0.take()?;
        spooling().remove(&p);
        Some(p)
    }
}

impl Drop for SpoolPath {
    fn drop(&mut self) {
        if let Some(p) = self.release() {
            if let Err(e) = std::fs::remove_file(&p) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("removing the spool file {}: {e}", p.display());
                }
            }
        }
    }
}

/// An artifact written to a file under the artifacts directory as its bytes
/// arrive, so an answer as long as a speech stream is never held in memory
/// whole. [`Spool::finish`] syncs it to the disk and reads its digest, size
/// and first bytes back from the file itself, so they are what the file
/// holds even when a write was cut off; a record [`adopt`]s it under its
/// digest. A spool dropped first removes its file. While it is being
/// written the file is no orphan ([`orphans`] passes it over); one a crash
/// left is.
#[derive(Debug)]
pub struct Spool {
    path: SpoolPath,
    file: tokio::fs::File,
}

/// A finished [`Spool`]: on the disk, its digest and size known.
#[derive(Debug)]
pub struct Spooled {
    path: SpoolPath,
    pub sha256: String,
    pub size: u64,
    /// Its first bytes, to sniff its type by.
    pub head: Bytes,
}

impl Spool {
    /// A new spool file, `spool-<pid>-<n>` in the artifacts directory.
    pub async fn create(data_dir: &Path) -> Result<Self, String> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let d = dir(data_dir);
        let fail = |e: std::io::Error| format!("spooling the lab artifact: {e}");
        tokio::fs::create_dir_all(&d).await.map_err(fail)?;
        let dd = d.clone();
        tokio::task::spawn_blocking(move || set_mode(&dd, 0o700))
            .await
            .map_err(|e| e.to_string())?
            .map_err(fail)?;
        let p = d.join(format!(
            "spool-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        spooling().insert(p.clone());
        let path = SpoolPath(Some(p.clone()));
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let file = opts.open(&p).await.map_err(fail)?;
        Ok(Self { path, file })
    }

    /// The next bytes, onto the file.
    pub async fn write(&mut self, chunk: &[u8]) -> Result<(), String> {
        use tokio::io::AsyncWriteExt as _;
        self.file
            .write_all(chunk)
            .await
            .map_err(|e| format!("spooling the lab artifact: {e}"))
    }

    /// The file on the disk, its digest, size and first bytes read back
    /// from it.
    pub async fn finish(mut self) -> Result<Spooled, String> {
        use std::io::Read as _;
        use tokio::io::AsyncWriteExt as _;
        let fail = |e: std::io::Error| format!("spooling the lab artifact: {e}");
        self.file.flush().await.map_err(fail)?;
        self.file.sync_all().await.map_err(fail)?;
        let Some(p) = self.path.0.clone() else {
            return Err("the spool file was given up already".into());
        };
        let (sha256, size, head) = tokio::task::spawn_blocking(move || {
            let mut f = std::fs::File::open(&p)?;
            let mut hasher = Sha256::new();
            let mut head = Vec::new();
            let mut size = 0u64;
            let mut block = vec![0u8; 1 << 20];
            loop {
                let n = f.read(&mut block)?;
                if n == 0 {
                    break;
                }
                hasher.update(&block[..n]);
                let room = SNIFF_HEAD.saturating_sub(head.len());
                head.extend_from_slice(&block[..room.min(n)]);
                size += n as u64;
            }
            Ok::<_, std::io::Error>((hex::encode(hasher.finalize()), size, head))
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(fail)?;
        Ok(Spooled {
            sha256,
            size,
            head: Bytes::from(head),
            path: SpoolPath(self.path.0.take()),
        })
    }
}

/// Store a finished spool under its digest and return the digest: renamed
/// into place, or removed when a whole file of that digest is there
/// already, as [`write`] keeps one. The caller holds [`hold`] until the row
/// that names it is committed.
pub async fn adopt(data_dir: &Path, mut s: Spooled) -> Result<String, String> {
    let target = path(data_dir, &s.sha256)?;
    let Some(from) = s.path.0.clone() else {
        return Err("the spool file was given up already".into());
    };
    let size = s.size;
    let d = dir(data_dir);
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        match std::fs::metadata(&target) {
            Ok(meta) if meta.len() == size => return std::fs::remove_file(&from),
            Ok(meta) => tracing::warn!(
                "lab artifact {} holds {} of its {size} bytes: written again",
                target.display(),
                meta.len(),
            ),
            Err(_) => {}
        }
        std::fs::rename(&from, &target)?;
        if let Ok(dir) = std::fs::File::open(&d) {
            let _ = dir.sync_all();
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("keeping the spooled lab artifact: {e}"))?;
    // Moved or removed: nothing left for the drop to remove.
    s.path.release();
    Ok(s.sha256.clone())
}

/// The bytes of the file with this digest.
pub async fn read(data_dir: &Path, sha: &str) -> Result<Vec<u8>, String> {
    let p = path(data_dir, sha)?;
    tokio::fs::read(&p)
        .await
        .map_err(|e| format!("the lab artifact {} could not be read ({e})", p.display()))
}

/// Unlink the files with these digests. A failure is logged, never raised:
/// the rows are gone already, and a file left behind is an orphan the usage
/// read counts.
pub async fn unlink(data_dir: &Path, shas: &[String]) {
    for sha in shas {
        let Ok(p) = path(data_dir, sha) else {
            continue;
        };
        if let Err(e) = tokio::fs::remove_file(&p).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("removing lab artifact {}: {e}", p.display());
            }
        }
    }
}

/// The files in the artifacts directory that no row names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Orphans {
    pub files: u64,
    pub bytes: u64,
    pub paths: Vec<PathBuf>,
}

/// Scan for [`Orphans`]: every file in the directory whose name is not a
/// digest a row names (a leftover temporary file included). The caller
/// holds [`hold`], so a write in progress is not one; a [`Spool`] being
/// written is passed over.
pub async fn orphans(pool: &SqlitePool, data_dir: &Path) -> Result<Orphans, String> {
    let named = crate::store::named_lab_shas(pool)
        .await
        .map_err(|e| e.to_string())?;
    let d = dir(data_dir);
    tokio::task::spawn_blocking(move || -> std::io::Result<Orphans> {
        let mut out = Orphans::default();
        // Every file the scan sees, checked against the spools after the
        // whole directory is read: a spool registers before its file is
        // created, so one the scan saw is registered when the list is read
        // here, however late it began (read first, a spool started between
        // the two would be taken for an orphan and its run lost).
        let mut seen = Vec::new();
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let meta = entry.metadata()?;
            if !meta.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_sha(&name) && named.contains(&name) {
                continue;
            }
            seen.push((entry.path(), meta.len()));
        }
        let live = spooling().clone();
        for (path, len) in seen {
            // A spool adopted or dropped since it was seen is gone, not an
            // orphan.
            if live.contains(&path) || !path.exists() {
                continue;
            }
            out.files += 1;
            out.bytes += len;
            out.paths.push(path);
        }
        out.paths.sort();
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("scanning the lab artifacts: {e}"))
}

/// Remove every orphan; how many went.
pub async fn remove_orphans(pool: &SqlitePool, data_dir: &Path) -> Result<u64, String> {
    let _held = hold().await;
    let found = orphans(pool, data_dir).await?;
    let mut removed = 0;
    for p in &found.paths {
        match tokio::fs::remove_file(p).await {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("removing {}: {e}", p.display())),
        }
    }
    Ok(removed)
}

/// A data URL's parts: its head as typed, the type it declares and its
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataUrl {
    /// Everything before the comma, `data:` included
    /// (`data:image/x-receipt;name=a.png;base64`): what [`data_url_again`]
    /// writes back.
    pub head: String,
    pub declared: Option<String>,
    pub bytes: Vec<u8>,
}

/// Read `data:<type>[;<parameter>…];base64,<payload>`. `None` for any other
/// string, a data URL whose payload is not canonical base64 included
/// (whitespace, missing padding): it is kept inline as it was. So the
/// payload [`data_url_again`] writes is the payload as typed.
pub fn parse_data_url(url: &str) -> Option<DataUrl> {
    let (head, payload) = url.split_once(',')?;
    let ty = head.strip_prefix("data:")?.strip_suffix(";base64")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    let declared = ty
        .split(';')
        .next()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Some(DataUrl {
        head: head.to_string(),
        declared,
        bytes,
    })
}

/// An artifact sent again as the data URL it came as: `<head>,<base64>`,
/// byte for byte what was typed (labs-store design Decision 21). An
/// artifact that came as bytes has no head and goes as
/// `data:<type>;base64,…`, its type without parameters.
pub fn data_url_again(head: Option<&str>, mime: &str, bytes: &[u8]) -> String {
    let payload = base64::engine::general_purpose::STANDARD.encode(bytes);
    match head {
        Some(head) => format!("{head},{payload}"),
        None => format!(
            "data:{};base64,{payload}",
            mime.split(';').next().unwrap_or_default().trim()
        ),
    }
}

/// An artifact's MIME type: sniffed from the bytes, as chat attachments are,
/// on the blocking pool (a ZIP-headed input is inflated to tell its kind);
/// the declared type where the bytes are of no kind lmgw recognises, and
/// `application/octet-stream` where none was declared either.
pub async fn mime_of(bytes: Bytes, declared: Option<&str>) -> String {
    match crate::extract::sniff::sniff_artifact_async(bytes).await {
        Ok(s) => s.mime.to_string(),
        Err(_) => declared
            .filter(|d| !d.trim().is_empty())
            .unwrap_or("application/octet-stream")
            .to_string(),
    }
}

/// [`mime_of`] for a file known by its first bytes only (a spooled one):
/// a head cut inside a UTF-8 character is read up to it, so an event
/// stream is still the text it is.
pub async fn mime_of_head(head: Bytes, declared: Option<&str>) -> String {
    let head = match std::str::from_utf8(&head) {
        Err(e) if e.error_len().is_none() && e.valid_up_to() > 0 => head.slice(..e.valid_up_to()),
        _ => head,
    };
    mime_of(head, declared).await
}

/// The file extension an export gives an artifact of type `mime`.
pub fn ext_of(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or_default().trim() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/aac" => "aac",
        "audio/opus" => "opus",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/flac" => "flac",
        "audio/mp4" => "m4a",
        "audio/webm" => "webm",
        "audio/midi" => "mid",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "application/json" => "json",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base64_data_url_is_read_and_anything_else_is_not() {
        let d = parse_data_url("data:image/png;base64,iVBORw0KGgo=").unwrap();
        assert_eq!(d.head, "data:image/png;base64");
        assert_eq!(d.declared.as_deref(), Some("image/png"));
        assert_eq!(d.bytes, b"\x89PNG\r\n\x1a\n");
        assert!(parse_data_url("data:text/plain,hello").is_none());
        assert!(parse_data_url("https://img.example/a.png").is_none());
        assert!(parse_data_url("data:image/png;base64,***").is_none());
        assert!(
            parse_data_url("data:image/png;base64, iVBORw0KGgo=").is_none(),
            "a payload that would not come back as typed stays inline"
        );
        assert!(parse_data_url("data:image/png;base64,iVBORw0KGgo").is_none());
        assert_eq!(parse_data_url("data:;base64,AA==").unwrap().declared, None);
    }

    #[test]
    fn a_data_url_goes_out_again_as_it_was_typed() {
        for url in [
            "data:image/png;base64,iVBORw0KGgo=",
            "data:image/x-receipt;name=receipt.png;base64,iVBORw0KGgo=",
            "data:text/plain;charset=US-ASCII;base64,aGVsbG8=",
            "data:;base64,AA==",
        ] {
            let d = parse_data_url(url).unwrap();
            assert_eq!(
                data_url_again(Some(&d.head), "whatever/sniffed", &d.bytes),
                url
            );
        }
        assert_eq!(
            data_url_again(None, "text/plain; charset=utf-8", b"hello"),
            "data:text/plain;base64,aGVsbG8=",
            "bytes that came as bytes: the type alone, no space in the URL"
        );
    }

    #[tokio::test]
    async fn the_type_is_sniffed_and_the_declared_one_only_stands_in() {
        let mime = |b: &[u8], d: Option<&'static str>| mime_of(Bytes::copy_from_slice(b), d);
        assert_eq!(
            mime(b"\x89PNG\r\n\x1a\n....", Some("image/jpeg")).await,
            "image/png"
        );
        assert_eq!(
            mime(&[0, 159, 146, 150], Some("image/x-raw")).await,
            "image/x-raw"
        );
        assert_eq!(
            mime(&[0, 159, 146, 150], None).await,
            "application/octet-stream"
        );
        assert_eq!(ext_of("image/jpeg"), "jpg");
        assert_eq!(ext_of("text/plain; charset=utf-8"), "txt");
        assert_eq!(ext_of("application/x-whatever"), "bin");
    }

    #[tokio::test]
    async fn a_short_file_under_a_digest_is_written_again() {
        let data = tempfile::tempdir().unwrap();
        let bytes = Bytes::from_static(b"the whole of it");
        let sha = sha256_hex(&bytes);
        std::fs::create_dir_all(dir(data.path())).unwrap();
        let p = path(data.path(), &sha).unwrap();
        std::fs::write(&p, b"the who").unwrap();
        assert_eq!(write(data.path(), bytes.clone()).await.unwrap(), sha);
        assert_eq!(std::fs::read(&p).unwrap(), bytes);
        // Whole now: kept as it is.
        let before = std::fs::metadata(&p).unwrap().modified().unwrap();
        write(data.path(), bytes.clone()).await.unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().modified().unwrap(), before);
        let left: Vec<_> = std::fs::read_dir(dir(data.path())).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary file left");
    }

    #[test]
    fn only_a_digest_names_a_file() {
        let d = Path::new("/data");
        assert!(path(d, "../../etc/passwd").is_err());
        let sha = sha256_hex(b"x");
        assert_eq!(
            path(d, &sha).unwrap(),
            Path::new("/data/labs/artifacts").join(&sha)
        );
    }
}
