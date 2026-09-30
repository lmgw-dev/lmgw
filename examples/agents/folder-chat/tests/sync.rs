//! The sync, offline: quickdoc's fixture embedder, a temp folder, no gateway.

mod common;

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use common::*;
use folder_chat::config::INDEX_DIR_NAME;
use folder_chat::index::IndexDir;
use folder_chat::scan::SkipReason;
use folder_chat::sync::{
    AbortKind, SyncError, SyncEvent, SyncOptions, TextlessPages, EMBED_BATCH_SIZE,
};
use quickdoc_core::embed::FixtureEmbedder;
use quickdoc_core::store;

async fn docs(idx: &IndexDir) -> Vec<String> {
    let c = store::get_corpus_by_id(idx.pool(), "folder@1")
        .await
        .unwrap()
        .unwrap();
    store::list_documents(idx.pool(), c.id)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.url)
        .collect()
}

async fn chunks_of(idx: &IndexDir, url: &str) -> Vec<store::Chunk> {
    let c = store::get_corpus_by_id(idx.pool(), "folder@1")
        .await
        .unwrap()
        .unwrap();
    let d = store::list_documents(idx.pool(), c.id)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.url == url)
        .unwrap_or_else(|| panic!("{url} is not in the index"));
    store::list_chunks(idx.pool(), d.id).await.unwrap()
}

fn seed(root: &std::path::Path) {
    write(
        root,
        "notes.md",
        "# Setup\n\nInstall podman first.\n\n## SELinux\n\nUse the zeta relabel flag.\n",
    );
    write(
        root,
        "src/main.rs",
        "fn main() {\n    println!(\"hello quokka\");\n}\n",
    );
    write(root, "todo.txt", "buy milk\n\ncall the gamma office\n");
}

#[tokio::test]
async fn every_skip_is_counted_by_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    seed(root);
    write(root, ".hidden.md", "secret");
    write(root, ".git/config", "[core]");
    write(root, "photo.png", [0x89, b'P', b'N', b'G']);
    write(root, "Makefile", "all:\n\techo build\n");
    write(root, "sub/deep.py", "print('deep')\n");
    write(root, "latin1.txt", [b'c', b'a', b'f', 0xe9, b'\n']);
    write(root, "blank.md", "\n   \n");
    std::os::unix::fs::symlink(root.join("notes.md"), root.join("link.md")).unwrap();
    std::os::unix::fs::symlink(root.join("sub"), root.join("dirlink")).unwrap();

    let idx = IndexDir::open(root).await.unwrap();
    let out = sync(&idx, Counting::new()).await;
    let r = out.report();

    let by = |k: SkipReason| r.skipped_by_reason.get(&k).copied().unwrap_or(0);
    // .hidden.md, .git, and the agent's own index directory.
    assert_eq!(by(SkipReason::Hidden), 3, "{:?}", r.skipped);
    assert_eq!(by(SkipReason::Symlink), 2, "{:?}", r.skipped);
    assert_eq!(by(SkipReason::Unsupported), 1, "{:?}", r.skipped);
    assert_eq!(by(SkipReason::NotUtf8), 1, "{:?}", r.skipped);
    assert_eq!(by(SkipReason::Empty), 1, "{:?}", r.skipped);
    assert!(r
        .skipped
        .iter()
        .any(|s| s.path == "dirlink" && s.reason == SkipReason::Symlink));
    assert_eq!(
        r.found, 7,
        "every file of an indexable type, before content checks"
    );
    assert_eq!(r.new_files, 5);
    assert_eq!(
        docs(&idx).await,
        [
            "Makefile",
            "notes.md",
            "src/main.rs",
            "sub/deep.py",
            "todo.txt"
        ]
    );

    // The plan carries the walk's skips; content-level ones arrive as events.
    let planned = out
        .events
        .iter()
        .find_map(|e| match e {
            SyncEvent::Planned {
                skipped_by_reason,
                new,
                ..
            } => Some((skipped_by_reason.clone(), *new)),
            _ => None,
        })
        .unwrap();
    assert_eq!(planned.1, 7);
    assert!(!planned.0.contains_key(&SkipReason::NotUtf8));
    assert!(out.events.iter().any(|e| matches!(
        e,
        SyncEvent::Skipped { file, reason: SkipReason::NotUtf8, .. } if file == "latin1.txt"
    )));
    assert!(matches!(out.events.last(), Some(SyncEvent::Done { .. })));
}

#[tokio::test]
async fn an_unreadable_file_is_a_skip_not_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    seed(root);
    write(root, "locked.md", "# locked");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        root.join("locked.md"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    if std::fs::read(root.join("locked.md")).is_ok() {
        eprintln!("skipped: running as root, mode 000 does not stop a read");
        return;
    }
    let idx = IndexDir::open(root).await.unwrap();
    let r = sync(&idx, Counting::new()).await;
    assert_eq!(r.report().skipped_by_reason[&SkipReason::Unreadable], 1);
    assert_eq!(r.report().new_files, 3);
}

#[tokio::test]
async fn a_second_sync_of_an_unchanged_folder_embeds_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();

    let e = Counting::new();
    let first = sync(&idx, e.clone()).await;
    let r = first.report();
    assert_eq!(r.new_files, 3);
    assert!(e.texts() > 0);
    assert_eq!(r.embedded_chunks, e.texts());
    assert_eq!(r.chunks, r.embedded_chunks);
    assert_eq!(r.embed_batch_size, EMBED_BATCH_SIZE);
    assert!(first.events.iter().any(|ev| matches!(
        ev,
        SyncEvent::Embedding { batch: 1, of: 1, batch_size, .. } if *batch_size == EMBED_BATCH_SIZE
    )));

    let e2 = Counting::new();
    let second = sync(&idx, e2.clone()).await;
    let r2 = second.report();
    assert_eq!(e2.calls(), 0, "the size+mtime fast path reads nothing");
    assert_eq!(r2.unchanged, 3);
    assert_eq!(
        (
            r2.new_files,
            r2.changed_files,
            r2.touched_unchanged,
            r2.removed_files
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(r2.chunks, r.chunks);
}

#[tokio::test]
async fn a_touched_but_identical_file_is_rehashed_and_not_reembedded() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    sync(&idx, Counting::new()).await.report();

    // Touched, not edited: a new mtime (still well in the past, so it is
    // trusted), the same bytes.
    set_mtime(
        &tmp.path().join("notes.md"),
        SystemTime::now() - Duration::from_secs(1800),
    );

    let e = Counting::new();
    let out = sync(&idx, e.clone()).await;
    let r = out.report();
    assert_eq!(e.calls(), 0);
    assert_eq!(r.touched_unchanged, 1);
    assert_eq!(r.unchanged, 2);
    assert!(out
        .events
        .iter()
        .any(|ev| matches!(ev, SyncEvent::Unchanged { file } if file == "notes.md")));

    // The new mtime was recorded: the next sync takes the fast path again.
    let r3 = sync(&idx, Counting::new()).await;
    assert_eq!(r3.report().unchanged, 3);
}

