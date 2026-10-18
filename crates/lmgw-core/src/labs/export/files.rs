//! `format=files`: the Image and Audio labs' files under readable names
//! ([`plan`]), for the owner to keep and play rather than to read back.
//!
//! One group (`group=<id>`) whose runs made one file answers that file as
//! it is: a picture, a speech file, a stream as a WAV, a transcript as text.
//! Anything else is a zip of the files, each run's outputs in order and
//! every group oldest first, and `manifest.jsonl`, a line per file naming
//! the run it came from (model, task, voice, text, the request's
//! parameters, when). A file missing from the data dir is named in a
//! `MISSING.txt` and its manifest line, as the JSONL zip does. The group's
//! inputs (an upload transcribed, an edit's picture) are the owner's own
//! files: the manifest names them, the JSONL export carries them.
//!
//! The Decision lab makes no files: a 400. The zip is written as the JSONL
//! zip is, into an anonymous temporary file, the groups read in batches.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use lmgw_api_types::lab_store::{LabExportQuery, LabKind, FILES_MANIFEST};
use sqlx::SqlitePool;

use super::{blocking, failed, file_name, options, ExportError, ExportFile, BATCH};
use crate::labs::{artifacts, views};
use crate::store::{self, LabArtifactRow, LabGroupFilter};

mod plan;
mod stream;

use plan::{Entry, Namer, Source};

/// Export `lab`'s scope `q` (filter `f`) as its files.
pub(super) async fn export(
    pool: &SqlitePool,
    data_dir: &Path,
    lab: LabKind,
    q: &LabExportQuery,
    f: &LabGroupFilter<'_>,
) -> Result<ExportFile, ExportError> {
    if lab == LabKind::Decision {
        return Err(ExportError::BadRequest(
            "the decision lab makes no files: export it as jsonl or csv".into(),
        ));
    }
    let href = |a: &LabArtifactRow| views::api_href(a.id);
    let mut names = Namer::default();
    let mut zip = None;
    let mut after = None;
    loop {
        let rows = store::lab_groups_oldest(pool, f, after, BATCH)
            .await
            .map_err(failed)?;
        let Some(last) = rows.last() else {
            break;
        };
        after = Some(last.id);
        let groups = views::groups(pool, rows, &href).await.map_err(failed)?;
        let mut entries = Vec::new();
        for g in &groups {
            entries.extend(plan::entries(lab, g, &mut names));
        }
        // One group's one file is answered as it is, when it is there.
        if q.group.is_some() && entries.len() == 1 {
            if let Some(file) = single(data_dir, &entries[0]).await? {
                return Ok(file);
            }
        }
        let z = match zip.take() {
            Some(z) => z,
            None => Zip::start(data_dir).await?,
        };
        zip = Some(z.add(entries).await?);
    }
    let z = match zip {
        Some(z) => z,
        None => Zip::start(data_dir).await?,
    };
    z.finish(file_name(lab, q, "zip")).await
}

/// A media type a header can carry; `application/octet-stream` for one it
/// cannot (a declared type with a line break).
fn header_safe(mime: &str) -> String {
    match axum::http::HeaderValue::from_str(mime) {
        Ok(_) => mime.to_string(),
        Err(_) => "application/octet-stream".into(),
    }
}

/// The one file `e` as the answer, or `None` when its kept file is not in
/// the data dir (the zip says so instead).
async fn single(data_dir: &Path, e: &Entry) -> Result<Option<ExportFile>, ExportError> {
    let data_dir = data_dir.to_path_buf();
    let e = e.clone();
    blocking(move || {
        let mut file = match &e.source {
            Source::Artifact { sha256 } => match open(&data_dir, sha256) {
                Ok(f) => f,
                Err(_) => return Ok(None),
            },
            Source::Wav { sha256, from, pcm } => {
                let Ok(bytes) = read(&data_dir, sha256) else {
                    return Ok(None);
                };
                temp_with(&data_dir, &stream::wav_of(&bytes, *from, *pcm))?
            }
            Source::Text(t) => temp_with(&data_dir, t.as_bytes())?,
        };
        let len = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;
        Ok(Some(ExportFile {
            file,
            len,
            name: e.line.file.clone(),
            mime: header_safe(&e.line.mime),
        }))
    })
    .await
}

