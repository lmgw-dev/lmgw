//! A lab's export (labs-store design §1.7): JSONL, lossless, one
//! `lmgw.lab.v1` line per group; a zip of that JSONL and the files when the
//! scope has artifacts; the lab's flat CSV table; or, for the Image and
//! Audio labs, the files themselves under readable names ([`files`]).
//!
//! Written into an anonymous temporary file under the data dir and handed
//! back with its length, as the Chat export is: a scope is as large as the
//! lab is and is not capped. Groups are read oldest first in batches, a
//! transport unit and no limit, and the file work runs on the blocking pool.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use lmgw_api_types::lab_store::{
    LabExportFormat, LabExportLine, LabExportQuery, LabKind, EXPORT_FORMAT,
};
use sqlx::SqlitePool;
use zip::write::SimpleFileOptions;

use super::{artifacts, export_csv, export_csv_media, views};
use crate::store::{self, LabArtifactRow, LabGroupFilter, LabGroupScope};

/// `format=files`: the Image and Audio labs' files, a zip with a manifest
/// or one group's one file.
mod files;

/// How many groups an export reads at a time.
const BATCH: u32 = 200;

/// A finished export: the file, rewound, and how the answer names it.
#[derive(Debug)]
pub struct ExportFile {
    pub file: File,
    pub len: u64,
    pub name: String,
    pub mime: String,
}

/// Why an export was refused or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// A query that is no scope: a time that is not RFC 3339, a CSV of a lab
    /// that has no table.
    BadRequest(String),
    /// `group` names no group of this lab.
    NotFound(String),
    Failed(String),
}

fn failed(e: impl std::fmt::Display) -> ExportError {
    ExportError::Failed(e.to_string())
}

/// An RFC 3339 time as the stored timestamps write it (UTC, milliseconds),
/// so the two compare as text.
pub fn stored_time(s: &str) -> Result<String, String> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string()
        })
        .map_err(|e| format!("'{s}' is not an RFC 3339 time ({e})"))
}

/// A model name as a file-name part.
fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `lmgw-lab-<lab>[-group-<id> | -batch-<id> | -<model>]-<date>.<ext>`.
pub fn file_name(lab: LabKind, q: &LabExportQuery, ext: &str) -> String {
    let scope = match (q.group, q.batch, q.model.as_deref()) {
        (Some(id), _, _) => format!("-group-{id}"),
        (None, Some(id), _) => format!("-batch-{id}"),
        (None, None, Some(m)) if !m.is_empty() => format!("-{}", slug(m)),
        _ => String::new(),
    };
    format!(
        "lmgw-lab-{}{scope}-{}.{ext}",
        lab.as_str(),
        chrono::Utc::now().format("%Y-%m-%d")
    )
}

/// Each file's path inside a zip: `artifacts/<sha256>.<ext>`, the extension
/// the first row naming it gives, and the files in the order first named.
/// Behind a mutex because the export's `href` is a shared `Fn`.
#[derive(Default)]
struct ZipPaths(std::sync::Mutex<Paths>);

#[derive(Default)]
struct Paths {
    by_sha: HashMap<String, String>,
    /// `(sha256, path)` per file, in the order first named.
    order: Vec<(String, String)>,
}

impl ZipPaths {
    /// The path of `a`'s file, the same for every row naming it.
    fn of(&self, a: &LabArtifactRow) -> String {
        let mut p = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(inside) = p.by_sha.get(&a.sha256) {
            return inside.clone();
        }
        let inside = format!("artifacts/{}.{}", a.sha256, artifacts::ext_of(&a.mime));
        p.by_sha.insert(a.sha256.clone(), inside.clone());
        p.order.push((a.sha256.clone(), inside.clone()));
        inside
    }

    /// `(sha256, path)` per file, in the order first named.
    fn into_files(self) -> Vec<(String, String)> {
        self.0.into_inner().unwrap_or_else(|e| e.into_inner()).order
    }
}

/// Where an export is being written.
enum Sink {
    Plain(BufWriter<File>),
    Zip(Box<zip::ZipWriter<File>>),
}

impl Sink {
    fn write_text(&mut self, text: &str) -> std::io::Result<()> {
        match self {
            Self::Plain(w) => w.write_all(text.as_bytes()),
            Self::Zip(z) => z.write_all(text.as_bytes()),
        }
    }
}

/// Run `f` on the blocking pool.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, ExportError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(failed)?
        .map_err(failed)
}

fn options(stored: bool) -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(if stored {
            zip::CompressionMethod::Stored
        } else {
            zip::CompressionMethod::Deflated
        })
        .large_file(true)
}

/// The lab's flat table's header record.
fn csv_header(lab: LabKind) -> String {
    match lab {
        LabKind::Decision => export_csv::decision_header(),
        LabKind::Image => export_csv_media::image_header(),
        LabKind::Audio => export_csv_media::audio_header(),
    }
}

/// Group `g`'s records in the lab's flat table.
fn csv_rows(lab: LabKind, g: &lmgw_api_types::lab_store::LabGroupView) -> String {
    match lab {
        LabKind::Decision => export_csv::decision_rows(g),
        LabKind::Image => export_csv_media::image_rows(g),
        LabKind::Audio => export_csv_media::audio_rows(g),
    }
}

