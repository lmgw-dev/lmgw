//! Documents lab: the dashboard's playground for the document conversion
//! (document-conversion design §10, WP12), the Image lab's sibling for
//! `POST /v1/documents/convert`, its async jobs and the docling-compatible
//! `POST /v1/convert/file`. The page is the SPA's Documents lab; this module
//! is its server (`/documents-lab/api/*`).
//!
//! Every request is kept in the lab store (labs-store design §5): the page
//! makes a group of one cell from its form and its files first
//! (`/documents-lab/api/groups`, the uploads the group's input artifacts),
//! then runs it (`/groups/{id}/run`). The run builds the multipart a client
//! would send from the stored form and the kept files and hands it to the
//! route's own handler in process, with the route's `max_body_mb` bound, so
//! what the page shows is what the API answers: the options judged, the tool
//! pool, the warnings, the usage row. Only the auth middleware and the HTTP
//! hop are skipped, the trade the other labs make.
//!
//! The run is the route's answer with each document's text (its markdown,
//! text or structure; docling's `md_content` and `text_content`; each entry
//! of docling's zip) an output artifact named `lab-artifact:<id>` where it
//! was ([`texts`]), so the history stays small however long the documents
//! are. An `async` request answers at once with its job; the run is the
//! job's result, kept by a task that waits for the job to end while the page
//! follows the job at `/v1/documents/jobs/{id}` ([`job`]).

/// The run's multipart body streamed from the kept files.
mod body;
/// `POST /documents-lab/api/groups`: the form and the uploads kept as a
/// group.
mod groups;
/// Waiting for an async run's job and keeping its result.
mod job;
/// Recording a route's answer as the group's run.
mod kept;
/// `POST /documents-lab/api/groups/{id}/run`.
mod run;
/// The texts in an answer, found where they are, and the answer with each
/// replaced by its artifact.
mod texts;

pub use groups::create;
pub use run::run;
