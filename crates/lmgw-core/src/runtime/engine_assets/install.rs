//! Download, verify, unpack (Windows design §4.2).
//!
//! A build is installed under `<engines>/<engine>/<tag>-<variant>/`, and
//! only through these steps, one install at a time per build directory —
//! within this process (a mutex) and across every lmgw process sharing the
//! engines root (a file lock, `<engines>/<engine>/.<tag>-<variant>.lock`;
//! production beside a dev instance):
//!
//! 1. each asset (the build, then its cudart companion) is downloaded into
//!    `<engines>/.downloads/<engine>/<dir>/` as `<name>.part`, hashed while
//!    it arrives, and checked against the pin's SHA-256 and size before it
//!    is renamed to `<name>`; a mismatch is the start's error and the part
//!    is removed. An asset already there from an install that did not
//!    finish is hashed again and used when it still matches;
//! 2. the verified archives are unpacked into a staging directory beside
//!    the build's (`.staging-<dir>`), which also gets the install record
//!    ([`MARKER`]: repository, tag, variant, the assets with their digests);
//! 3. the staging directory is renamed to the build's: a build directory
//!    either is a whole verified install or does not exist. The download
//!    directory is removed afterwards. Nothing ever runs from it.
//!
//! A build directory whose record matches the pin is used as it is. One
//! whose record names the same repository, tag and variant with other
//! assets (the release was re-pinned under its tag) is renamed aside to
//! `.<dir>.replaced-<time>` and installed anew; nothing is deleted. One
//! that exists without such a record (unpacked by hand) is refused with
//! the path, never replaced.
//!
//! On Windows a virus scanner opens a new file for a moment, and a rename
//! or removal then fails with "access denied" (os error 5) or a sharing
//! violation (32): those are tried again, [`BUSY_ATTEMPTS`] times with a
//! growing pause ([`busy_pause`]), and the error after the last names the
//! attempts and the time. A failed install keeps its verified downloads.
//!
//! A connection that is not made within [`CONNECT_TIMEOUT`] and a download
//! that delivers nothing for [`STALL`] fail with that figure in their
//! error; there is no bound on how long a download that keeps moving may
//! take.
//!
//! An install runs for one start ([`Job::watch`]). A wait for another lmgw
//! process's install is put on that start's entry, where the dashboard
//! shows it; it has no time bound. When the start is no longer wanted (a
//! stop, the GPU hold or an eviction took its entry), the waits and the
//! downloads stop within [`WATCH_POLL`]: a download's `.part` stays and the
//! next download writes over it. The unpack and the renames are not cut
//! short.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tokio::io::AsyncWriteExt;

use super::pins::{Asset, Variant};
use crate::runtime::launcher::StartWatch;
use lmgw_api_types::units::fmt_bytes;

/// The install record in every build directory.
pub const MARKER: &str = ".lmgw-install.json";

/// Where the downloads are made, under the engines root.
pub const DOWNLOADS_DIR: &str = ".downloads";

/// How long a download may deliver no bytes before it fails (named in the
/// error). A connection that stalls this long is not coming back; one that
/// keeps delivering has no time limit.
pub const STALL: Duration = Duration::from_secs(60);

/// How long the connection to the download server may take (named in the
/// error).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a rename or removal that a virus scanner's open handle denies
/// is tried (module doc), the first try included.
pub const BUSY_ATTEMPTS: u32 = 8;

/// The pause before try `n + 1` (from 1): 100 ms, doubling, at most 2 s;
/// 7.1 s over all [`BUSY_ATTEMPTS`].
pub fn busy_pause(n: u32) -> Duration {
    Duration::from_millis((100u64 << (n - 1).min(5)).min(2000))
}

/// How often a wait or a download asks whether its start is still wanted
/// (module doc).
pub const WATCH_POLL: Duration = Duration::from_millis(250);

/// Where release assets are downloaded from: GitHub, or a test's server.
pub const GITHUB: &str = "https://github.com";

