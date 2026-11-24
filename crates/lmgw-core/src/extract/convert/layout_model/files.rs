//! The layout model's file: which one, where it lives, and whether what is
//! there is it.
//!
//! One file is pinned ([`HERON`]): docling's own ONNX export of its heron
//! layout model, by repository, commit, name, size and SHA-256, read from
//! the hub's public listing on 2026-11-18 (its card states
//! `license: apache-2.0`, base model `docling-project/docling-layout-heron`).
//! It is not shipped and never fetched unasked: Settings › Documents
//! downloads it through lmgw's download queue ([`super::status::add`]), or
//! the file is put by hand where the page says.
//!
//! It lives in the aux models directory under the path every download
//! takes, `<owner>/<repo>/<file>` (design §3). A file there is the model
//! when it has the pinned size and the pinned SHA-256. Every load reads the
//! file once ([`read_checked`]), hashes those bytes and loads the session
//! from the same bytes, so no file but the pinned one is ever loaded, even
//! one swapped in with the same size and modification time. The verdict
//! is also remembered for the page ([`locate`]: a file refused at a load
//! shows as the wrong file until it changes).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use sha2::{Digest, Sha256};

/// A pinned model file.
#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    /// Its id on the page and in the op.
    pub id: &'static str,
    pub label: &'static str,
    /// What it adds, in plain words.
    pub adds: &'static str,
    pub repo: &'static str,
    /// The commit the file is taken at.
    pub revision: &'static str,
    pub file: &'static str,
    /// Lowercase hex.
    pub sha256: &'static str,
    pub size_bytes: u64,
    pub licence: &'static str,
}

/// docling's heron layout model, its own ONNX export.
pub const HERON: ModelFile = ModelFile {
    id: "layout",
    label: "Layout model (docling heron)",
    adds: "Finds each PDF page's paragraphs, headings, lists, tables, pictures, captions, \
           formulas and code, so text is cut and ordered the way a reader sees it. Runs on \
           the processor, no graphics card needed.",
    repo: "docling-project/docling-layout-heron-onnx",
    revision: "40bde044036bb181c130ddf6c51792187268748f",
    file: "model.onnx",
    sha256: "59c81a3a2923042d85034ffc487f8f47e4854117e879aef89b2b9f728fb4922a",
    size_bytes: 171_220_471,
    licence: "Apache-2.0",
};

impl ModelFile {
    /// Where the file lives under the aux models directory.
    pub fn dest_rel_path(&self) -> String {
        format!("{}/{}", self.repo, self.file)
    }

    /// The page it can be read about and fetched from by hand.
    pub fn url(&self) -> String {
        format!(
            "{}/{}/blob/{}/{}",
            crate::hf::HF_BASE,
            self.repo,
            self.revision,
            self.file
        )
    }
}

/// What is at the model's place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located {
    /// The aux models directory is not set: there is no place yet.
    NoModelsDir,
    /// Nothing is there.
    Missing(PathBuf),
    /// A file of the pinned size; its hash is checked before it is loaded.
    Found(PathBuf),
    /// Something else is there: why it is not the model.
    Wrong(PathBuf, String),
}

/// What is at `model`'s place under `models_dir`, by one `stat` (and the
/// remembered hash, when this file was hashed before).
pub fn locate(model: &ModelFile, models_dir: &str) -> Located {
    if models_dir.trim().is_empty() {
        return Located::NoModelsDir;
    }
    let path = Path::new(models_dir).join(model.dest_rel_path());
    let Ok(meta) = std::fs::metadata(&path) else {
        return Located::Missing(path);
    };
    if !meta.is_file() {
        return Located::Wrong(path, "it is not a file".into());
    }
    if meta.len() != model.size_bytes {
        return Located::Wrong(
            path,
            format!(
                "it has {} bytes, the model has {} (a download that did not finish, or another \
                 file)",
                meta.len(),
                model.size_bytes
            ),
        );
    }
    match remembered(&path, &meta) {
        Some(Err(why)) => Located::Wrong(path, why),
        _ => Located::Found(path),
    }
}

/// One hashed file: what it was, and the verdict.
struct Hashed {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    verdict: Result<(), String>,
}

static HASHED: Mutex<Option<Hashed>> = Mutex::new(None);

fn remembered(path: &Path, meta: &std::fs::Metadata) -> Option<Result<(), String>> {
    let g = HASHED.lock().unwrap_or_else(|e| e.into_inner());
    g.as_ref()
        .filter(|h| h.path == path && h.len == meta.len() && h.modified == meta.modified().ok())
        .map(|h| h.verdict.clone())
}

/// The SHA-256 of `bytes`, lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The file at `path` read once, when its bytes are `model` by their
/// SHA-256 (module doc): what a load gives ONNX Runtime. At most the
/// model's size and one byte is read, so a larger file put there meanwhile
/// is refused without being held. Reads the whole file: call it on a
/// blocking thread.
pub fn read_checked(model: &ModelFile, path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let unreadable = |e: std::io::Error| format!("the model file could not be read ({})", e.kind());
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let meta = file.metadata().map_err(unreadable)?;
    let mut bytes = Vec::with_capacity(usize::try_from(model.size_bytes).unwrap_or(0));
    file.take(model.size_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 != model.size_bytes {
        return Err(format!(
            "it has {}{} bytes, the model has {}",
            bytes.len(),
            if bytes.len() as u64 > model.size_bytes {
                " or more"
            } else {
                ""
            },
            model.size_bytes
        ));
    }
    let got = sha256_hex(&bytes);
    let verdict = match got == model.sha256 {
        true => Ok(()),
        false => Err(format!(
            "its SHA-256 is {got}, the model's is {} (another file under the model's name)",
            model.sha256
        )),
    };
    *HASHED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Hashed {
        path: path.to_path_buf(),
        len: meta.len(),
        modified: meta.modified().ok(),
        verdict: verdict.clone(),
    });
    verdict.map(|()| bytes)
}

/// The model's path when a conversion can use it, or why it cannot, in
/// words for the file's warning.
pub fn usable(models_dir: &str) -> Result<PathBuf, String> {
    match locate(&HERON, models_dir) {
        Located::Found(p) => Ok(p),
        Located::NoModelsDir => Err(format!(
            "no folder for its file is set ({})",
            crate::hf::models_dir_location("aux")
        )),
        Located::Missing(_) => Err(
            "its file is not there any more; download it again under Settings → Documents".into(),
        ),
        Located::Wrong(_, why) => Err(format!("the file at its place is not the model: {why}")),
    }
}