#[tokio::test]
async fn a_changed_file_is_rechunked_and_its_old_text_leaves_fts() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    sync(&idx, Counting::new()).await.report();
    assert_eq!(fts_count(&idx, "gamma").await, 1);

    write(
        tmp.path(),
        "todo.txt",
        "buy oat milk\n\ncall the delta office\n",
    );
    let e = Counting::new();
    let r = sync(&idx, e.clone()).await;
    assert_eq!(r.report().changed_files, 1);
    assert_eq!(r.report().unchanged, 2);
    let chunks = chunks_of(&idx, "todo.txt").await;
    assert!(chunks.iter().any(|c| c.payload.contains("delta")));
    assert!(chunks.iter().all(|c| !c.payload.contains("gamma")));
    assert_eq!(fts_count(&idx, "gamma").await, 0);
    assert_eq!(fts_count(&idx, "delta").await, 1);
    assert_eq!(
        e.texts(),
        chunks.len(),
        "only the changed file was embedded"
    );
}

#[tokio::test]
async fn a_deleted_file_leaves_the_index_and_fts() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    sync(&idx, Counting::new()).await.report();
    assert_eq!(fts_count(&idx, "quokka").await, 1);

    std::fs::remove_file(tmp.path().join("src/main.rs")).unwrap();
    let out = sync(&idx, Counting::new()).await;
    assert_eq!(out.report().removed_files, 1);
    assert!(out
        .events
        .iter()
        .any(|e| matches!(e, SyncEvent::Removed { file } if file == "src/main.rs")));
    assert_eq!(docs(&idx).await, ["notes.md", "todo.txt"]);
    assert_eq!(fts_count(&idx, "quokka").await, 0);
    assert_eq!(out.report().documents, 2);
}

#[tokio::test]
async fn switching_the_embedding_model_reembeds_everything() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    let first = sync(&idx, Counting::new()).await;
    let total = first.report().chunks;

    let other = Counting::with(FixtureEmbedder::new(DIMS).with_model("other-model"), None);
    let out = sync(&idx, other.clone()).await;
    let r = out.report();
    let reason = r.index_reset.as_deref().unwrap();
    assert!(
        reason.contains("fixture-bow") && reason.contains("other-model"),
        "{reason}"
    );
    assert!(out
        .events
        .iter()
        .any(|e| matches!(e, SyncEvent::IndexReset { .. })));
    assert_eq!(r.new_files, 3);
    assert_eq!(other.texts(), total);

    // A width change is a model change too.
    let narrow = Counting::with(FixtureEmbedder::new(32).with_model("other-model"), None);
    let r = sync(&idx, narrow).await;
    assert!(r.report().index_reset.as_deref().unwrap().contains("32d"));
    assert_eq!(r.report().embed_dims, 32);
}

#[tokio::test]
async fn changing_the_chunk_size_rechunks_everything() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    sync_with(&idx, Counting::new(), 400, None).await.report();
    let out = sync_with(&idx, Counting::new(), 60, None).await;
    let reason = out.report().index_reset.clone().unwrap();
    assert!(reason.contains("400") && reason.contains("60"), "{reason}");
    assert_eq!(out.report().new_files, 3);
}

#[tokio::test]
async fn the_index_directory_is_the_only_thing_written() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    seed(root);
    write(root, ".git/HEAD", "ref: refs/heads/main\n");
    write(root, "photo.png", [1, 2, 3]);
    write(root, "latin1.txt", [0xe9]);
    std::os::unix::fs::symlink(root.join("notes.md"), root.join("link.md")).unwrap();
    mkfifo(&root.join("pipe.md"));
    if common_pdf_ok().await {
        write(root, "paper.pdf", minimal_pdf(&["alpha page"]));
    }
    write(root, "fake.pdf", b"%PDF-1.4 extracted by the fake");
    // Big enough for several batches at 60 tokens a chunk.
    write(root, "long.md", long_markdown(120));
    write(
        root,
        "poison.md",
        "# Poison\n\nPOISONPILL text the model refuses.\n",
    );
    let fake = fake_pdftotext("1.0-fake", &["fake page one\n", "fake page two\n"], None);
    let opts = SyncOptions {
        pdftotext: fake.path.clone(),
        ..SyncOptions::new(60, None)
    };
    let before = fingerprint(root, INDEX_DIR_NAME);

    let idx = IndexDir::open(root).await.unwrap();
    // A hold part-way through long.md: some of its batches are stored.
    let held = Counting::with(FixtureEmbedder::new(DIMS), Some(3));
    assert!(sync_opts(&idx, held, &opts).await.result.is_err());
    assert_eq!(
        fingerprint(root, INDEX_DIR_NAME),
        before,
        "a sync stopped by a hold"
    );
    // Resumed; poison.md fails twice while the probe works.
    let out = sync_opts(&idx, Counting::failing_on("POISONPILL"), &opts).await;
    assert!(out.report().errors.iter().any(|e| e.file == "poison.md"));
    assert!(
        out.report().reused_chunks > 0,
        "the resume kept stored chunks"
    );
    assert_eq!(fingerprint(root, INDEX_DIR_NAME), before, "a resumed sync");

    // The owner edits and deletes; each sync leaves the folder exactly as the
    // owner left it.
    write(root, "todo.txt", "changed\n");
    let after_edit = fingerprint(root, INDEX_DIR_NAME);
    assert_eq!(
        sync_opts(&idx, Counting::new(), &opts)
            .await
            .report()
            .changed_files,
        1
    );
    assert_eq!(
        fingerprint(root, INDEX_DIR_NAME),
        after_edit,
        "a changed-file sync"
    );

    std::fs::remove_file(root.join("notes.md")).unwrap();
    let after_delete = fingerprint(root, INDEX_DIR_NAME);
    assert_eq!(
        sync_opts(&idx, Counting::new(), &opts)
            .await
            .report()
            .removed_files,
        1
    );
    assert_eq!(
        fingerprint(root, INDEX_DIR_NAME),
        after_delete,
        "a removal sync"
    );

    // A PDF that times out, a file over the memory limit, a file just saved.
    write(root, "slow.pdf", b"%PDF-1.4 slow");
    write(
        root,
        "huge.md",
        "# huge\n\n".to_string() + &"word ".repeat(2000),
    );
    write_now(root, "fresh.md", "# fresh\n\njust saved\n");
    let mem = tempfile::tempdir().unwrap();
    std::fs::write(mem.path().join("memory.max"), "32768\n").unwrap();
    let sleeper = fake_pdftotext("1.0-fake", &[], Some(30));
    let after_more = fingerprint(root, INDEX_DIR_NAME);
    let out = sync_opts(
        &idx,
        Counting::new(),
        &SyncOptions {
            pdftotext: sleeper.path.clone(),
            pdf_timeout: Duration::from_millis(300),
            memory_max_path: mem.path().join("memory.max"),
            ..SyncOptions::new(60, None)
        },
    )
    .await;
    let r = out.report();
    let by = |k: SkipReason| r.skipped_by_reason.get(&k).copied().unwrap_or(0);
    assert!(by(SkipReason::PdfTimeout) >= 1, "{:?}", r.skipped);
    assert_eq!(by(SkipReason::TooLargeForMemory), 1, "{:?}", r.skipped);
    assert!(r.racy_files >= 1);
    assert_eq!(
        fingerprint(root, INDEX_DIR_NAME),
        after_more,
        "timeouts, memory skips and a racy file"
    );

    assert!(root.join(INDEX_DIR_NAME).join("index.sqlite").is_file());
    // Nothing but the index file, SQLite's own sidecars, and the two tags
    // written when the directory was made.
    for e in std::fs::read_dir(root.join(INDEX_DIR_NAME)).unwrap() {
        let name = e.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            name.starts_with("index.sqlite") || name == ".gitignore" || name == "CACHEDIR.TAG",
            "unexpected {name}"
        );
    }
}

