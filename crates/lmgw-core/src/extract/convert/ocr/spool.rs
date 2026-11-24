//! A file on disk for the tools that read parts of it (document-conversion
//! design §5.1, WP4 review round, §14 performance round): a PDF, which
//! every poppler run reads (its geometry, its pictures, its pages rendered
//! for text recognition, the layout model and the page-reading model), a
//! TIFF whose pages are decoded one by one.
//!
//! Piping the file to each tool sends it once per run, and poppler reads a
//! piped file whole into memory: a 500-page, 200 MiB scan was 100 GiB
//! through pipes, each `pdftoppm` reading all of it to render one page.
//! Written once to a spool file, each run gets the file as its stdin and
//! reads what it needs by position: poppler is told `/dev/stdin`, which it
//! opens again by that path and reads by position (it reads an `fd://0`
//! input to its end into memory, as it reads a pipe: measured 2026-11-19,
//! poppler 26.08, `pdftoppm` of one page of a 215 MiB PDF 272 MiB resident
//! and 0.32 s that way against 34 MiB and 0.03 s by path); lmgw's reader
//! reads its stdin by position when it is one. A confined run may reopen
//! its stdin and nothing beside it: its rules carry `--read-stdin`, a read
//! right on the inode descriptor 0 is (`run/start.rs`), not on the spool
//! directory, so it cannot reach another file's spool and cannot write its
//! own (performance round review B2).
//!
//! **A file on disk, not a memfd** (measured 2026-11-19, btrfs on NVMe,
//! poppler 26.08): writing the 215 MiB test PDF took 26–36 ms to the spool
//! file and 79–87 ms to a memfd, and the four default poppler runs read
//! both alike (1.8–2.0 s); a memfd would be a second copy of the upload in
//! memory, the spool file is the page cache's to drop. A PDF is spooled
//! whatever its size: a 83 KiB file's spool took 0.1 ms.
//!
//! The spool file is unlinked when it is made (`tempfile::tempfile_in`), so
//! it has no name, nothing is left of it after a crash, and it is gone when
//! the conversion ends or is dropped. Once written it is made read-only
//! (`0400`), so a tool that is not confined (where Landlock is missing) can
//! open it again through `/dev/stdin` for reading only (review N5). It lives under the data dir
//! (`<data dir>/convert-spool`, private), not in `/tmp`, which is memory on
//! many systems. Each tool run gets an open file description of its own
//! ([`Spool::input`]), so runs side by side do not share a position.
//!
//! This needs `/proc/self/fd` to open a nameless file again: Linux only.
//! Elsewhere, or when the file cannot be made (a full disk, no data dir in
//! a test), there is no spool and each page's tool is sent the bytes, as
//! before; that is said in the log once per file, never to the caller.

use std::path::Path;

use bytes::Bytes;

use super::super::run::Input;

/// The directory spool files are made in, under the data dir.
pub const SPOOL_DIR: &str = "convert-spool";

/// One file's bytes for its pages' tools: on disk where that works, else
/// in memory.
#[derive(Debug)]
pub struct Spool {
    bytes: Bytes,
    #[cfg(target_os = "linux")]
    file: Option<std::fs::File>,
    /// The spool directory it was made for (`None`: a test's, in memory):
    /// where its runs' private directories go too ([`Spool::private_dir`]).
    dir: Option<std::path::PathBuf>,
}

impl Spool {
    /// `bytes` spooled under `dir` (`None`: kept in memory).
    pub async fn of(dir: Option<&Path>, bytes: Bytes) -> Self {
        let at = dir.map(Path::to_path_buf);
        #[cfg(target_os = "linux")]
        {
            let file = match dir {
                Some(dir) => {
                    let (dir, b) = (dir.to_path_buf(), bytes.clone());
                    match tokio::task::spawn_blocking(move || write(&dir, &b)).await {
                        Ok(Ok(f)) => Some(f),
                        Ok(Err(e)) => {
                            tracing::warn!(
                                "a document could not be spooled under the data dir ({e}); its \
                                 pages' tools are sent the file once each instead"
                            );
                            None
                        }
                        Err(_) => None,
                    }
                }
                None => None,
            };
            Self {
                bytes,
                file,
                dir: at,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self { bytes, dir: at }
        }
    }

    /// A private directory for one of this file's runs (page renders, a
    /// poppler run's working directory), `<prefix><pid>-…` under the spool
    /// directory, so what a killed gateway leaves is swept at the next start
    /// (`libreoffice::scratch::sweep_once`) and page renders stay off a
    /// RAM-backed `/tmp` (security round A, hardening 10). Without a spool
    /// directory (a test's), the system's temporary directory.
    pub fn private_dir(&self, prefix: &str) -> std::io::Result<tempfile::TempDir> {
        let Some(dir) = &self.dir else {
            return tempfile::Builder::new().prefix(prefix).tempdir();
        };
        private_root(dir)?;
        tempfile::Builder::new()
            .prefix(&format!("{prefix}{}-", std::process::id()))
            .tempdir_in(dir)
    }

    /// The file's bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// How many bytes each run is sent through a pipe: the file's, where it
    /// is not spooled; none where it is (`pool::charge::piped`).
    pub fn piped_bytes(&self) -> u64 {
        #[cfg(target_os = "linux")]
        if self.file.is_some() {
            return 0;
        }
        self.bytes.len() as u64
    }

    /// The file as one tool run's stdin: an open file description of the
    /// run's own on the spool file, or the bytes for a pipe.
    pub fn input(&self) -> Input {
        #[cfg(target_os = "linux")]
        if let Some(f) = &self.file {
            use std::os::fd::AsRawFd;
            // Opened again by its descriptor: the file has no name.
            match std::fs::File::open(format!("/proc/self/fd/{}", f.as_raw_fd())) {
                Ok(own) => return Input::File(own),
                Err(e) => tracing::warn!("a spooled document could not be opened again: {e}"),
            }
        }
        Input::Bytes(self.bytes.clone())
    }
}

/// What poppler is told to read when its stdin is a file
/// ([`Input::File`]): `/dev/stdin`, which it opens again and reads by
/// position (module doc). Not `fd://0`, which poppler reads to its end into
/// memory.
pub const STDIN_PATH: &str = "/dev/stdin";

/// The file argument a poppler run is given for `input`: [`STDIN_PATH`]
/// for a spool file, `-` for a pipe.
pub fn file_arg(input: &Input) -> &'static str {
    match input {
        Input::File(_) => STDIN_PATH,
        Input::Bytes(_) => "-",
    }
}

