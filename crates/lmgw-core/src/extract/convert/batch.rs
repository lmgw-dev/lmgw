//! Several files per request (document-conversion design §7.6, D18): every
//! file converted side by side as tool slots free up, its entry in request
//! order whatever order they finish in, the outcomes counted, and the HTTP
//! status the answer goes out with.

use std::collections::BTreeSet;
use std::sync::Arc;

use lmgw_api_types::documents::convert::{
    ConvertAnswer, ConvertError, ConvertSummary, ConvertWarning, ConvertedDocument, DocStatus,
    ErrorCode, ToolUsed,
};
use tokio::sync::Semaphore;

use super::budget::OutputBudget;
use super::caps::ToolId;
use super::file::{self, Input};
use super::jobs::results::Spool;
use super::options::Plan;
use super::pool;
use super::tools::{Fact, ToolFacts};

/// Convert `files` by `plan`; `notes` are the request's own warnings.
/// Answers the HTTP status with the body. The files' converted output
/// together is bounded by [`MAX_UNCOMPRESSED_BYTES`](crate::extract::office::MAX_UNCOMPRESSED_BYTES).
pub async fn convert(
    files: Vec<Input>,
    plan: &Plan,
    facts: &ToolFacts,
    notes: Vec<ConvertWarning>,
) -> (u16, ConvertAnswer) {
    let each = Each {
        deadline: plan.deadline,
        ..Default::default()
    };
    convert_each(files, plan, facts, notes, OutputBudget::request(), &each).await
}

/// [`convert`] against `budget`: the file whose output crosses it fails with
/// `output_too_large`, files already converted stay converted, and files
/// that finish later fail the same way.
pub async fn convert_within(
    files: Vec<Input>,
    plan: &Plan,
    facts: &ToolFacts,
    notes: Vec<ConvertWarning>,
    budget: OutputBudget,
) -> (u16, ConvertAnswer) {
    convert_each(files, plan, facts, notes, budget, &Each::default()).await
}

/// What [`convert_each`] does beside converting: each file's entry sent to
/// `done` as soon as it has its outcome (an async job's progress), and a
/// deadline per file (`document_timeout`: the native option through
/// [`Plan::deadline`], docling's through its adapter).
#[derive(Default)]
pub struct Each {
    pub done: Option<tokio::sync::mpsc::UnboundedSender<ConvertedDocument>>,
    /// Counted from the file's first slot in the tool pool, as docling's
    /// `document_timeout` is conversion time.
    pub deadline: Option<std::time::Duration>,
    /// Where the files' bytes wait: the files given to [`convert_each`]
    /// carry no bytes then, each is read when it is admitted and deleted
    /// when its file is done. At most as many files are admitted at once as
    /// the pool has slots, so a queued job's uploads stay on disk.
    pub spool: Option<Spool>,
}

/// [`convert_within`], with `each`'s hooks.
pub async fn convert_each(
    files: Vec<Input>,
    plan: &Plan,
    facts: &ToolFacts,
    notes: Vec<ConvertWarning>,
    budget: OutputBudget,
    each: &Each,
) -> (u16, ConvertAnswer) {
    let budget = Arc::new(budget);
    let admitted = match &each.spool {
        Some(_) => usize::try_from(pool::global().facts().slots)
            .unwrap_or(usize::MAX)
            .clamp(1, Semaphore::MAX_PERMITS),
        None => Semaphore::MAX_PERMITS,
    };
    let admit = Semaphore::new(admitted);
    // Each file's future boxed: a request of many files must not grow the
    // handler's stack by one inlined conversion each.
    let done = futures::future::join_all(files.into_iter().map(|f| {
        Box::pin(async {
            // Never closed, so the permit always comes.
            let _permit = admit.acquire().await.ok();
            let index = f.index;
            let f = match &each.spool {
                Some(spool) => match spool.load(f).await {
                    Ok(f) => f,
                    Err(d) => return finished(*d, each),
                },
                None => f,
            };
            let d = within_deadline(f, plan, facts, &budget, each.deadline).await;
            if let Some(spool) = &each.spool {
                spool.done(index).await;
            }
            finished(d, each)
        })
    }))
    .await;
    let used: BTreeSet<ToolId> = done
        .iter()
        .flat_map(|d| d.tool.into_iter().chain(d.also.iter().copied()))
        .collect();
    let documents: Vec<ConvertedDocument> = done.into_iter().map(|d| d.doc).collect();
    let summary = summary(&documents);
    let status = status(&documents);
    let tools = used
        .into_iter()
        .filter_map(|id| match facts.fact(id) {
            Some(Fact::Found(f)) => Some(ToolUsed {
                id: id.as_str().to_string(),
                version: f.version.clone(),
            }),
            _ => None,
        })
        .collect();
    (
        status,
        ConvertAnswer {
            documents,
            summary,
            tools,
            warnings: notes,
        },
    )
}