fn mkfifo(p: &std::path::Path) {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
}

/// `n` sections of a few sentences each: many chunks at a small chunk size.
fn long_markdown(n: usize) -> String {
    (0..n)
        .map(|i| {
            format!(
                "## Section {i}\n\nThe quokka number {i} keeps notes about item {i} and \
                 nothing else, so every section is different.\n\n"
            )
        })
        .collect()
}

async fn common_pdf_ok() -> bool {
    folder_chat::pdf::available().await
}

#[tokio::test]
async fn markdown_chunks_carry_their_heading_path() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    sync(&idx, Counting::new()).await.report();
    let chunks = chunks_of(&idx, "notes.md").await;
    let paths: Vec<&str> = chunks.iter().map(|c| c.heading_path.as_str()).collect();
    assert_eq!(paths, ["# Setup", "# Setup > ## SELinux"]);
    for c in &chunks {
        assert_eq!(c.derived_title, "notes.md", "the path is the chunk's label");
        let text = std::fs::read_to_string(tmp.path().join("notes.md")).unwrap();
        assert_eq!(
            &text[c.span_start as usize..c.span_end as usize],
            c.payload,
            "spans are byte offsets of the verbatim payload"
        );
    }
}

#[tokio::test]
async fn pdfs_are_extracted_per_page() {
    if !folder_chat::pdf::available().await {
        eprintln!("skipped: pdftotext is not on PATH (install poppler-utils to run this test)");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "paper.pdf",
        minimal_pdf(&["Kestrel migration notes", "Osprey nesting season"]),
    );
    write(tmp.path(), "scan.pdf", minimal_pdf(&[""]));
    write(tmp.path(), "mixed.pdf", minimal_pdf(&["", "Heron survey"]));
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    let out = sync(&idx, Counting::new()).await;
    let r = out.report();
    assert_eq!(r.new_files, 2, "{:?}", r);
    assert_eq!(r.skipped_by_reason[&SkipReason::PdfNoText], 1);
    // The real pdftotext ends every page with a form feed.
    assert_eq!(
        r.pdf_pages_without_text,
        vec![TextlessPages {
            path: "mixed.pdf".into(),
            pages: 2,
            textless: vec![1],
        }]
    );
    assert_eq!(chunks_of(&idx, "mixed.pdf").await[0].heading_path, "page 2");
    let chunks = chunks_of(&idx, "paper.pdf").await;
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].heading_path, "page 1");
    assert!(chunks[0].payload.contains("Kestrel"));
    assert_eq!(chunks[1].heading_path, "page 2");
    assert!(chunks[1].payload.contains("Osprey"));
}

#[tokio::test]
async fn a_gpu_hold_stops_the_sync_and_leaves_the_index_consistent() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    // One embed call succeeds (notes.md, first in order), the next is held.
    let held = Counting::with(FixtureEmbedder::new(DIMS), Some(1));
    let out = sync(&idx, held).await;
    match &out.result {
        Err(SyncError::Aborted {
            kind,
            reason,
            partial,
        }) => {
            assert_eq!(*kind, AbortKind::GpuHold);
            assert!(
                reason.starts_with(folder_chat::gateway::GPU_HOLD_MESSAGE),
                "{reason}"
            );
            assert_eq!(partial.new_files, 1);
        }
        other => panic!(
            "expected a gpu_hold abort, got {:?}",
            other.as_ref().map(|_| ())
        ),
    }
    assert!(matches!(
        out.events.last(),
        Some(SyncEvent::Aborted {
            kind: AbortKind::GpuHold,
            ..
        })
    ));
    // Only the finished file is in the index; the held one left no row.
    assert_eq!(docs(&idx).await, ["notes.md"]);

    // The hold released: the next sync picks up exactly what is missing.
    let e = Counting::new();
    let r = sync(&idx, e).await;
    assert_eq!(r.report().new_files, 2);
    assert_eq!(r.report().unchanged, 1);
}