/// One file's spool, made by the first stage that renders a page of it and
/// shared by every stage after (text recognition, then the page-reading
/// model): the file is written once per conversion, never once per stage
/// or per page. A file of one page is not spooled (one pipe is as cheap).
#[derive(Debug)]
pub struct Shared {
    dir: Option<std::path::PathBuf>,
    bytes: Bytes,
    spool: tokio::sync::OnceCell<std::sync::Arc<Spool>>,
}

impl Shared {
    /// `bytes`, a file of `pages` pages, to be spooled under `dir` when a
    /// stage first asks.
    pub fn new(dir: Option<&Path>, bytes: Bytes, pages: u32) -> Self {
        Self {
            dir: dir.filter(|_| pages > 1).map(Path::to_path_buf),
            bytes,
            spool: tokio::sync::OnceCell::new(),
        }
    }

    /// A spool made already (the PDF reader's, before its first run).
    pub fn of(spool: std::sync::Arc<Spool>) -> Self {
        Self {
            dir: None,
            bytes: spool.bytes.clone(),
            spool: tokio::sync::OnceCell::new_with(Some(spool)),
        }
    }

    /// The spool, made on the first call.
    pub async fn get(&self) -> std::sync::Arc<Spool> {
        self.spool
            .get_or_init(|| async {
                std::sync::Arc::new(Spool::of(self.dir.as_deref(), self.bytes.clone()).await)
            })
            .await
            .clone()
    }
}

/// `dir`, made private if it is missing.
fn private_root(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// `bytes` in a nameless file under `dir` (made private if it is missing).
#[cfg(target_os = "linux")]
fn write(dir: &Path, bytes: &[u8]) -> std::io::Result<std::fs::File> {
    use std::io::Write;
    private_root(dir)?;
    use std::os::unix::fs::PermissionsExt;
    let mut f = tempfile::tempfile_in(dir)?;
    f.write_all(bytes)?;
    f.flush()?;
    // Read-only from now on (module doc): the gateway's own descriptor
    // stays open for writing, every reopened one is read-only.
    f.set_permissions(std::fs::Permissions::from_mode(0o400))?;
    Ok(f)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Read;

    #[tokio::test]
    async fn a_spooled_file_has_no_name_and_each_run_its_own_position() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().join(SPOOL_DIR);
        let spool = Spool::of(Some(&at), Bytes::from_static(b"0123456789")).await;
        assert_eq!(std::fs::read_dir(&at).unwrap().count(), 0, "unlinked");
        let (Input::File(mut a), Input::File(mut b)) = (spool.input(), spool.input()) else {
            panic!("not on disk");
        };
        let mut four = [0u8; 4];
        a.read_exact(&mut four).unwrap();
        assert_eq!(&four, b"0123");
        // The second description starts at the start, whatever the first read.
        let mut all = String::new();
        b.read_to_string(&mut all).unwrap();
        assert_eq!(all, "0123456789");
        // Without a directory, or with one that cannot be made, the bytes.
        let none = Spool::of(None, Bytes::from_static(b"x")).await;
        assert!(matches!(none.input(), Input::Bytes(b) if &b[..] == b"x"));
        let blocked = dir.path().join("a-file");
        std::fs::write(&blocked, b"").unwrap();
        let none = Spool::of(Some(&blocked.join("under")), Bytes::from_static(b"x")).await;
        assert!(matches!(none.input(), Input::Bytes(_)));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests_stdin;