/// A file's outcome, sent to whoever watches (an async job, which stores
/// it). What stays here for the job's end is the file's outcome without
/// its content (its text, structure and pictures): the job reads only its
/// state from it, and the stored entry has the rest (§14 performance round:
/// a job no longer holds every file's output until its last file is done).
fn finished(mut d: file::Done, each: &Each) -> file::Done {
    if let Some(tx) = &each.done {
        let light = ConvertedDocument {
            content: None,
            structure: None,
            pictures: Vec::new(),
            text: None,
            name: d.doc.name.clone(),
            warnings: d.doc.warnings.clone(),
            stats: d.doc.stats.clone(),
            error: d.doc.error.clone(),
            format: d.doc.format.clone(),
            ..d.doc
        };
        let full = std::mem::replace(&mut d.doc, light);
        let _ = tx.send(full);
    }
    d
}

/// A failed file without a tool.
pub(super) fn failed_file(
    index: u32,
    name: String,
    code: ErrorCode,
    message: String,
) -> file::Done {
    file::Done {
        doc: ConvertedDocument {
            index,
            name,
            status: DocStatus::Failed,
            format: None,
            parts: 0,
            content: None,
            structure: None,
            pictures: Vec::new(),
            text: None,
            warnings: Vec::new(),
            stats: Default::default(),
            error: Some(ConvertError {
                code,
                message,
                tool: None,
            }),
        },
        tool: None,
        also: Vec::new(),
    }
}

/// One file, failed by name when `deadline` passes first (its running
/// tools are stopped as the conversion is dropped). The file's clock
/// ([`pool::clock`]) starts at its first slot in the tool pool and stops
/// while it waits for another, for the layout model or for a model's turn:
/// a file that queues is not late. A file whose page-reading model stage
/// took the end over ([`pool::clock::FileClock::hand_over`]) is not thrown
/// away at the deadline: that stage ends its calls there and the file is
/// finished from what was read.
async fn within_deadline(
    f: Input,
    plan: &Plan,
    facts: &ToolFacts,
    budget: &Arc<OutputBudget>,
    deadline: Option<std::time::Duration>,
) -> file::Done {
    let Some(limit) = deadline else {
        return file::convert(f, plan, facts, budget).await;
    };
    let (index, name) = (f.index, f.name.clone());
    let clock = pool::clock::FileClock::new(limit);
    let conversion = pool::clock::scope(clock.clone(), file::convert(f, plan, facts, budget));
    tokio::pin!(conversion);
    tokio::select! {
        d = &mut conversion => return d,
        () = clock.expiry() => {}
    }
    if clock.handed_over() {
        return conversion.await;
    }
    failed_file(
        index,
        name,
        ErrorCode::DeadlineExceeded,
        format!(
            "the file was not converted within the caller's document_timeout of {} s; its \
             tools were stopped",
            limit.as_secs_f64()
        ),
    )
}

/// The files' outcomes, counted.
pub fn summary(docs: &[ConvertedDocument]) -> ConvertSummary {
    let count = |s: DocStatus| docs.iter().filter(|d| d.status == s).count() as u32;
    ConvertSummary {
        files: docs.len() as u32,
        converted: count(DocStatus::Converted),
        partial: count(DocStatus::Partial),
        failed: count(DocStatus::Failed),
        skipped: count(DocStatus::Skipped),
    }
}