#[tokio::test]
async fn a_changed_file_interrupted_by_a_hold_is_redone_next_time() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    sync(&idx, Counting::new()).await.report();
    write(tmp.path(), "notes.md", "# Setup\n\nNow with epsilon.\n");
    let held = Counting::with(FixtureEmbedder::new(DIMS), Some(0));
    assert!(sync(&idx, held).await.result.is_err());
    // The old chunks still answer until the file is redone.
    assert_eq!(fts_count(&idx, "zeta").await, 1);

    let r = sync(&idx, Counting::new()).await;
    assert_eq!(r.report().changed_files, 1);
    assert_eq!(fts_count(&idx, "zeta").await, 0);
    assert_eq!(fts_count(&idx, "epsilon").await, 1);
}

#[tokio::test]
async fn a_chunk_larger_than_the_embedding_context_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    let out = sync_with(&idx, Counting::new(), 400, Some(256)).await;
    match out.result {
        Err(SyncError::Aborted { kind, reason, .. }) => {
            assert_eq!(kind, AbortKind::ChunkTooLarge);
            assert!(reason.contains("400") && reason.contains("256"), "{reason}");
        }
        _ => panic!("expected ChunkTooLarge"),
    }
    // An unreported context is a note, not a silent pass.
    let ok = sync_with(&idx, Counting::new(), 400, None).await;
    assert!(ok
        .report()
        .notes
        .iter()
        .any(|n| n.contains("no context_length")));
}

#[tokio::test]
async fn only_one_sync_at_a_time() {
    let tmp = tempfile::tempdir().unwrap();
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    let _held = idx.try_begin_sync().unwrap();
    let out = sync(&idx, Counting::new()).await;
    assert!(matches!(out.result, Err(SyncError::AlreadyRunning)));
}

#[tokio::test]
async fn a_huge_single_line_file_is_many_chunks_not_a_skip() {
    let tmp = tempfile::tempdir().unwrap();
    // ~200k characters, no whitespace, no newline — a minified bundle.
    let line: String = (0..33_000).map(|i| format!("{i:06}")).collect();
    write(tmp.path(), "bundle.js", &line);
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    let out = sync_with(&idx, Counting::new(), 100, None).await;
    let r = out.report();
    assert_eq!(r.new_files, 1);
    assert!(r.embedded_chunks >= 490, "{}", r.embedded_chunks);
    assert_eq!(r.chunks, r.embedded_chunks, "every piece is stored");
    let batches = out
        .events
        .iter()
        .filter(|e| matches!(e, SyncEvent::Embedding { .. }))
        .count();
    assert_eq!(batches, r.embedded_chunks.div_ceil(EMBED_BATCH_SIZE));
}