/// What an install directory records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub repo: String,
    pub tag: String,
    /// `None` for a single-asset install (eSpeak NG).
    pub variant: Option<Variant>,
    pub assets: Vec<Asset>,
    /// RFC 3339; not compared.
    #[serde(default)]
    pub installed_at: String,
}

impl Record {
    fn same_install(&self, other: &Record) -> bool {
        self.same_build(other) && self.assets == other.assets
    }

    /// The same repository, tag and variant (the assets may differ: a
    /// re-pin under the same tag).
    fn same_build(&self, other: &Record) -> bool {
        self.repo == other.repo && self.tag == other.tag && self.variant == other.variant
    }
}

/// How a verified archive becomes files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unpack {
    Zip,
    /// A Windows Installer package, its files laid out as its Directory
    /// table says ([`super::msi`]).
    Msi,
}

/// One install job: the directory, what it records, how to fetch and unpack
/// each asset, and the check the unpacked tree has to pass.
pub struct Job<'a> {
    /// The build directory (`<engines>/llama/b12244-cuda12`).
    pub dir: PathBuf,
    pub record: Record,
    pub unpack: Unpack,
    /// What it is, for sentences ("llama.cpp b12244 cuda12").
    pub label: String,
    /// Checks the staged tree before it becomes the build directory (the
    /// executable is where the launcher will look for it).
    pub check: &'a (dyn Fn(&Path) -> Result<(), String> + Send + Sync),
    /// The start this install is for (module doc).
    pub watch: &'a dyn StartWatch,
}

