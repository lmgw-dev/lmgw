//! The downloads' open files (device transfers design §5, I26): a download
//! body reads its transfer's file through a [`Download`], which holds the
//! file weakly; this registry holds it strongly, by transfer id. Every end
//! of a transfer closes its downloads ([`Downloads::close`]): the registry
//! lets go of their files, so each one's fd closes as soon as a read in
//! flight returns — a receiver that stopped reading keeps neither the fd
//! nor the deleted file's disk space — and the body's next block fails
//! (2026-10-22). Nothing asks the filesystem whether the name is gone,
//! which NFS, FUSE and a hard-linked snapshot answer wrongly.

use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, Weak};

use bytes::Bytes;

/// The open files, by transfer id and then by download.
type Open = Arc<StdMutex<HashMap<String, HashMap<u64, Arc<File>>>>>;

/// The registry.
#[derive(Debug, Default)]
pub(crate) struct Downloads {
    open: Open,
    /// The next download's number among its transfer's.
    next: AtomicU64,
}

/// One download's hold on its transfer's file: weak, so the registry's
/// close is what closes the fd. Dropped (the body ended or went), it takes
/// its file out of the registry.
pub(crate) struct Download {
    open: Open,
    id: String,
    number: u64,
    file: Weak<File>,
}

fn lock(open: &Open) -> MutexGuard<'_, HashMap<String, HashMap<u64, Arc<File>>>> {
    open.lock().unwrap_or_else(|e| e.into_inner())
}

impl Downloads {
    /// Register `file`, transfer `id`'s, for a download.
    pub(crate) fn open(&self, id: &str, file: File) -> Download {
        let file = Arc::new(file);
        let weak = Arc::downgrade(&file);
        let number = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.open)
            .entry(id.to_string())
            .or_default()
            .insert(number, file);
        Download {
            open: self.open.clone(),
            id: id.to_string(),
            number,
            file: weak,
        }
    }

    /// Transfer `id` ended: let go of every file its downloads read. Each
    /// fd closes here, or once the read in flight on it returns.
    pub(crate) fn close(&self, id: &str) {
        let files = lock(&self.open).remove(id);
        // Outside the lock: the closes.
        drop(files);
    }

    /// How many files the registry holds: one per download in progress.
    pub(crate) fn len(&self) -> usize {
        lock(&self.open).values().map(HashMap::len).sum()
    }
}

impl Download {
    /// Up to `len` bytes of the file from byte `at` (`content::read_at`);
    /// `NotFound` once its transfer ended. The read holds the file only
    /// while it runs.
    pub(crate) async fn read_at(&self, at: u64, len: usize) -> std::io::Result<Bytes> {
        let Some(file) = self.file.upgrade() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the transfer ended and its bytes were deleted",
            ));
        };
        super::content::read_at(file, at, len).await
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        let mut open = lock(&self.open);
        let Some(files) = open.get_mut(&self.id) else {
            // Its transfer ended: the close took the file.
            return;
        };
        let file = files.remove(&self.number);
        if files.is_empty() {
            open.remove(&self.id);
        }
        drop(open);
        drop(file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file to read: `bytes` in a temporary directory.
    fn a_file(bytes: &[u8]) -> (tempfile::TempDir, File) {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        std::fs::write(&p, bytes).unwrap();
        let f = File::open(&p).unwrap();
        (tmp, f)
    }

    /// A close lets go of the file at once, though the download is still
    /// there, and its next read fails; another transfer's download reads
    /// on.
    #[tokio::test]
    async fn a_close_lets_go_of_the_file_and_fails_the_next_read() {
        let downloads = Downloads::default();
        let (_a, fa) = a_file(b"abc");
        let (_b, fb) = a_file(b"xyz");
        let a = downloads.open("a", fa);
        let b = downloads.open("b", fb);
        assert_eq!(&a.read_at(1, 10).await.unwrap()[..], b"bc");
        assert_eq!(downloads.len(), 2);
        downloads.close("a");
        assert_eq!(a.file.strong_count(), 0, "the fd is closed");
        assert_eq!(downloads.len(), 1);
        assert_eq!(
            a.read_at(0, 3).await.unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(&b.read_at(0, 3).await.unwrap()[..], b"xyz");
        drop(a);
        drop(b);
        assert_eq!(downloads.len(), 0, "a download that goes takes its file");
    }
}