#[tokio::test]
async fn gitignore_and_ignore_files_prune_and_are_counted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    seed(root);
    // Not a git repository: the files are honoured all the same.
    write(root, ".gitignore", "node_modules/\n*.log\n");
    write(root, "node_modules/pkg/index.js", "module.exports = 1;\n");
    write(root, "node_modules/pkg/README.md", "# pkg\n");
    write(root, "app.log", "a log line\n");
    write(root, "sub/trace.log", "a trace\n");
    write(root, "sub/keep.md", "# keep\n");
    // A deeper file adds its own rules, relative to where it is.
    write(root, "docs/.ignore", "draft-*.md\n");
    write(root, "docs/draft-1.md", "# draft\n");
    write(root, "docs/final.md", "# final\n");
    // … and can re-include what a shallower one excluded.
    write(root, "logs/.gitignore", "!important.log\n");
    write(root, "logs/important.log", "important\n");
    write(root, "logs/noise.log", "noise\n");
    // A symlinked ignore file is never read (and is a skipped symlink).
    write(root, "patterns.txt", "*.md\n");
    write(root, "sym/a.md", "# a\n");
    std::os::unix::fs::symlink(root.join("patterns.txt"), root.join("sym/.gitignore")).unwrap();

    let idx = IndexDir::open(root).await.unwrap();
    let out = sync(&idx, Counting::new()).await;
    let r = out.report();

    assert_eq!(r.ignored_dirs, ["node_modules"], "pruned, one entry");
    assert_eq!(
        r.ignored_files, 4,
        "app.log, sub/trace.log, docs/draft-1.md, logs/noise.log"
    );
    let by = |k: SkipReason| r.skipped_by_reason.get(&k).copied().unwrap_or(0);
    assert_eq!(by(SkipReason::Ignored), 5);
    assert_eq!(
        r.ignore_files,
        [".gitignore", "docs/.ignore", "logs/.gitignore"]
    );
    // Nothing under a pruned directory was walked, and ignored files are
    // counted, not listed.
    assert!(
        r.skipped.iter().all(|s| !s.path.starts_with("node_modules")
            && !s.path.ends_with(".log")
            && s.reason != SkipReason::Ignored),
        "{:?}",
        r.skipped
    );
    // The ignore files are hidden entries like any other; the symlinked one
    // is hidden too, since hidden is checked first.
    assert_eq!(by(SkipReason::Hidden), 5, "{:?}", r.skipped);

    let indexed = docs(&idx).await;
    for want in [
        "docs/final.md",
        "logs/important.log",
        "patterns.txt",
        "sub/keep.md",
        "sym/a.md",
    ] {
        assert!(indexed.iter().any(|d| d == want), "{want} in {indexed:?}");
    }
    for gone in [
        "app.log",
        "docs/draft-1.md",
        "logs/noise.log",
        "sub/trace.log",
    ] {
        assert!(!indexed.iter().any(|d| d == gone), "{gone} in {indexed:?}");
    }
    assert!(!indexed.iter().any(|d| d.starts_with("node_modules")));

    // The plan already carries the count and the pruned directories.
    let planned = out
        .events
        .iter()
        .find_map(|e| match e {
            SyncEvent::Planned {
                skipped_by_reason,
                ignored_dirs,
                ..
            } => Some((skipped_by_reason.clone(), ignored_dirs.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(planned.0.get(&SkipReason::Ignored), Some(&5));
    assert_eq!(planned.1, ["node_modules"]);

    // An ignore rule that starts matching removes the file on the next sync.
    write(root, ".gitignore", "node_modules/\n*.log\ntodo.txt\n");
    let again = sync(&idx, Counting::new()).await;
    assert_eq!(again.report().removed_files, 1);
    assert!(!docs(&idx).await.iter().any(|d| d == "todo.txt"));
}

// ---------------------------------------------------------------------------
// Reads that race the scan (a path swapped after the folder was listed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn paths_swapped_after_the_scan_are_refused_per_file_never_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    // What a followed symlink could reach: in the container, /lmgw with the
    // agent token.
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(
        outside.path().join("notes.md"),
        "# token\n\nTOKENSECRET one\n",
    )
    .unwrap();
    std::fs::write(
        outside.path().join("secret.md"),
        "# token\n\nTOKENSECRET two\n",
    )
    .unwrap();
    write(&root, "a.md", "# A\n\nfirst in order\n");
    write(&root, "b.md", "# B\n\nbecomes a fifo\n");
    write(&root, "c.md", "# C\n\nbecomes a link\n");
    write(
        &root,
        "docs/notes.md",
        "# Docs\n\nbecomes a link to outside\n",
    );
    let idx = IndexDir::open(&root).await.unwrap();

    // a.md's batch runs after the scan and before the others are read.
    let fired = std::sync::atomic::AtomicBool::new(false);
    let (r2, out2) = (root.clone(), outside.path().to_path_buf());
    let e = Counting::hooked(move |_| {
        if fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        std::fs::remove_dir_all(r2.join("docs")).unwrap();
        std::os::unix::fs::symlink(&out2, r2.join("docs")).unwrap();
        std::fs::remove_file(r2.join("b.md")).unwrap();
        mkfifo(&r2.join("b.md"));
        std::fs::remove_file(r2.join("c.md")).unwrap();
        std::os::unix::fs::symlink(out2.join("secret.md"), r2.join("c.md")).unwrap();
    });
    // A FIFO opened for reading would block until a writer came: bounded.
    let out = tokio::time::timeout(Duration::from_secs(20), sync(&idx, e))
        .await
        .expect("a file swapped for a FIFO hung the sync");
    let r = out.report();
    assert_eq!(docs(&idx).await, ["a.md"]);
    assert_eq!(
        fts_count(&idx, "tokensecret").await,
        0,
        "nothing outside was read"
    );
    let skip = |p: &str| r.skipped.iter().find(|s| s.path == p).cloned().unwrap();
    let d = skip("docs/notes.md");
    assert_eq!(d.reason, SkipReason::Symlink);
    assert!(d.detail.as_deref().unwrap().contains("'docs'"), "{d:?}");
    let b = skip("b.md");
    assert_eq!(b.reason, SkipReason::Unsupported);
    assert!(b.detail.unwrap().contains("not a regular file"));
    assert_eq!(skip("c.md").reason, SkipReason::Symlink);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
}

// ---------------------------------------------------------------------------
// Write containment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_index_dir_swapped_for_a_symlink_stops_the_sync_before_any_write() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    seed(root);
    let idx = IndexDir::open(root).await.unwrap();
    sync(&idx, Counting::new()).await.report();

    // The directory the pool has open moves away; a link takes its name.
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::rename(root.join(INDEX_DIR_NAME), root.join("moved")).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), root.join(INDEX_DIR_NAME)).unwrap();
    write(root, "new.md", "# New\n\nwould be indexed\n");
    let moved = fingerprint(&root.join("moved"), "");

    let e = Counting::new();
    let out = sync(&idx, e.clone()).await;
    match &out.result {
        Err(SyncError::Aborted { kind, reason, .. }) => {
            assert_eq!(*kind, AbortKind::IndexContainment);
            assert!(
                reason.contains(INDEX_DIR_NAME) && reason.contains("nothing was written"),
                "{reason}"
            );
        }
        other => panic!(
            "expected a containment stop, got {:?}",
            other.as_ref().map(|_| ())
        ),
    }
    assert!(matches!(
        out.events.last(),
        Some(SyncEvent::Aborted {
            kind: AbortKind::IndexContainment,
            ..
        })
    ));
    assert_eq!(e.calls(), 0);
    assert_eq!(
        std::fs::read_dir(elsewhere.path()).unwrap().count(),
        0,
        "nothing was written through the link"
    );
    assert_eq!(
        fingerprint(&root.join("moved"), ""),
        moved,
        "nor into the moved index"
    );
}

#[tokio::test]
async fn a_second_hard_link_or_a_symlinked_sidecar_stops_the_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    seed(root);
    let idx = IndexDir::open(root).await.unwrap();
    let dir = root.join(INDEX_DIR_NAME);

    // A second name for the index file, elsewhere in the folder.
    std::fs::hard_link(dir.join("index.sqlite"), root.join("copy.sqlite")).unwrap();
    let out = sync(&idx, Counting::new()).await;
    match &out.result {
        Err(SyncError::Aborted { kind, reason, .. }) => {
            assert_eq!(*kind, AbortKind::IndexContainment);
            assert!(reason.contains("2 hard links"), "{reason}");
        }
        other => panic!("{:?}", other.as_ref().map(|_| ())),
    }
    std::fs::remove_file(root.join("copy.sqlite")).unwrap();
    assert_eq!(sync(&idx, Counting::new()).await.report().new_files, 3);

    // A rollback journal SQLite would write through.
    let elsewhere = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(elsewhere.path().join("j"), dir.join("index.sqlite-journal"))
        .unwrap();
    let out = sync(&idx, Counting::new()).await;
    match &out.result {
        Err(SyncError::Aborted { kind, reason, .. }) => {
            assert_eq!(*kind, AbortKind::IndexContainment);
            assert!(reason.contains("index.sqlite-journal"), "{reason}");
        }
        other => panic!("{:?}", other.as_ref().map(|_| ())),
    }
    assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
}

// ---------------------------------------------------------------------------
// One bad file does not block the sync
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_file_the_model_cannot_embed_is_an_error_and_the_sync_goes_on() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    write(tmp.path(), "bad.md", "# Bad\n\nPOISONPILL\n");
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    let e = Counting::failing_on("POISONPILL");
    let out = sync(&idx, e.clone()).await;
    let r = out.report();
    assert_eq!(r.new_files, 3, "every other file is indexed");
    assert_eq!(r.errors.len(), 1);
    let err = &r.errors[0];
    assert_eq!(err.file, "bad.md");
    assert!(
        err.message.contains("EMBED_BATCH_ATTEMPTS") && err.message.contains("probe"),
        "{}",
        err.message
    );
    assert_eq!(r.embed_batch_attempts, 2);
    // bad.md: two attempts; then one probe (besides the sync's own).
    assert_eq!(e.probes(), 2);
    assert!(!docs(&idx).await.iter().any(|d| d == "bad.md"));
    // Not current: the next sync tries it again, and only it.
    let again = sync(&idx, Counting::new()).await;
    assert_eq!(again.report().new_files, 1);
    assert_eq!(again.report().unchanged, 3);
}