fn open(data_dir: &Path, sha: &str) -> std::io::Result<File> {
    let p: PathBuf = artifacts::path(data_dir, sha).map_err(std::io::Error::other)?;
    File::open(p)
}

fn read(data_dir: &Path, sha: &str) -> std::io::Result<Vec<u8>> {
    let p: PathBuf = artifacts::path(data_dir, sha).map_err(std::io::Error::other)?;
    std::fs::read(p)
}

/// An anonymous temporary file under the data dir holding `bytes`.
fn temp_with(data_dir: &Path, bytes: &[u8]) -> std::io::Result<File> {
    std::fs::create_dir_all(data_dir)?;
    let mut f = tempfile::tempfile_in(data_dir)?;
    f.write_all(bytes)?;
    Ok(f)
}

/// A files zip being written: the files so far, their manifest lines in a
/// temporary file of their own (the manifest goes in last, once every
/// file's size and presence is known), the files that were missing.
struct Zip {
    zip: zip::ZipWriter<File>,
    manifest: BufWriter<File>,
    missing: Vec<String>,
    data_dir: PathBuf,
}

impl Zip {
    async fn start(data_dir: &Path) -> Result<Self, ExportError> {
        let data_dir = data_dir.to_path_buf();
        blocking(move || {
            std::fs::create_dir_all(&data_dir)?;
            Ok(Self {
                zip: zip::ZipWriter::new(tempfile::tempfile_in(&data_dir)?),
                manifest: BufWriter::new(tempfile::tempfile_in(&data_dir)?),
                missing: Vec::new(),
                data_dir,
            })
        })
        .await
    }

    /// Write `entries`' files and their manifest lines.
    async fn add(mut self, entries: Vec<Entry>) -> Result<Self, ExportError> {
        blocking(move || {
            for e in entries {
                self.write(e)?;
            }
            Ok(self)
        })
        .await
    }

    fn write(&mut self, mut e: Entry) -> std::io::Result<()> {
        let bytes = match &e.source {
            Source::Artifact { sha256 } => match open(&self.data_dir, sha256) {
                Ok(mut from) => {
                    self.zip.start_file(e.line.file.as_str(), options(true))?;
                    e.line.size = std::io::copy(&mut from, &mut self.zip)?;
                    None
                }
                Err(why) => {
                    self.missing.push(format!("{}: {why}", e.line.file));
                    e.line.missing = true;
                    None
                }
            },
            Source::Wav { sha256, from, pcm } => match read(&self.data_dir, sha256) {
                Ok(kept) => Some((stream::wav_of(&kept, *from, *pcm), true)),
                Err(why) => {
                    self.missing.push(format!("{}: {why}", e.line.file));
                    e.line.missing = true;
                    None
                }
            },
            Source::Text(t) => Some((t.clone().into_bytes(), false)),
        };
        if let Some((bytes, stored)) = bytes {
            self.zip.start_file(e.line.file.as_str(), options(stored))?;
            self.zip.write_all(&bytes)?;
            e.line.size = bytes.len() as u64;
        }
        serde_json::to_writer(&mut self.manifest, &e.line)?;
        self.manifest.write_all(b"\n")
    }

    /// The manifest and the missing files' note go in; the zip, rewound.
    async fn finish(self, name: String) -> Result<ExportFile, ExportError> {
        let (file, len) = blocking(move || {
            let Self {
                mut zip,
                manifest,
                missing,
                ..
            } = self;
            let mut manifest = manifest.into_inner().map_err(|e| e.into_error())?;
            manifest.seek(SeekFrom::Start(0))?;
            zip.start_file(FILES_MANIFEST, options(false))?;
            std::io::copy(&mut manifest, &mut zip)?;
            if !missing.is_empty() {
                zip.start_file("MISSING.txt", options(false))?;
                zip.write_all(
                    format!(
                        "These artifact files were not in the data dir when this export was \
                         written; {FILES_MANIFEST} names their runs:\n{}\n",
                        missing.join("\n")
                    )
                    .as_bytes(),
                )?;
            }
            let mut file = zip.finish().map_err(std::io::Error::other)?;
            let len = file.seek(SeekFrom::End(0))?;
            file.seek(SeekFrom::Start(0))?;
            Ok((file, len))
        })
        .await?;
        Ok(ExportFile {
            file,
            len,
            name,
            mime: "application/zip".into(),
        })
    }
}