/// Export `lab`'s scope `q` (labs-store design §1.7).
pub async fn export(
    pool: &SqlitePool,
    data_dir: &Path,
    lab: LabKind,
    q: &LabExportQuery,
) -> Result<ExportFile, ExportError> {
    let from = q
        .from
        .as_deref()
        .map(stored_time)
        .transpose()
        .map_err(ExportError::BadRequest)?;
    let to =
        q.to.as_deref()
            .map(stored_time)
            .transpose()
            .map_err(ExportError::BadRequest)?;
    if let Some(id) = q.group {
        if store::lab_group(pool, lab, id)
            .await
            .map_err(failed)?
            .is_none()
        {
            return Err(ExportError::NotFound(format!(
                "the {} lab has no group {id}",
                lab.as_str()
            )));
        }
    }
    if let Some(id) = q.batch {
        if store::lab_batch(pool, Some(lab), id)
            .await
            .map_err(failed)?
            .is_none()
        {
            return Err(ExportError::NotFound(format!(
                "the {} lab has no set run {id}",
                lab.as_str()
            )));
        }
    }
    let f = LabGroupFilter {
        lab,
        group: q.group,
        model: q.model.as_deref().filter(|m| !m.is_empty()),
        from: from.as_deref(),
        to: to.as_deref(),
        scope: q.batch.map_or(LabGroupScope::All, LabGroupScope::Batch),
    };
    let csv = match q.format {
        LabExportFormat::Files => return files::export(pool, data_dir, lab, q, &f).await,
        LabExportFormat::Csv => true,
        LabExportFormat::Jsonl => false,
    };
    let zipped = !csv
        && store::count_lab_group_artifacts(pool, &f)
            .await
            .map_err(failed)?
            > 0;
    let (name, mime) = if csv {
        (file_name(lab, q, "csv"), "text/csv; charset=utf-8")
    } else if zipped {
        (file_name(lab, q, "zip"), "application/zip")
    } else {
        (file_name(lab, q, "jsonl"), "application/x-ndjson")
    };
    let jsonl_name = file_name(lab, q, "jsonl");

    let dir = data_dir.to_path_buf();
    let mut sink = blocking(move || {
        std::fs::create_dir_all(&dir)?;
        let file = tempfile::tempfile_in(&dir)?;
        Ok(if zipped {
            let mut z = zip::ZipWriter::new(file);
            z.start_file(jsonl_name, options(false))?;
            Sink::Zip(Box::new(z))
        } else {
            Sink::Plain(BufWriter::new(file))
        })
    })
    .await?;
    if csv {
        let header = csv_header(lab);
        sink = blocking(move || {
            sink.write_text(&header)?;
            Ok(sink)
        })
        .await?;
    }

    // The files a zip carries, each once, in the order the groups name them,
    // and every row naming one gets its one path: rows can share a file and
    // differ in type, and the first row's names the extension.
    let paths = ZipPaths::default();
    let href = |a: &LabArtifactRow| {
        if zipped {
            paths.of(a)
        } else {
            views::api_href(a.id)
        }
    };
    let mut after = None;
    loop {
        let rows = store::lab_groups_oldest(pool, &f, after, BATCH)
            .await
            .map_err(failed)?;
        let Some(last) = rows.last() else {
            break;
        };
        after = Some(last.id);
        let groups = views::groups(pool, rows, &href).await.map_err(failed)?;
        let mut text = String::new();
        for g in groups {
            if csv {
                text.push_str(&csv_rows(lab, &g));
            } else {
                let line = LabExportLine {
                    format: EXPORT_FORMAT.into(),
                    lab,
                    group: g,
                };
                text.push_str(&serde_json::to_string(&line).map_err(failed)?);
                text.push('\n');
            }
        }
        sink = blocking(move || {
            sink.write_text(&text)?;
            Ok(sink)
        })
        .await?;
    }

    let files = paths.into_files();
    let data_dir = data_dir.to_path_buf();
    let file = blocking(move || {
        let mut file = match sink {
            Sink::Plain(w) => w.into_inner().map_err(|e| e.into_error())?,
            Sink::Zip(mut z) => {
                let mut missing = Vec::new();
                for (sha, inside) in &files {
                    let src: PathBuf = match artifacts::path(&data_dir, sha) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    match File::open(&src) {
                        Ok(mut from) => {
                            z.start_file(inside.as_str(), options(true))?;
                            std::io::copy(&mut from, &mut z)?;
                        }
                        Err(e) => missing.push(format!("{inside}: {e}")),
                    }
                }
                if !missing.is_empty() {
                    z.start_file("MISSING.txt", options(false))?;
                    z.write_all(
                        format!(
                            "These artifact files were not in the data dir when this export \
                             was written; their rows are in the JSONL:\n{}\n",
                            missing.join("\n")
                        )
                        .as_bytes(),
                    )?;
                }
                z.finish().map_err(std::io::Error::other)?
            }
        };
        let len = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;
        Ok((file, len))
    })
    .await?;
    Ok(ExportFile {
        file: file.0,
        len: file.1,
        name,
        mime: mime.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_is_compared_as_the_store_writes_it() {
        assert_eq!(
            stored_time("2026-10-16T14:00:00+02:00").unwrap(),
            "2026-10-16T12:00:00.000Z"
        );
        assert!(stored_time("yesterday").is_err());
    }

    #[test]
    fn the_file_is_named_after_the_lab_and_its_scope() {
        let q = LabExportQuery {
            group: Some(12),
            ..Default::default()
        };
        let n = file_name(LabKind::Decision, &q, "jsonl");
        assert!(
            n.starts_with("lmgw-lab-decision-group-12-") && n.ends_with(".jsonl"),
            "{n}"
        );
        let q = LabExportQuery {
            model: Some("decide/laya".into()),
            ..Default::default()
        };
        assert!(
            file_name(LabKind::Decision, &q, "csv").starts_with("lmgw-lab-decision-decide-laya-")
        );
        let q = LabExportQuery {
            batch: Some(3),
            ..Default::default()
        };
        assert!(file_name(LabKind::Decision, &q, "csv").starts_with("lmgw-lab-decision-batch-3-"));
    }
}