#[tokio::test]
async fn a_pdf_that_outlasts_the_timeout_is_skipped_naming_the_constant() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    write(tmp.path(), "slow.pdf", b"%PDF-1.4 whatever");
    let sleeper = fake_pdftotext("9.9-fake", &[], Some(60));
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    let t0 = std::time::Instant::now();
    let out = sync_opts(
        &idx,
        Counting::new(),
        &SyncOptions {
            pdftotext: sleeper.path.clone(),
            pdf_timeout: Duration::from_millis(300),
            ..SyncOptions::new(400, None)
        },
    )
    .await;
    assert!(
        t0.elapsed() < Duration::from_secs(20),
        "the sleeper was killed"
    );
    let r = out.report();
    let s = r.skipped.iter().find(|s| s.path == "slow.pdf").unwrap();
    assert_eq!(s.reason, SkipReason::PdfTimeout);
    let d = s.detail.as_deref().unwrap();
    assert!(
        d.contains("PDF_EXTRACT_TIMEOUT") && d.contains("300 s"),
        "{d}"
    );
    assert_eq!(r.pdf_extract_timeout_ms, 300);
    assert_eq!(r.new_files, 3);
}

// ---------------------------------------------------------------------------
// The embedding model's identity is more than its alias
// ---------------------------------------------------------------------------

/// The fixture's vectors, negated: the same alias and width, another model.
struct Negated(FixtureEmbedder);

#[async_trait::async_trait]
impl quickdoc_core::embed::Embedder for Negated {
    fn identity(&self) -> quickdoc_core::embed::EmbedIdentity {
        self.0.identity()
    }

    async fn embed(&self, texts: &[String]) -> quickdoc_core::Result<Vec<Vec<f32>>> {
        let mut v = self.0.embed(texts).await?;
        v.iter_mut().flatten().for_each(|x| *x = -*x);
        Ok(v)
    }
}

#[tokio::test]
async fn the_same_alias_and_width_with_other_vectors_rebuilds_the_index() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    let first = sync(&idx, Counting::new()).await;
    assert_eq!(first.report().embed_probe_cosine, None, "recorded first");
    let same = sync(&idx, Counting::new()).await;
    let c = same.report().embed_probe_cosine.unwrap();
    assert!(c > 0.999, "{c}");
    assert_eq!(same.report().index_reset, None);

    let out = sync(
        &idx,
        std::sync::Arc::new(Negated(FixtureEmbedder::new(DIMS))),
    )
    .await;
    let r = out.report();
    let reason = r.index_reset.as_deref().unwrap();
    assert!(
        reason.contains("PROBE_SAME_MODEL_MIN_COSINE") && reason.contains("-1.000"),
        "{reason}"
    );
    assert!(out
        .events
        .iter()
        .any(|e| matches!(e, SyncEvent::IndexReset { .. })));
    assert_eq!(r.new_files, 3, "every file re-embedded");
    // The new model is now the baseline.
    let after = sync(
        &idx,
        std::sync::Arc::new(Negated(FixtureEmbedder::new(DIMS))),
    )
    .await;
    assert_eq!(after.report().index_reset, None);
    assert_eq!(after.report().unchanged, 3);
}

// ---------------------------------------------------------------------------
// Large files: stored batch by batch, resumed, and bounded by memory
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_hold_mid_file_keeps_what_was_stored_and_resumes_without_duplicates() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "long.md", long_markdown(150));
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    // Two batches go through, the third is held.
    let held = Counting::with(FixtureEmbedder::new(DIMS), Some(2));
    let out = sync_with(&idx, held.clone(), 60, None).await;
    assert!(matches!(
        out.result,
        Err(SyncError::Aborted {
            kind: AbortKind::GpuHold,
            ..
        })
    ));
    let stored = chunks_of(&idx, "long.md").await.len();
    assert_eq!(stored, 2 * EMBED_BATCH_SIZE, "the stored batches stay");

    let e = Counting::new();
    let out = sync_with(&idx, e.clone(), 60, None).await;
    let r = out.report();
    assert_eq!(r.reused_chunks, stored, "nothing stored is embedded again");
    let total = chunks_of(&idx, "long.md").await;
    assert_eq!(e.texts() + stored, total.len());
    assert_eq!(r.chunks, total.len());
    // One row per distinct text; every section once in BM25.
    let distinct: HashSet<&str> = total.iter().map(|c| c.payload.as_str()).collect();
    assert_eq!(distinct.len(), total.len());
    assert_eq!(total.len(), 150);
    assert_eq!(fts_count(&idx, "quokka").await, 150);
    // Current now: the next sync reads nothing.
    let quiet = Counting::new();
    assert_eq!(
        sync_with(&idx, quiet.clone(), 60, None)
            .await
            .report()
            .unchanged,
        1
    );
    assert_eq!(quiet.calls(), 0);
}

#[tokio::test]
async fn a_file_over_the_memory_share_is_skipped_naming_the_limit() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    write(
        tmp.path(),
        "big.md",
        "# Big\n\n".to_string() + &"x".repeat(3000),
    );
    let mem = tempfile::tempdir().unwrap();
    let max = mem.path().join("memory.max");
    std::fs::write(&max, "8192\n").unwrap();
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    let opts = SyncOptions {
        memory_max_path: max.clone(),
        ..SyncOptions::new(400, None)
    };
    let out = sync_opts(&idx, Counting::new(), &opts).await;
    let r = out.report();
    assert_eq!(r.memory_limit_bytes, Some(8192));
    assert_eq!(r.large_file_limit_bytes, Some(2048));
    let s = r.skipped.iter().find(|s| s.path == "big.md").unwrap();
    assert_eq!(s.reason, SkipReason::TooLargeForMemory);
    let d = s.detail.as_deref().unwrap();
    for want in [
        "LARGE_FILE_MEMORY_FRACTION (0.25)",
        "memory.max",
        "run.limits.memory_mb",
        ".ignore",
    ] {
        assert!(d.contains(want), "{want} in {d}");
    }
    assert_eq!(r.new_files, 3);

    // `max` is no limit: the same file is indexed.
    std::fs::write(&max, "max\n").unwrap();
    let r = sync_opts(&idx, Counting::new(), &opts).await;
    assert_eq!(r.report().new_files, 1);
    assert_eq!(r.report().memory_limit_bytes, None);
    assert!(r
        .report()
        .notes
        .iter()
        .any(|n| n.contains("no container memory limit")));
}

