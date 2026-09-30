//! The uploaded files themselves (chat-complete design §9.1): under
//! `<data_dir>/knowledge/`, the directory 0700 and each file 0600, named by
//! the sha256 of its bytes. Kept so a file can be re-ingested (new chunk
//! settings, a vision model added later) and downloaded from the source
//! viewer — never inside a database that is exported whole.
//!
//! Content-addressed: the same bytes in two bases are one file, removed when
//! the last row naming it goes ([`remove_if_unused`]).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, MutexGuard};

/// Held by whoever stores an original *and* records the row that needs it
/// ([`hold`]), and by [`remove_if_unused`] across its check and its delete.
/// Without it, deleting the last row of some bytes could remove the file an
/// upload of the same bytes to another base had just stored and not yet
/// recorded.
static LOCK: Mutex<()> = Mutex::const_new(());

/// Take the originals lock. Do not call [`remove_if_unused`] while holding it.
pub async fn hold() -> MutexGuard<'static, ()> {
    LOCK.lock().await
}

/// The directory under the data dir.
pub const ORIGINALS_DIR: &str = "knowledge";

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join(ORIGINALS_DIR)
}

/// Where the original with this digest lives. The digest is checked to be
/// hex first: it names a file, and nothing else may.
pub fn path(data_dir: &Path, sha: &str) -> Result<PathBuf, String> {
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("'{sha}' is not a sha256 digest"));
    }
    Ok(dir(data_dir).join(sha))
}

/// Store `bytes` under their digest (a no-op when already there) and return
/// the digest. Written to a temporary name and renamed, so a crash never
/// leaves a truncated original under a real digest.
pub async fn store(data_dir: &Path, bytes: &[u8]) -> Result<String, String> {
    let sha = sha256_hex(bytes);
    let target = path(data_dir, &sha)?;
    let d = dir(data_dir);
    let bytes = bytes.to_vec();
    let sha2 = sha.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::create_dir_all(&d)?;
        set_mode(&d, 0o700)?;
        if target.exists() {
            set_mode(&target, 0o600)?;
            return Ok(());
        }
        // A name of its own per write: two uploads of the same bytes at once
        // must not share a temporary file. Both renames land the same content,
        // so whichever comes second simply replaces an identical file.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = d.join(format!(
            ".{sha2}.{}.{}.part",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let written = std::fs::write(&tmp, &bytes)
            .and_then(|()| set_mode(&tmp, 0o600))
            .and_then(|()| std::fs::rename(&tmp, &target));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("storing the original: {e}"))?;
    Ok(sha)
}

pub async fn read(data_dir: &Path, sha: &str) -> Result<Vec<u8>, String> {
    let p = path(data_dir, sha)?;
    tokio::fs::read(&p).await.map_err(|e| {
        format!(
            "the original file {} could not be read ({e}) — upload it again",
            p.display()
        )
    })
}

/// Delete the original when no file row names it any more. A failure is
/// logged, never raised: the row is already gone, and a stray file under a
/// digest is harmless where a half-finished delete is not.
pub async fn remove_if_unused(pool: &SqlitePool, data_dir: &Path, sha: &str) {
    let _held = hold().await;
    match super::store::sha_in_use(pool, sha).await {
        Ok(true) => {}
        Ok(false) => {
            // The vision readings kept for these bytes go with them.
            if let Err(e) = super::store::delete_page_reads(pool, sha).await {
                tracing::warn!("removing the page readings of {sha}: {e}");
            }
            if let Ok(p) = path(data_dir, sha) {
                if let Err(e) = tokio::fs::remove_file(&p).await {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!("removing knowledge original {}: {e}", p.display());
                    }
                }
            }
        }
        Err(e) => tracing::warn!("checking whether original {sha} is still used: {e}"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn originals_are_private_and_named_by_their_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = store(tmp.path(), b"hello").await.unwrap();
        assert_eq!(sha, sha256_hex(b"hello"));
        assert_eq!(read(tmp.path(), &sha).await.unwrap(), b"hello");
        // Storing the same bytes again is a no-op, not an error.
        assert_eq!(store(tmp.path(), b"hello").await.unwrap(), sha);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir(tmp.path())), 0o700);
            assert_eq!(mode(&path(tmp.path(), &sha).unwrap()), 0o600);
        }
        assert!(path(tmp.path(), "../etc/passwd").is_err());
    }

    /// The check-then-delete of an unused original and an upload's
    /// store-then-record are one critical section each: a delete cannot land
    /// between an upload storing the bytes and recording the row that needs
    /// them.
    #[tokio::test]
    async fn a_delete_waits_for_an_upload_that_holds_the_originals() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = super::super::store::open_in_memory().await.unwrap();
        let sha = store(tmp.path(), b"bytes").await.unwrap();
        let guard = hold().await;
        let (p, d, s) = (pool.clone(), tmp.path().to_path_buf(), sha.clone());
        let delete = tokio::spawn(async move { remove_if_unused(&p, &d, &s).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!delete.is_finished(), "the delete waits for the holder");
        assert!(path(tmp.path(), &sha).unwrap().exists());
        drop(guard);
        delete.await.unwrap();
        // Unused, so it is gone once the holder let go.
        assert!(!path(tmp.path(), &sha).unwrap().exists());
    }

    #[tokio::test]
    async fn concurrent_stores_of_the_same_bytes_do_not_trip_over_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let bytes = vec![7u8; 64 * 1024];
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let (d, b) = (tmp.path().to_path_buf(), bytes.clone());
            tasks.push(tokio::spawn(async move { store(&d, &b).await }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        let sha = sha256_hex(&bytes);
        assert_eq!(read(tmp.path(), &sha).await.unwrap(), bytes);
        let left: Vec<_> = std::fs::read_dir(dir(tmp.path()))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, [sha], "no temporary file is left behind");
    }
}
