//! The lab store (labs-store design §1): what the Audio, Image and Decision
//! labs keep, designed once for all three. A **group** is one click, a
//! **run** one dispatch to a `/v1` route, an **artifact** media bytes kept as
//! a file. The tables are `store::lab_*`; this module holds what the routes
//! do with them: the artifact files, recording, the usage read, deleting and
//! the export. Slice 2 (§3) adds presets and set runs: a question set over a
//! file's rows as a job, and its metrics. Slice 4 (§5) puts the Image and
//! Audio labs on it: their pictures, speech and uploads as artifacts.

/// The artifact files under `<data_dir>/labs/artifacts/`: write, read,
/// unlink, the lock, the orphan scan, data URLs.
pub mod artifacts;
/// The set-run job (`JobKind::LabBatch`): its executor, start and resume.
pub mod batch;
/// A set run as the API's view: its progress and where its job is.
pub mod batch_view;
/// The cells in flight: a cell is claimed before its dispatch.
pub mod claims;
/// The CSV field writer, shared with the usage export.
pub mod csv;
/// Deleting a group, a set run or clearing a lab, rows first and files
/// after.
pub mod delete;
/// The JSONL and zip export, and the CSV export's file.
pub mod export;
/// The Decision lab's flat CSV table.
pub mod export_csv;
/// The Image and Audio labs' flat CSV tables.
pub mod export_csv_media;
/// A set run's input: CSV and JSONL, the mapping and the label checks.
pub mod ingest;
/// A set run's metrics, as pure functions.
pub mod metrics;
/// Presets as the API reads and writes them.
pub mod presets;
/// Recording a group and a run, one write transaction each.
pub mod record;
/// What a run keeps of a route's answer besides its bytes: the gateway's
/// headers, a failure's code and message, a success's model and tokens.
pub mod run_facts;
/// What a lab keeps: counts, sizes and orphans.
pub mod usage;
/// Stored rows as the API's views.
pub mod views;