/// D18: 200 when a file was converted or partly converted; otherwise one
/// file keeps its own error's status, several answer 422.
pub fn status(docs: &[ConvertedDocument]) -> u16 {
    let read = docs
        .iter()
        .any(|d| matches!(d.status, DocStatus::Converted | DocStatus::Partial));
    if read {
        return 200;
    }
    match docs {
        [one] => one.error.as_ref().map_or(422, |e| e.code.status()),
        _ => 422,
    }
}

#[cfg(test)]
mod tests {
    use lmgw_api_types::documents::convert::ErrorCode;
    use lmgw_api_types::documents::InstallPlatform;

    use super::*;
    use crate::extract::convert::options;
    use crate::extract::test_files;

    fn input(index: u32, bytes: &[u8]) -> Input {
        Input {
            index,
            name: format!("example-{index}"),
            bytes: Ok(bytes::Bytes::copy_from_slice(bytes)),
        }
    }

    async fn run(files: Vec<Input>) -> (u16, ConvertAnswer) {
        let (plan, notes) = options::plan(&Default::default(), InstallPlatform::Fedora).unwrap();
        convert(files, &plan, &ToolFacts::all_missing(), notes).await
    }

    #[tokio::test]
    async fn one_bad_file_never_fails_the_others_and_order_is_kept() {
        let doc = test_files::docx(&["Hello"]);
        let (status, a) = run(vec![
            input(0, &doc),
            input(1, b"\x89PNG\r\n\x1a\n"),
            input(2, b"plain text"),
        ])
        .await;
        assert_eq!(status, 200);
        let idx: Vec<u32> = a.documents.iter().map(|d| d.index).collect();
        assert_eq!(idx, [0, 1, 2]);
        assert_eq!(a.documents[1].status, DocStatus::Failed);
        assert_eq!(
            (a.summary.files, a.summary.converted, a.summary.failed),
            (3, 2, 1)
        );
        assert!(a.tools.is_empty());
    }

    #[tokio::test]
    async fn the_file_that_crosses_the_output_budget_fails_by_name() {
        // Three 40-byte texts against a 100-byte budget: two fit in whatever
        // order they finish, the third crosses it.
        let text = [b'x'; 40];
        let (plan, notes) = options::plan(&Default::default(), InstallPlatform::Fedora).unwrap();
        let files = vec![input(0, &text), input(1, &text), input(2, &text)];
        let (status, a) = convert_within(
            files,
            &plan,
            &ToolFacts::all_missing(),
            notes,
            OutputBudget::with_limit(100),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!((a.summary.converted, a.summary.failed), (2, 1));
        let bad = a
            .documents
            .iter()
            .find(|d| d.status == DocStatus::Failed)
            .unwrap();
        let e = bad.error.as_ref().unwrap();
        assert_eq!(e.code, ErrorCode::OutputTooLarge);
        assert!(e.message.contains("100 B (MAX_UNCOMPRESSED_BYTES"), "{e:?}");
        assert!(a
            .documents
            .iter()
            .filter(|d| d.status == DocStatus::Converted)
            .all(|d| d.content.as_deref().map(str::len) == Some(40)));
    }

    #[tokio::test]
    async fn when_nothing_converts_one_file_keeps_its_code_and_several_are_422() {
        let pdf = test_files::pdf(&[Some("Hello")]);
        let (status, a) = run(vec![input(0, &pdf)]).await;
        assert_eq!(status, 415);
        assert_eq!(
            a.documents[0].error.as_ref().unwrap().code,
            ErrorCode::ToolMissing
        );
        let (status, _) = run(vec![input(0, b"\0\x01\x02\x9f")]).await;
        assert_eq!(status, 415);
        let (status, a) = run(vec![input(0, &pdf), input(1, b"\x89PNG\r\n\x1a\n")]).await;
        assert_eq!(status, 422);
        assert_eq!(a.summary.failed, 2);
    }
}
