//! The reader as the gateway starts it: a request on the command line, the
//! file on stdin, one frame on stdout.

use std::io::Write;
use std::process::{Command, Stdio};

use lmgw_doc_reader::wire::{self, Answer, Failure, Op, WIRE_VERSION};

const READER: &str = env!("CARGO_BIN_EXE_lmgw-doc-reader");

/// The reader's stdout, stderr and exit status for `args` with `input` on
/// stdin.
fn run(args: &[String], input: &[u8]) -> (Vec<u8>, String, Option<i32>) {
    let mut child = Command::new(READER)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    (
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

fn answer(op: Op, input: &[u8]) -> (Answer, Vec<u8>) {
    let (out, err, status) = run(&op.args(), input);
    assert_eq!(status, Some(0), "{err}");
    let (answer, at) = wire::read(&out).unwrap();
    (answer, out[at..].to_vec())
}

#[test]
fn it_says_its_version_and_its_wire_version() {
    let (out, _, status) = run(&["--version".to_string()], b"");
    assert_eq!(status, Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out),
        format!(
            "lmgw-doc-reader {} (wire {WIRE_VERSION})\n",
            env!("CARGO_PKG_VERSION")
        )
    );
}

#[test]
fn a_command_line_that_is_no_request_is_refused_without_reading_stdin() {
    for args in [vec![], vec!["pdf".to_string()], vec!["epub".to_string()]] {
        let (out, err, status) = run(&args, b"ignored");
        assert_eq!(status, Some(2), "{args:?}");
        assert!(out.is_empty());
        assert!(err.contains("started by lmgw"), "{err}");
    }
}

#[test]
fn a_page_comes_back_as_markdown_in_a_frame() {
    let page = b"<meta charset=\"iso-8859-1\"><h1>Caf\xE9</h1><p>Text.</p>";
    let (a, rest) = answer(Op::Html, page);
    let Answer::Page(p) = a else {
        panic!("{a:?}");
    };
    assert_eq!(p.markdown, "# Café\n\nText.");
    assert_eq!(p.too_deep, None);
    assert!(rest.is_empty());
}

#[test]
fn a_named_failure_travels_in_the_frame_with_status_zero() {
    let (a, _) = answer(Op::Xls, b"not a workbook");
    assert!(matches!(a, Answer::Failed(Failure::Unreadable(_))), "{a:?}");
    let (a, _) = answer(Op::Epub { budget: 1 << 20 }, b"PK\x05\x06");
    assert!(matches!(a, Answer::Failed(Failure::Unreadable(_))), "{a:?}");
    let (a, _) = answer(Op::Email, b"");
    assert!(matches!(a, Answer::Failed(_)), "{a:?}");
}

#[test]
fn a_message_s_attachments_follow_the_header_as_bytes() {
    let mail = "From: a@example.com\r\nTo: b@example.com\r\nSubject: s\r\nMIME-Version: 1.0\r\n\
                Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
                --b\r\nContent-Type: text/plain\r\n\r\nBody.\r\n\
                --b\r\nContent-Type: application/octet-stream\r\n\
                Content-Disposition: attachment; filename=\"a.bin\"\r\n\
                Content-Transfer-Encoding: base64\r\n\r\nAAEC/w==\r\n--b--\r\n";
    let (out, _, status) = run(&Op::Email.args(), mail.as_bytes());
    assert_eq!(status, Some(0));
    let (a, at) = wire::read(&out).unwrap();
    let Answer::Mail(m) = a else {
        panic!("{a:?}");
    };
    assert_eq!(m.attachments.len(), 1);
    let lens: Vec<u64> = m.attachments.iter().map(|a| a.len).collect();
    let ranges = wire::blobs(&out, at, &lens).unwrap();
    assert_eq!(&out[ranges[0].clone()], [0, 1, 2, 255]);
    assert_eq!(ranges[0].end, out.len(), "nothing after the last blob");
}

/// What the reader says about a page it does not write (too deep) is in
/// the frame; its stderr stays empty and never carries the document.
#[test]
fn stderr_never_carries_the_document() {
    let secret = "s3cret-words-of-the-document";
    let page = format!("<p>{secret}</p>{}", "<div>".repeat(300));
    let (out, err, status) = run(&Op::Html.args(), page.as_bytes());
    assert_eq!(status, Some(0));
    assert!(!err.contains(secret), "{err}");
    let (a, _) = wire::read(&out).unwrap();
    // Too deep: answered, nothing written, nothing on stderr.
    assert!(matches!(a, Answer::Page(p) if p.too_deep.is_some() && p.markdown.is_empty()));
    assert!(err.is_empty(), "{err}");
}

/// Given a memory limit, the reader confines itself before it reads: the
/// limit is its own (`RLIMIT_AS`), and a descriptor its parent left open
/// for it is closed. Looked at while it waits for its stdin.
#[cfg(target_os = "linux")]
#[test]
fn it_limits_itself_and_closes_what_it_inherited_before_it_reads() {
    use std::os::fd::AsRawFd;
    let limit: u64 = 768 << 20;
    // A descriptor without close-on-exec, as a library of the gateway's
    // process might hold one: the reader inherits it.
    let file = std::fs::File::open("/proc/self/exe").unwrap();
    // SAFETY: duplicating a descriptor this test owns; the duplicate (which
    // has no close-on-exec flag) is closed below.
    let leaked = unsafe { libc_dup(file.as_raw_fd()) };
    assert!(leaked > 2);
    let mut child = Command::new(READER)
        .args([wire::MEMORY_LIMIT_FLAG, &limit.to_string(), "html"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // SAFETY: closing the duplicate made above.
    unsafe { libc_close(leaked) };
    let pid = child.id();
    // It blocks on stdin once it has confined itself: wait for the limit
    // to show.
    let limits = || std::fs::read_to_string(format!("/proc/{pid}/limits")).unwrap_or_default();
    let started = std::time::Instant::now();
    while !limits()
        .lines()
        .any(|l| l.starts_with("Max address space") && l.contains(&limit.to_string()))
    {
        assert!(
            started.elapsed().as_secs() < 10,
            "the limit never showed: {}",
            limits()
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let open: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let mut sorted = open.clone();
    sorted.sort();
    assert_eq!(sorted, ["0", "1", "2"], "inherited descriptors stay open");
    // And it reads as usual under the limit.
    child.stdin.take().unwrap().write_all(b"<p>ok</p>").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let (a, _) = wire::read(&out.stdout).unwrap();
    assert!(matches!(a, Answer::Page(p) if p.markdown == "ok"));
    // A limit it cannot set is refused before anything is read.
    let (out, err, status) = run(
        &[
            wire::MEMORY_LIMIT_FLAG.to_string(),
            "x".into(),
            "html".into(),
        ],
        b"<p>ok</p>",
    );
    assert_eq!((status, out.is_empty()), (Some(2), true), "{err}");
}

#[cfg(target_os = "linux")]
extern "C" {
    #[link_name = "dup"]
    fn libc_dup(fd: i32) -> i32;
    #[link_name = "close"]
    fn libc_close(fd: i32) -> i32;
}
