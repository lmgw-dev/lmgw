//! The reader as a child, through the runner: the real executable (cargo
//! builds it beside the test binaries) on files that overstay, ask for too
//! much memory or answer too much, and stand-ins that answer wrongly.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;

use super::*;
use crate::extract::convert::pool_size::Machine;
use crate::extract::convert::tools::DocTools;

/// The real reader, where lmgw looks for it.
async fn real() -> PathBuf {
    let facts = DocTools::on_path_with_own(OsString::new()).current().await;
    path(&facts).expect("cargo builds lmgw-doc-reader beside the test binaries")
}

/// A stand-in reader: a shell script.
fn stand_in(dir: &std::path::Path, script: &str) -> PathBuf {
    let at = dir.join(READER_BIN);
    let mut f = std::fs::File::create(&at).unwrap();
    writeln!(f, "#!/bin/sh\n{script}").unwrap();
    drop(f);
    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
    at
}

async fn html(bin: &std::path::Path, page: Vec<u8>, output: u64) -> Result<Frame, ReadError> {
    let slot = pool::global().acquire().await;
    read_with(
        &slot,
        bin,
        Op::Html,
        Input::Bytes(Bytes::from(page)),
        output,
    )
    .await
}

#[test]
fn it_is_looked_for_beside_lmgw_in_libexec_and_in_a_cargo_target() {
    let dirs: Vec<PathBuf> = std::env::split_paths(&own_dirs()).collect();
    let exe = std::env::current_exe().unwrap();
    let here = exe.parent().unwrap();
    assert_eq!(dirs[0], here);
    assert_eq!(dirs[1], here.parent().unwrap().join("libexec").join("lmgw"));
    // This test binary is in `target/<profile>/deps`.
    assert_eq!(dirs[2], here.parent().unwrap());
    assert!(dirs.iter().all(|d| d.is_absolute()));
}

