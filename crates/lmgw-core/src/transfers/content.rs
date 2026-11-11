//! A transfer's file (device transfers design §3): `<data_dir>/transfers/<id>`,
//! the directory `0700` and the file `0600`, created with its mode before a
//! byte is written. The id is 32 random lowercase hex digits, checked
//! before it names a file. While a transfer uploads, the file's length is
//! the resume point; the sha256 is computed from the file once the last
//! byte is there (I10). Writes go to explicit offsets (I22); a download
//! reads through the registry that every end closes (`downloads`, I26).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;

/// The directory under the data dir.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("transfers")
}

/// Whether `s` is a transfer id: 32 lowercase hex digits (I6).
pub fn is_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A new transfer id: 128 random bits as 32 lowercase hex digits.
pub fn new_id() -> String {
    let bytes: [u8; 16] = rand::random();
    hex::encode(bytes)
}

/// Where transfer `id`'s file is; `id` is a transfer id ([`is_id`]).
fn path(data_dir: &Path, id: &str) -> PathBuf {
    debug_assert!(is_id(id), "{id} names no transfer file");
    dir(data_dir).join(id)
}

/// The length of transfer `id`'s file: the bytes lmgw holds of it; `0`
/// when it has none (yet, or any more).
pub(crate) async fn len(data_dir: &Path, id: &str) -> u64 {
    if !is_id(id) {
        return 0;
    }
    tokio::fs::metadata(path(data_dir, id))
        .await
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Transfer `id`'s file opened to write to at explicit offsets
/// ([`write_at`]), made (`0600`, in its `0700` directory) when it is not
/// there. A file made here has its directory synced, and a directory made
/// here the data directory, so a `received` an answer gave names a file a
/// crash keeps (§3).
pub(crate) async fn open_write(data_dir: &Path, id: &str) -> std::io::Result<Arc<File>> {
    let data_dir = data_dir.to_path_buf();
    let p = path(&data_dir, id);
    tokio::task::spawn_blocking(move || {
        let d = dir(&data_dir);
        let made_dir = !d.is_dir();
        std::fs::create_dir_all(&d)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
        }
        if made_dir {
            sync_dir(&data_dir)?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&p) {
            Ok(f) => {
                sync_dir(&d)?;
                Ok(Arc::new(f))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => std::fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .map(Arc::new),
            Err(e) => Err(e),
        }
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Sync directory `d`, so a name made in it survives a crash.
fn sync_dir(d: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(d)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = d;
        Ok(())
    }
}

/// Write `bytes` to `file` at byte `at`, off the runtime's threads. At its
/// own offset, never appended: a write that lands late (its request gone)
/// writes the bytes that belong there, and cannot shift a retry's (I22).
pub(crate) async fn write_at(file: &Arc<File>, at: u64, bytes: Bytes) -> std::io::Result<()> {
    let file = file.clone();
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            file.write_all_at(&bytes, at)
        }
        #[cfg(not(unix))]
        {
            use std::io::{Seek, Write};
            let mut f: &File = &file;
            f.seek(std::io::SeekFrom::Start(at))?;
            f.write_all(&bytes)
        }
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Sync `file`'s bytes to the disk (I11).
pub(crate) async fn sync(file: &Arc<File>) -> std::io::Result<()> {
    let file = file.clone();
    tokio::task::spawn_blocking(move || file.sync_all())
        .await
        .map_err(std::io::Error::other)?
}

/// Transfer `id`'s file opened to read from, for a download to register
/// (`downloads`).
pub(crate) async fn open_read(data_dir: &Path, id: &str) -> std::io::Result<File> {
    let p = path(data_dir, id);
    tokio::task::spawn_blocking(move || File::open(p))
        .await
        .map_err(std::io::Error::other)?
}

/// Up to `len` bytes of `file` from byte `at`, off the runtime's threads;
/// fewer only at the file's end. `file` goes into the read and is let go
/// with it: a download's file is held only while a read runs.
pub(crate) async fn read_at(file: Arc<File>, at: u64, len: usize) -> std::io::Result<Bytes> {
    tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; len];
        let mut got = 0;
        while got < len {
            let n = read_some_at(&file, &mut buf[got..], at + got as u64)?;
            if n == 0 {
                break;
            }
            got += n;
        }
        buf.truncate(got);
        Ok(Bytes::from(buf))
    })
    .await
    .map_err(std::io::Error::other)?
}

