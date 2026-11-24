//! The admin document shared by the tests of one test run (debug builds
//! only): building it takes seconds (3.4–3.8 s on a test machine,
//! 2026-11-20), and under nextest every test is a process of its own, so
//! every test that reads it (a typed answer's `common::round_trips`, the
//! audit log's redaction list) used to build it again.
//!
//! [`ENV`] names a JSON file. nextest's setup script
//! (`scripts/nextest-test-env.sh`, wired in `.config/nextest.toml`)
//! sets it for lmgw-core's `it` binary. The first process that needs the
//! document in a run builds it, holding a lock next to the file, and writes
//! it with its [stamp](stamp): the run's `NEXTEST_RUN_ID` and the path of the
//! executable. Every later process of the same run and binary reads it; a
//! file with another stamp (an earlier run, another binary) is built again
//! and written over, so the file never outlives the build that wrote it. A
//! file that does not parse is a hard error, never a reason to build.
//!
//! Without [`ENV`] (plain `cargo test`, a dev instance) and in a release
//! build this is not here at all: `admin_doc` builds the document itself.
//! `tests/it/openapi_doc_cache.rs` builds it for real and holds it to what
//! the file says.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The environment variable naming the cache file.
pub const ENV: &str = "LMGW_OPENAPI_DOC_CACHE";

#[derive(Serialize)]
struct Out<'a> {
    stamp: &'a str,
    doc: &'a Value,
}

#[derive(Deserialize)]
struct In {
    stamp: String,
    doc: Value,
}

/// The admin document: from the file [`ENV`] names when it is set, else
/// `build()`.
pub(super) fn admin(build: fn() -> Value) -> Value {
    match std::env::var_os(ENV) {
        Some(path) => load_or_build(Path::new(&path), &stamp(), build),
        None => build(),
    }
}

/// This process's run and build: the nextest run (`NEXTEST_RUN_ID`, set for
/// every test) and the executable. Without a run id there is no run to
/// share a document with, and the setup script is the only thing that sets
/// [`ENV`]: a hard error.
pub fn stamp() -> String {
    let run = std::env::var("NEXTEST_RUN_ID").unwrap_or_else(|_| {
        panic!(
            "{ENV} is set but NEXTEST_RUN_ID is not: the cached API document is \
             shared within one nextest run only (unset {ENV})"
        )
    });
    let exe = std::env::current_exe().expect("the test's own executable");
    format!("{run} {}", exe.display())
}

/// The document `path` holds under `stamp`, or `build()` written there
/// under it. One process builds at a time (the lock file next to `path`);
/// one waiting for it reads what it wrote.
///
/// # Panics
///
/// When the file is there and does not parse, or cannot be read, locked or
/// written.
pub fn load_or_build(path: &Path, stamp: &str, build: impl FnOnce() -> Value) -> Value {
    if let Some(doc) = read(path, stamp) {
        return doc;
    }
    let lock_path = with_suffix(path, "lock");
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .unwrap_or_else(|e| panic!("{}: {e}", lock_path.display()));
    lock.lock()
        .unwrap_or_else(|e| panic!("locking {}: {e}", lock_path.display()));
    if let Some(doc) = read(path, stamp) {
        return doc;
    }
    let doc = build();
    let tmp = with_suffix(path, "tmp");
    let written = File::create(&tmp).and_then(|mut f| {
        serde_json::to_writer(&mut f, &Out { stamp, doc: &doc })?;
        f.flush()
    });
    written.unwrap_or_else(|e| panic!("writing {}: {e}", tmp.display()));
    std::fs::rename(&tmp, path)
        .unwrap_or_else(|e| panic!("renaming {} to {}: {e}", tmp.display(), path.display()));
    doc
}

/// The document in `path` when the file is there and carries `stamp`.
///
/// # Panics
///
/// When the file is there and does not parse: it is only ever written
/// whole (a temporary file renamed over it), so a broken one is a fault to
/// look at, not a cache miss.
pub fn read(path: &Path, stamp: &str) -> Option<Value> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => panic!("{}: {e}", path.display()),
    };
    let file: In = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "{} (the cached API document, {ENV}) does not parse: {e}",
            path.display()
        )
    });
    (file.stamp == stamp).then_some(file.doc)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}
