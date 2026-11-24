//! Files of a job's directory, made private (0700 directories, 0600 files
//! on Unix; the data dir's own access rules elsewhere): they hold document
//! content, uploads and converted text alike.

use std::path::Path;

/// `path` and its missing parents, private.
pub async fn create_dir_all(path: &Path) -> std::io::Result<()> {
    let mut b = tokio::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    b.mode(0o700);
    b.create(path).await
}

/// `bytes` into a file at `path`, private. An existing file is replaced.
pub async fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let mut o = tokio::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    o.mode(0o600);
    let mut f = o.open(path).await?;
    f.write_all(bytes).await?;
    f.flush().await
}

/// Whether `e` is the disk being full.
pub fn disk_full(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::StorageFull
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[tokio::test]
    async fn directories_are_0700_and_files_0600() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a").join("b");
        create_dir_all(&dir).await.unwrap();
        let file = dir.join("f");
        write(&file, b"text").await.unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&root.path().join("a")), 0o700);
        assert_eq!(mode(&file), 0o600);
    }
}