/// One read of `file` at `at`.
fn read_some_at(file: &File, buf: &mut [u8], at: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(buf, at)
    }
    #[cfg(not(unix))]
    {
        use std::io::{Read, Seek};
        let mut f: &File = file;
        f.seek(std::io::SeekFrom::Start(at))?;
        f.read(buf)
    }
}

/// The length of transfer `id`'s file once its bytes are synced (`0` when
/// there is none): the resume point an answer reports without writing,
/// so it names bytes a crash keeps (§3).
pub(crate) async fn synced_len(data_dir: &Path, id: &str) -> std::io::Result<u64> {
    let p = path(data_dir, id);
    tokio::task::spawn_blocking(
        move || match std::fs::OpenOptions::new().write(true).open(&p) {
            Ok(f) => {
                f.sync_all()?;
                Ok(f.metadata()?.len())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        },
    )
    .await
    .map_err(std::io::Error::other)?
}

/// The sha256 of transfer `id`'s file, lowercase hex, synced first (the
/// bytes a `ready` names are on the disk), read in 1 MiB blocks off the
/// runtime's threads.
pub(crate) async fn hash_file(data_dir: &Path, id: &str) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read as _;
    let p = path(data_dir, id);
    tokio::task::spawn_blocking(move || {
        // Windows syncs (FlushFileBuffers) only a handle open for writing.
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(cfg!(windows))
            .open(&p)?;
        f.sync_all()?;
        let mut hasher = Sha256::new();
        let mut block = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut block)?;
            if n == 0 {
                break;
            }
            hasher.update(&block[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Delete transfer `id`'s file; one that is not there is fine.
pub(crate) async fn unlink(data_dir: &Path, id: &str) {
    if !is_id(id) {
        return;
    }
    match tokio::fs::remove_file(path(data_dir, id)).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            "transfer {id}: deleting its file failed ({e}); the hourly sweep tries again"
        ),
    }
}

/// The transfer ids the directory holds a file for; other names are left
/// alone.
pub(crate) async fn files(data_dir: &Path) -> Vec<String> {
    let Ok(mut entries) = tokio::fs::read_dir(dir(data_dir)).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while let Ok(Some(e)) = entries.next_entry().await {
        if let Some(name) = e.file_name().to_str().filter(|n| is_id(n)) {
            out.push(name.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_32_lowercase_hex_digits() {
        let id = new_id();
        assert!(is_id(&id), "{id}");
        assert_ne!(id, new_id());
        for bad in ["", "../etc/passwd", &"A".repeat(32), &"a".repeat(31)] {
            assert!(!is_id(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_file_is_written_at_its_offsets_and_hashes_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let id = new_id();
        assert_eq!(len(tmp.path(), &id).await, 0);
        for (at, part) in [(0, &b"ab"[..]), (2, b"c"), (1, b"b")] {
            let f = open_write(tmp.path(), &id).await.unwrap();
            write_at(&f, at, Bytes::from_static(part)).await.unwrap();
            sync(&f).await.unwrap();
        }
        assert_eq!(
            len(tmp.path(), &id).await,
            3,
            "a write at an offset it holds"
        );
        assert_eq!(
            hash_file(tmp.path(), &id).await.unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(dir(tmp.path())), 0o700);
            assert_eq!(mode(path(tmp.path(), &id)), 0o600);
        }
        assert_eq!(files(tmp.path()).await, std::slice::from_ref(&id));
        assert_eq!(synced_len(tmp.path(), &id).await.unwrap(), 3);
        let f = Arc::new(open_read(tmp.path(), &id).await.unwrap());
        assert_eq!(&read_at(f.clone(), 1, 10).await.unwrap()[..], b"bc");
        assert_eq!(Arc::strong_count(&f), 1, "the read let go of the file");
        unlink(tmp.path(), &id).await;
        unlink(tmp.path(), &id).await;
        assert!(files(tmp.path()).await.is_empty());
        assert_eq!(synced_len(tmp.path(), &id).await.unwrap(), 0);
    }
}