#[tokio::test]
async fn a_page_is_read_by_the_child() {
    let bin = real().await;
    let f = html(&bin, b"<h1>Example</h1><p>Text.</p>".to_vec(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(f.rest(), None);
    let Answer::Page(p) = f.answer else {
        panic!("{:?}", f.answer);
    };
    assert_eq!(p.markdown, "# Example\n\nText.");
}

/// A page nested a thousand deep two thousand times over costs html5ever
/// time that grows with the square of the depth for each (a page nested
/// past the parse's stop is answered too deep at once, `html/depth.rs`).
/// The child is stopped at its timeout; nothing of it ran in this process.
#[tokio::test]
async fn a_page_that_overstays_is_stopped_and_named() {
    let bin = real().await;
    let block = format!("{}{}", "<div>".repeat(1000), "</div>".repeat(1000));
    let page = block.repeat(2000).into_bytes();
    let started = std::time::Instant::now();
    let e = with_bounds(
        Duration::from_secs(2),
        2 << 30,
        html(&bin, page, MAX_UNCOMPRESSED_BYTES),
    )
    .await
    .unwrap_err();
    assert_eq!(e, ReadError::Timeout);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    let (code, why) = e.failure();
    assert_eq!(code, ErrorCode::Unreadable);
    assert!(why.contains("READER_TIMEOUT (300 s)"), "{why}");
}

/// html5ever rebuilds every open formatting element for each new paragraph:
/// 240 of them and a hundred thousand paragraphs are some gigabytes of
/// tree from 400 KiB of page. The child runs out of its limit and fails
/// alone.
#[tokio::test]
async fn a_page_that_asks_for_too_much_memory_fails_alone() {
    let bin = real().await;
    let mut page = String::from("<p>");
    for i in 0..240 {
        page.push_str(&format!("<b id={i}>"));
    }
    page.push_str(&"<p>x".repeat(100_000));
    let e = with_bounds(
        Duration::from_secs(120),
        256 << 20,
        html(&bin, page.into_bytes(), MAX_UNCOMPRESSED_BYTES),
    )
    .await
    .unwrap_err();
    assert_eq!(e, ReadError::OutOfMemory);
    let (code, why) = e.failure();
    assert_eq!(code, ErrorCode::Unreadable);
    assert!(why.contains("READER_MEMORY_LIMIT_BYTES (4.0 GiB)"), "{why}");
}

#[tokio::test]
async fn an_answer_past_its_bound_is_output_too_large() {
    let bin = real().await;
    let page = format!("<p>{}</p>", "word ".repeat(100_000)).into_bytes();
    let e = html(&bin, page, 64 << 10).await.unwrap_err();
    assert_eq!(e, ReadError::TooLarge);
    assert_eq!(e.failure().0, ErrorCode::OutputTooLarge);
}

/// A reader of another lmgw, one that answers nonsense, one that dies: each
/// its own named failure, with nothing of its stderr in it.
#[tokio::test]
async fn a_reader_that_answers_wrongly_is_named_without_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let other = stand_in(dir.path(), "cat >/dev/null; printf 'lmgw-doc-reader/99\\n'");
    let e = html(&other, b"x".to_vec(), 1 << 20).await.unwrap_err();
    assert_eq!(e, ReadError::OtherVersion("99".into()));
    let (code, why) = e.failure();
    assert_eq!(code, ErrorCode::ToolMissing);
    assert!(why.contains("restart lmgw"), "{why}");

    let dir = tempfile::tempdir().unwrap();
    let noise = stand_in(dir.path(), "cat >/dev/null; echo 'not a frame'");
    let e = html(&noise, b"x".to_vec(), 1 << 20).await.unwrap_err();
    assert!(
        matches!(&e, ReadError::Stopped(w) if w.contains("magic line")),
        "{e:?}"
    );

    let dir = tempfile::tempdir().unwrap();
    let dies = stand_in(
        dir.path(),
        "cat >/dev/null; echo 'the secret words of the document' >&2; exit 7",
    );
    let e = html(&dies, b"x".to_vec(), 1 << 20).await.unwrap_err();
    let ReadError::Stopped(status) = &e else {
        panic!("{e:?}");
    };
    assert!(status.contains('7'), "{status}");
    assert!(!e.said().contains("secret"), "{}", e.said());

    // An answer that names more attachment bytes than it carries.
    let dir = tempfile::tempdir().unwrap();
    let frame = dir.path().join("frame");
    let mail = wire::Mail {
        attachments: vec![wire::Attachment {
            name: "a.bin".into(),
            content_type: "application/octet-stream".into(),
            len: 1 << 40,
        }],
        ..Default::default()
    };
    let mut bytes = Vec::new();
    wire::write(&mut bytes, &Answer::Mail(mail), &[b"short"]).unwrap();
    std::fs::write(&frame, bytes).unwrap();
    let short = stand_in(
        dir.path(),
        &format!("cat >/dev/null; cat '{}'", frame.display()),
    );
    let f = html(&short, b"x".to_vec(), 1 << 20).await.unwrap();
    assert!(f.blobs(&[1 << 40]).is_none());
    assert!(f.blobs(&[5]).is_some());
}

#[tokio::test]
async fn a_reader_that_is_not_there_is_missing() {
    let e = read(
        &ToolFacts::all_missing(),
        Op::Html,
        Input::Bytes(Bytes::new()),
        1 << 20,
    )
    .await
    .unwrap_err();
    assert_eq!(e, ReadError::Missing);
    let (code, why) = e.failure();
    assert_eq!(code, ErrorCode::ToolMissing);
    assert!(why.contains("install lmgw again"), "{why}");
    let slot = pool::global().acquire().await;
    let gone = std::path::Path::new("/nonexistent/lmgw-doc-reader");
    let e = read_with(&slot, gone, Op::Html, Input::Bytes(Bytes::new()), 1 << 20)
        .await
        .unwrap_err();
    assert_eq!(e, ReadError::Missing);
}

/// A read whose caller goes away is stopped, and its slot stays in use
/// until the child has been reaped: the pool never counts as free a slot
/// whose reader still has memory.
#[tokio::test]
async fn a_dropped_read_keeps_its_slot_until_the_child_is_gone() {
    let bin = real().await;
    let pool = pool::ToolPool::new(Machine {
        cores: 1,
        memory: None,
    });
    let slot = pool.acquire().await;
    let page = "<div/>".repeat(1_000_000).into_bytes();
    let run = read_with(
        &slot,
        &bin,
        Op::Html,
        Input::Bytes(Bytes::from(page)),
        MAX_UNCOMPRESSED_BYTES,
    );
    // Long enough for the child to be parsing.
    assert!(tokio::time::timeout(Duration::from_millis(300), run)
        .await
        .is_err());
    // The caller's own hold goes too: only the reaper's lease is left.
    drop(slot);
    let children = || {
        std::fs::read_dir("/proc/self/task")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|t| std::fs::read_to_string(t.path().join("children")).ok())
            .flat_map(|c| c.split_whitespace().map(str::to_string).collect::<Vec<_>>())
            .filter(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .is_ok_and(|c| c.trim() == &READER_BIN[..15])
            })
            .count()
    };
    let started = std::time::Instant::now();
    loop {
        // Read in this order: a slot counted free must mean no child.
        let running = pool.facts().running;
        if running == 0 {
            assert_eq!(
                children(),
                0,
                "the slot was freed before the reader was reaped"
            );
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(20), "never reaped");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