/// The installer of one engines root.
pub struct Installer {
    root: PathBuf,
    base: String,
    http: reqwest::Client,
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl Installer {
    /// Downloads from `base` ([`GITHUB`]) into `root` (`<data>/engines`).
    pub fn new(root: PathBuf, base: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(STALL)
            .build()
            .unwrap_or_default();
        Self {
            root,
            base: base.trim_end_matches('/').to_string(),
            http,
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<base>/<repo>/releases/download/<tag>/<name>`.
    pub fn url(&self, repo: &str, tag: &str, name: &str) -> String {
        format!("{}/{repo}/releases/download/{tag}/{name}", self.base)
    }

    /// The record in `dir`, if it holds one that can be read.
    pub fn record_in(dir: &Path) -> Option<Record> {
        let s = std::fs::read_to_string(dir.join(MARKER)).ok()?;
        serde_json::from_str(&s).ok()
    }

    /// `dir` holds a verified install of `record`.
    pub fn is_installed(dir: &Path, record: &Record) -> bool {
        Self::record_in(dir).is_some_and(|r| r.same_install(record))
    }

    /// Install `job` unless it is installed already; the build directory.
    pub async fn ensure(&self, job: Job<'_>) -> Result<PathBuf, String> {
        let lock = {
            let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
            locks.entry(job.dir.clone()).or_default().clone()
        };
        let _turn = while_wanted(job.watch, &job.label, async { Ok(lock.lock().await) }).await?;
        if Self::is_installed(&job.dir, &job.record) {
            return Ok(job.dir);
        }
        let name = job
            .dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let parent = job
            .dir
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", job.dir.display()))?
            .to_path_buf();
        let engine = parent
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let _held = while_wanted(
            job.watch,
            &job.label,
            lock_across_processes(&parent.join(format!(".{name}.lock")), &job.label, job.watch),
        )
        .await?;
        // Another lmgw may have installed it while this one waited.
        if Self::is_installed(&job.dir, &job.record) {
            return Ok(job.dir);
        }
        if job.dir.exists() {
            match Self::record_in(&job.dir) {
                Some(r) if r.same_build(&job.record) => {
                    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
                    let aside = parent.join(format!(".{name}.replaced-{stamp}"));
                    busy_retry(&format!("moving {} aside", job.dir.display()), || {
                        std::fs::rename(&job.dir, &aside)
                    })
                    .await?;
                    tracing::warn!(
                        "{} holds {} with other assets than lmgw's pin (the release was re-pinned \
                         under its tag): moved aside to {}, and the pinned build is installed",
                        job.dir.display(),
                        job.label,
                        aside.display()
                    );
                }
                _ => {
                    return Err(format!(
                        "{} exists but is not lmgw's verified install of {} (no matching {MARKER} \
                         in it): move it away and lmgw downloads the pinned build again",
                        job.dir.display(),
                        job.label
                    ))
                }
            }
        }
        let downloads = self.root.join(DOWNLOADS_DIR).join(&engine).join(&name);
        std::fs::create_dir_all(&downloads)
            .map_err(|e| format!("creating {}: {e}", downloads.display()))?;
        let started = Instant::now();
        let mut archives = Vec::new();
        for a in &job.record.assets {
            let done = downloads.join(&a.name);
            if verified(&done, a).await {
                tracing::info!(
                    "{} for {} is already downloaded and still matches its pin: used as it is",
                    a.name,
                    job.label
                );
                archives.push(done);
                continue;
            }
            let url = self.url(&job.record.repo, &job.record.tag, &a.name);
            let download = self.download(&url, a, &downloads, &job.label);
            archives.push(while_wanted(job.watch, &job.label, download).await?);
        }
        let staging = parent.join(format!(".staging-{name}"));
        remove_if_there(&staging).await?;
        std::fs::create_dir_all(&staging)
            .map_err(|e| format!("creating {}: {e}", staging.display()))?;
        let unpack = job.unpack;
        let (s, a) = (staging.clone(), archives.clone());
        tokio::task::spawn_blocking(move || {
            a.iter()
                .try_for_each(|archive| unpack_into(unpack, archive, &s))
        })
        .await
        .map_err(|e| format!("the unpack task failed: {e}"))??;
        (job.check)(&staging).map_err(|e| format!("{}: {e}", job.label))?;
        let mut record = job.record.clone();
        record.installed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let json = serde_json::to_string_pretty(&record).unwrap_or_default();
        std::fs::write(staging.join(MARKER), json)
            .map_err(|e| format!("writing the install record into {}: {e}", staging.display()))?;
        busy_retry(
            &format!("moving {} to {}", staging.display(), job.dir.display()),
            || std::fs::rename(&staging, &job.dir),
        )
        .await?;
        if let Err(e) = remove_if_there(&downloads).await {
            tracing::warn!(
                "the verified downloads of {} stay in {} ({e})",
                job.label,
                downloads.display()
            );
        }
        tracing::info!(
            "installed {} into {} in {:.1} s",
            job.label,
            job.dir.display(),
            started.elapsed().as_secs_f64()
        );
        Ok(job.dir)
    }

    /// One asset into `dir`, verified; its path.
    async fn download(
        &self,
        url: &str,
        asset: &Asset,
        dir: &Path,
        label: &str,
    ) -> Result<PathBuf, String> {
        let part = dir.join(format!("{}.part", asset.name));
        let done = dir.join(&asset.name);
        tracing::info!(
            "downloading {} ({}) for {label} from {url}",
            asset.name,
            fmt_bytes(asset.size)
        );
        let started = Instant::now();
        let failed = |e: String| format!("downloading {} for {label} from {url}: {e}", asset.name);
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| failed(stalled(e)))?;
        if !resp.status().is_success() {
            return Err(failed(format!("the server answered {}", resp.status())));
        }
        let mut file = tokio::fs::File::create(&part)
            .await
            .map_err(|e| failed(format!("creating {}: {e}", part.display())))?;
        let mut hash = sha2::Sha256::new();
        let mut got: u64 = 0;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    drop(file);
                    let _ = std::fs::remove_file(&part);
                    return Err(failed(stalled(e)));
                }
            };
            got += chunk.len() as u64;
            if got > asset.size {
                drop(file);
                let _ = std::fs::remove_file(&part);
                return Err(failed(format!(
                    "it is larger than the pinned {} bytes",
                    asset.size
                )));
            }
            hash.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| failed(format!("writing {}: {e}", part.display())))?;
        }
        file.flush()
            .await
            .map_err(|e| failed(format!("writing {}: {e}", part.display())))?;
        drop(file);
        let digest = hex::encode(hash.finalize());
        if got != asset.size || digest != asset.sha256 {
            let _ = std::fs::remove_file(&part);
            return Err(failed(format!(
                "it is {got} bytes with SHA-256 {digest}, and the pin is {} bytes with SHA-256 {} — \
                 the download was removed and nothing of it is used",
                asset.size, asset.sha256
            )));
        }
        busy_retry(&format!("renaming {}", part.display()), || {
            std::fs::rename(&part, &done)
        })
        .await
        .map_err(failed)?;
        tracing::info!(
            "downloaded and verified {} ({}, SHA-256 {}) in {:.1} s",
            asset.name,
            fmt_bytes(got),
            &digest[..12],
            started.elapsed().as_secs_f64()
        );
        Ok(done)
    }
}

/// reqwest's error, with the bound named when one ended it: the connect
/// timeout for a connection that was not made, the stall bound for a read.
fn stalled(e: reqwest::Error) -> String {
    if e.is_connect() {
        if e.is_timeout() {
            format!(
                "no connection was made within {} s ({e}); lmgw gives up on connecting to the \
                 download server after that long",
                CONNECT_TIMEOUT.as_secs()
            )
        } else {
            format!("the connection failed: {e}")
        }
    } else if e.is_timeout() {
        format!(
            "no data arrived for {} s ({e}); lmgw gives up on a download after that long a stall",
            STALL.as_secs()
        )
    } else {
        e.to_string()
    }
}

/// `path` holds `asset`'s bytes: its size, then its SHA-256 (read off the
/// runtime's threads).
async fn verified(path: &Path, asset: &Asset) -> bool {
    if std::fs::metadata(path).map(|m| m.len()).ok() != Some(asset.size) {
        return false;
    }
    let (p, want) = (path.to_path_buf(), asset.sha256.clone());
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut f = std::fs::File::open(&p)?;
        let mut hash = sha2::Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
        }
        Ok::<_, std::io::Error>(hex::encode(hash.finalize()) == want)
    })
    .await
    .is_ok_and(|r| r.is_ok_and(|same| same))
}

/// The error a virus scanner's open handle gives a rename or removal on
/// Windows: access denied (os error 5) or a sharing violation (32). A
/// genuine access denied (a read-only directory, no permission) cannot be
/// told apart from it, so it is tried again too and fails after the
/// [`BUSY_ATTEMPTS`], its error naming them.
fn busy(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
        || (cfg!(windows) && e.raw_os_error() == Some(32))
}

/// `op`, tried again while it is [`busy`] (module doc); `what` names it in
/// the error.
pub async fn busy_retry<T>(
    what: &str,
    op: impl FnMut() -> std::io::Result<T>,
) -> Result<T, String> {
    busy_retry_with(what, busy_pause, op).await
}

/// [`busy_retry`] with its pauses from `pause` (a test's are zero).
async fn busy_retry_with<T>(
    what: &str,
    pause: fn(u32) -> Duration,
    mut op: impl FnMut() -> std::io::Result<T>,
) -> Result<T, String> {
    let started = Instant::now();
    let mut n = 1;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if busy(&e) && n < BUSY_ATTEMPTS => {
                tokio::time::sleep(pause(n)).await;
                n += 1;
            }
            Err(e) if busy(&e) => {
                return Err(format!(
                    "{what} failed {n} times over {:.1} s ({e}; on Windows a virus scanner holds a \
                     new file open for a moment, this one longer)",
                    started.elapsed().as_secs_f64()
                ))
            }
            Err(e) => return Err(format!("{what}: {e}")),
        }
    }
}

async fn remove_if_there(dir: &Path) -> Result<(), String> {
    busy_retry(
        &format!("removing {}", dir.display()),
        || match std::fs::remove_dir_all(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        },
    )
    .await
}

/// `step` of installing `label`, unless the start `watch` speaks for is no
/// longer wanted first (asked every [`WATCH_POLL`]): then `step` is dropped
/// and the error says why. Only around a step that a drop leaves nothing
/// half-done of (module doc).
async fn while_wanted<T>(
    watch: &dyn StartWatch,
    label: &str,
    step: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    let unwanted = async {
        while watch.wanted() {
            tokio::time::sleep(WATCH_POLL).await;
        }
    };
    tokio::select! {
        r = step => r,
        () = unwanted => Err(format!(
            "the start that needed {label} was stopped while it was being installed, so the \
             install stopped with it; the next start installs it"
        )),
    }
}

/// The cross-process install lock at `path` (module doc), held until the
/// file is dropped. A wait for another lmgw's install is a note on the
/// start's entry (`watch`) and in the log.
async fn lock_across_processes(
    path: &Path,
    label: &str,
    watch: &dyn StartWatch,
) -> Result<std::fs::File, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("opening the install lock {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => return Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            let note = format!(
                "waiting for another lmgw process to finish installing {label} (it holds {})",
                path.display()
            );
            tracing::info!("{note}");
            watch.note(note);
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(format!("taking the install lock {}: {e}", path.display()))
        }
    }
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || file.lock().map(|()| file))
        .await
        .map_err(|e| format!("waiting for the install lock: {e}"))?
        .map_err(|e| format!("taking the install lock {}: {e}", p.display()))
}

/// Unpack one verified archive into `dir`.
pub fn unpack_into(how: Unpack, archive: &Path, dir: &Path) -> Result<(), String> {
    match how {
        Unpack::Zip => {
            let f = std::fs::File::open(archive)
                .map_err(|e| format!("opening {}: {e}", archive.display()))?;
            let mut zip = zip::ZipArchive::new(std::io::BufReader::new(f))
                .map_err(|e| format!("{} is no readable zip: {e}", archive.display()))?;
            zip.extract(dir)
                .map_err(|e| format!("unpacking {}: {e}", archive.display()))
        }
        Unpack::Msi => super::msi::extract(archive, dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_matches_by_everything_but_its_time() {
        let a = Record {
            repo: "o/r".into(),
            tag: "b1".into(),
            variant: Some(Variant::Cpu),
            assets: vec![Asset {
                name: "x.zip".into(),
                sha256: "0".repeat(64),
                size: 1,
            }],
            installed_at: "2026-11-10T00:00:00Z".into(),
        };
        let mut b = a.clone();
        b.installed_at.clear();
        assert!(a.same_install(&b));
        b.tag = "b2".into();
        assert!(!a.same_install(&b));
        let mut c = a.clone();
        c.assets[0].sha256 = "1".repeat(64);
        assert!(!a.same_install(&c));
        assert!(a.same_build(&c), "a re-pin under the same tag");
    }

    #[test]
    fn the_busy_pauses_grow_to_2_s_and_sum_to_7_1_s() {
        let pauses: Vec<u64> = (1..BUSY_ATTEMPTS)
            .map(|n| busy_pause(n).as_millis() as u64)
            .collect();
        assert_eq!(pauses, [100, 200, 400, 800, 1600, 2000, 2000]);
        assert_eq!(pauses.iter().sum::<u64>(), 7100);
    }

    #[tokio::test]
    async fn a_denied_rename_is_tried_again_and_its_error_names_the_attempts() {
        let none = |_| Duration::ZERO;
        let mut n = 0;
        let ok = busy_retry_with("x", none, || {
            n += 1;
            if n < 3 {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            } else {
                Ok(n)
            }
        })
        .await;
        assert_eq!(ok, Ok(3));
        let err = busy_retry_with::<()>("moving a to b", none, || {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .await
        .unwrap_err();
        assert!(
            err.starts_with("moving a to b failed 8 times over"),
            "{err}"
        );
        let other = busy_retry_with::<()>("y", none, || {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        })
        .await
        .unwrap_err();
        assert!(!other.contains("times"), "{other}");
    }
}