// ---------------------------------------------------------------------------
// Racy mtimes
// ---------------------------------------------------------------------------

async fn recorded_mtime(idx: &IndexDir, url: &str) -> i64 {
    use sqlx::Row;
    sqlx::query(
        "SELECT f.mtime_ns FROM folder_chat_file f JOIN document d ON d.id = f.document_id
         WHERE d.url = ?1",
    )
    .bind(url)
    .fetch_one(idx.pool())
    .await
    .unwrap()
    .get("mtime_ns")
}

#[tokio::test]
async fn a_file_saved_just_now_is_checked_again_next_time() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    write_now(tmp.path(), "fresh.md", "# Fresh\n\njust saved\n");
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    let out = sync(&idx, Counting::new()).await;
    assert_eq!(out.report().racy_files, 1);
    assert!(out
        .report()
        .notes
        .iter()
        .any(|n| n.contains("RACY_MTIME_WINDOW")));
    assert_eq!(recorded_mtime(&idx, "fresh.md").await, 0, "not trusted");
    assert_ne!(recorded_mtime(&idx, "notes.md").await, 0);

    // Re-hashed (not re-embedded) rather than taken on its mtime's word.
    let e = Counting::new();
    let r = sync(&idx, e.clone()).await;
    assert_eq!(r.report().touched_unchanged, 1);
    assert_eq!(e.calls(), 0);

    // Once its mtime is old enough, it is trusted and takes the fast path.
    set_mtime(
        &tmp.path().join("fresh.md"),
        SystemTime::now() - Duration::from_secs(60),
    );
    sync(&idx, Counting::new()).await.report();
    assert_ne!(recorded_mtime(&idx, "fresh.md").await, 0);
    assert_eq!(sync(&idx, Counting::new()).await.report().unchanged, 4);
}

#[tokio::test]
async fn a_file_that_changed_between_the_scan_and_the_read_is_not_trusted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    write(&root, "a.md", "# A\n\nfirst\n");
    write(&root, "b.md", "# B\n\nsecond\n");
    let idx = IndexDir::open(&root).await.unwrap();
    let r2 = root.clone();
    let fired = std::sync::atomic::AtomicBool::new(false);
    let e = Counting::hooked(move |_| {
        if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            // Grown after the scan listed it, then dated back so only the
            // size says so.
            write(&r2, "b.md", "# B\n\nsecond, and then some\n");
        }
    });
    let out = sync(&idx, e).await;
    assert_eq!(out.report().racy_files, 1);
    assert_eq!(recorded_mtime(&idx, "b.md").await, 0);
    assert!(chunks_of(&idx, "b.md")
        .await
        .iter()
        .any(|c| c.payload.contains("and then some")));
}

// ---------------------------------------------------------------------------
// Line numbers stored with the chunks; pdftotext updates
// ---------------------------------------------------------------------------

async fn stored_lines(idx: &IndexDir, url: &str) -> Vec<(store::Chunk, Option<(usize, usize)>)> {
    let chunks = chunks_of(idx, url).await;
    let ids: Vec<String> = chunks.iter().map(|c| c.id.clone()).collect();
    let lines = idx.chunk_lines(&ids).await.unwrap();
    chunks
        .into_iter()
        .map(|c| {
            let l = lines.get(&c.id).copied();
            (c, l)
        })
        .collect()
}

#[tokio::test]
async fn every_chunk_carries_the_lines_it_was_cut_from() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    sync(&idx, Counting::new()).await.report();
    let text = std::fs::read_to_string(tmp.path().join("notes.md")).unwrap();
    let got = stored_lines(&idx, "notes.md").await;
    assert_eq!(got.len(), 2);
    for (c, lines) in &got {
        assert_eq!(
            *lines,
            Some(folder_chat::chunk::line_range(
                &text,
                c.span_start as usize,
                c.span_end as usize
            )),
            "{}",
            c.payload
        );
    }

    // What a citation gets, without reading the file.
    let mut cites: Vec<folder_chat::chat::Citation> = got
        .iter()
        .map(|(c, _)| folder_chat::chat::Citation {
            n: 1,
            path: "notes.md".into(),
            heading_path: c.heading_path.clone(),
            page: None,
            span_start: c.span_start,
            span_end: c.span_end,
            start_line: None,
            end_line: None,
            chunk_id: c.id.clone(),
            score: 0.0,
            text: c.payload.clone(),
        })
        .collect();
    std::fs::remove_file(tmp.path().join("notes.md")).unwrap();
    idx.fill_stored_lines(&mut cites).await.unwrap();
    assert_eq!(cites[1].start_line, Some(5));
    assert_eq!(cites[1].end_line, Some(7));
}

#[tokio::test]
async fn an_index_without_stored_lines_records_them_without_re_embedding() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    sync(&idx, Counting::new()).await.report();
    // An index from before lines were recorded.
    sqlx::query("DELETE FROM folder_chat_chunk_lines")
        .execute(idx.pool())
        .await
        .unwrap();
    let e = Counting::new();
    let out = sync(&idx, e.clone()).await;
    let r = out.report();
    assert_eq!(e.calls(), 0, "every vector was kept");
    assert_eq!(r.lines_recorded_files, 3);
    assert_eq!((r.new_files, r.changed_files), (0, 0));
    assert!(r.notes.iter().any(|n| n.contains("line numbers")));
    assert!(stored_lines(&idx, "todo.txt")
        .await
        .iter()
        .all(|(_, l)| l.is_some()));
    assert_eq!(sync(&idx, Counting::new()).await.report().unchanged, 3);
}

#[tokio::test]
async fn a_new_pdftotext_re_extracts_the_pdfs_and_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    write(tmp.path(), "paper.pdf", b"%PDF-1.4 bytes");
    let v1 = fake_pdftotext("1.0", &["alpha page\n", "beta page\n"], None);
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    let opts = |p: &FakePdftotext| SyncOptions {
        pdftotext: p.path.clone(),
        ..SyncOptions::new(400, None)
    };
    let r = sync_opts(&idx, Counting::new(), &opts(&v1)).await;
    assert_eq!(r.report().new_files, 4);
    assert_eq!(
        r.report().pdftotext_version.as_deref(),
        Some("pdftotext version 1.0")
    );
    let pages = stored_lines(&idx, "paper.pdf").await;
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].1, Some((1, 1)));
    assert_eq!(
        pages[1].1,
        Some((2, 2)),
        "page 2 starts after the form feed"
    );

    // Same bytes, a new poppler whose text differs on page 2: only the PDF
    // is extracted again, only its changed chunk embedded, nothing reset.
    let v2 = fake_pdftotext("2.0", &["alpha page\n", "osprey page\n"], None);
    let e = Counting::new();
    let out = sync_opts(&idx, e.clone(), &opts(&v2)).await;
    let r = out.report();
    assert_eq!(r.index_reset, None);
    assert_eq!(r.unchanged, 3, "the other files are not even read");
    assert_eq!(r.changed_files, 1);
    assert_eq!(e.texts(), 1, "page 1 kept its vector");
    assert!(
        r.notes.iter().any(|n| n.contains(
            "pdftotext changed from 'pdftotext version 1.0' to \
                               'pdftotext version 2.0'"
        )),
        "{:?}",
        r.notes
    );
    assert_eq!(fts_count(&idx, "osprey").await, 1);
    assert_eq!(fts_count(&idx, "beta").await, 0);

    // Recorded: the next sync takes the fast path for the PDF too.
    let quiet = Counting::new();
    let r = sync_opts(&idx, quiet.clone(), &opts(&v2)).await;
    assert_eq!(r.report().unchanged, 4);
    assert_eq!(quiet.calls(), 0);
}

#[tokio::test]
async fn pdf_pages_without_text_are_reported_by_every_sync_and_backfilled_once() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    write(tmp.path(), "scan.pdf", b"%PDF-1.4 bytes");
    // Page 2 is a scan: pdftotext gives it nothing but its form feed.
    let fake = fake_pdftotext("1.0", &["alpha page\n", "", "gamma page\n"], None);
    let opts = SyncOptions {
        pdftotext: fake.path.clone(),
        ..SyncOptions::new(400, None)
    };
    let want = vec![TextlessPages {
        path: "scan.pdf".into(),
        pages: 3,
        textless: vec![2],
    }];
    let says = |r: &folder_chat::sync::SyncReport| {
        r.notes.iter().any(|n| {
            n.contains("1 PDF page(s) in 1 file(s)") && n.contains("scan.pdf (page 2 of 3)")
        })
    };
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    let out = sync_opts(&idx, Counting::new(), &opts).await;
    let r = out.report();
    assert_eq!(r.new_files, 4);
    assert_eq!(r.pdf_pages_without_text, want);
    assert!(says(r), "{:?}", r.notes);
    assert_eq!(chunks_of(&idx, "scan.pdf").await.len(), 2);

    // A sync that does not read the file still reports it.
    let out = sync_opts(&idx, Counting::new(), &opts).await;
    let r = out.report();
    assert_eq!(r.unchanged, 4);
    assert_eq!(r.pdf_pages_without_text, want);
    assert!(says(r), "{:?}", r.notes);

    // An index from before the pages were recorded: extracted once more,
    // nothing embedded, nothing else read.
    sqlx::query("DELETE FROM folder_chat_pdf_pages")
        .execute(idx.pool())
        .await
        .unwrap();
    let e = Counting::new();
    let out = sync_opts(&idx, e.clone(), &opts).await;
    let r = out.report();
    assert_eq!(e.calls(), 0, "every vector was kept");
    assert_eq!(r.pdf_pages_recorded_files, 1);
    assert_eq!((r.new_files, r.changed_files, r.unchanged), (0, 0, 3));
    assert_eq!(r.pdf_pages_without_text, want);
    assert!(
        r.notes.iter().any(|n| n.contains("extracted again once")),
        "{:?}",
        r.notes
    );
    assert_eq!(
        sync_opts(&idx, Counting::new(), &opts)
            .await
            .report()
            .unchanged,
        4
    );

    // Without a pdftotext to record them with, a PDF that lacks its row
    // stays as indexed and nothing is reported against it.
    let rows = |idx: &IndexDir| {
        let pool = idx.pool().clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM folder_chat_pdf_pages")
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    sqlx::query("DELETE FROM folder_chat_pdf_pages")
        .execute(idx.pool())
        .await
        .unwrap();
    let missing = SyncOptions {
        pdftotext: tmp.path().join("no-such-pdftotext"),
        ..SyncOptions::new(400, None)
    };
    let out = sync_opts(&idx, Counting::new(), &missing).await;
    let r = out.report();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!((r.unchanged, r.pdf_pages_recorded_files), (4, 0));
    assert_eq!(chunks_of(&idx, "scan.pdf").await.len(), 2);

    // A pdftotext that times out on the backfill pass keeps the file indexed.
    let slow = fake_pdftotext("1.0", &[], Some(30));
    let out = sync_opts(
        &idx,
        Counting::new(),
        &SyncOptions {
            pdftotext: slow.path.clone(),
            pdf_timeout: Duration::from_millis(300),
            ..SyncOptions::new(400, None)
        },
    )
    .await;
    let r = out.report();
    assert_eq!(r.removed_files, 0);
    assert!(
        r.errors
            .iter()
            .any(|e| e.file == "scan.pdf" && e.message.contains("stays indexed")),
        "{:?}",
        r.errors
    );
    assert_eq!(chunks_of(&idx, "scan.pdf").await.len(), 2);
    assert_eq!(rows(&idx).await, 0);
    let r = sync_opts(&idx, Counting::new(), &opts).await;
    assert_eq!(r.report().pdf_pages_recorded_files, 1);
    assert_eq!(rows(&idx).await, 1);

    // Once the file is gone, so is its row.
    std::fs::remove_file(tmp.path().join("scan.pdf")).unwrap();
    let out = sync_opts(&idx, Counting::new(), &opts).await;
    assert!(out.report().pdf_pages_without_text.is_empty());
    assert_eq!(rows(&idx).await, 0, "the row cascades with its document");
}
