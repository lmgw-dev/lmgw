//! A spooled PDF read by poppler confined (performance round review B2):
//! by position through `/dev/stdin`, so a run needs far less memory than
//! the file; the run reaches its own spool and nothing beside it, and
//! cannot write it.

use std::path::Path;
use std::time::Duration;

use super::*;
use crate::extract::convert::pool;
use crate::extract::convert::run::{self, Bounds, RunError, Tool};

/// A one-page PDF of `pad` bytes and more: a comment block before its
/// first object, its cross-reference table at the end.
fn padded(pad: usize) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let line = [b"%".as_slice(), &[b'p'; 1022], b"\n"].concat();
    for _ in 0..pad / line.len() {
        out.extend_from_slice(&line);
    }
    let content = b"BT /F1 12 Tf 20 100 Td (Example padded) Tj ET";
    let objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
           /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(),
        [
            format!("<< /Length {} >>\nstream\n", content.len()).as_bytes(),
            content,
            b"\nendstream",
        ]
        .concat(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

#[tokio::test]
async fn a_confined_poppler_reads_its_spool_by_position() {
    if crate::extract::convert::confine::test_helper("a_confined_poppler_reads_its_spool")
        .await
        .is_none()
    {
        return;
    }
    let bin = Path::new(crate::extract::pdf::PDFTOTEXT);
    if run::start::absolute(bin).is_none() {
        eprintln!("skipped: poppler is not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // 96 MiB of file, 80 MiB of address space: read whole (as `fd://0` or a
    // pipe makes poppler read it) it cannot fit; read by position it does.
    let spool = Spool::of(Some(dir.path()), Bytes::from(padded(96 << 20))).await;
    let input = spool.input();
    assert!(matches!(input, Input::File(_)));
    let arg = file_arg(&input);
    assert_eq!(arg, STDIN_PATH);
    let slot = pool::global().acquire().await;
    let bounds = Bounds {
        timeout: Duration::from_secs(60),
        output: 1 << 20,
        memory: 80 << 20,
    };
    let out = run::poppler_held(&slot, None, bin, &[arg, "-"], input, bounds)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&out).contains("Example padded"));
}

#[tokio::test]
async fn a_confined_run_reaches_its_own_spool_alone_and_only_to_read() {
    let Some(_) = crate::extract::convert::confine::test_helper("a_confined_run_reaches").await
    else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let beside = dir.path().join("beside.example");
    std::fs::write(&beside, b"another file").unwrap();
    let spool = Spool::of(Some(dir.path()), Bytes::from_static(b"its own spool")).await;
    let sh = Path::new("/usr/bin/sh");
    let rules = crate::extract::convert::confine::poppler(sh, None).unwrap();
    let slot = pool::global().acquire().await;
    let bounds = Bounds {
        timeout: Duration::from_secs(30),
        output: 1 << 20,
        memory: 1 << 30,
    };
    let run = |script: String| {
        let args = ["-c".to_string(), script];
        let input = spool.input();
        let rules = &rules;
        let slot = &slot;
        async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let tool = Tool {
                rules: Some(rules),
                ..Tool::new(sh, &args)
            };
            run::run_held(slot, tool, input, bounds).await
        }
    };
    let own = run("cat /dev/stdin".into()).await.unwrap();
    assert_eq!(own, b"its own spool");
    let other = run(format!("cat '{}'", beside.display())).await;
    assert!(matches!(other, Err(RunError::Failed(_))), "{other:?}");
    let write = run("echo x 1<> /dev/stdin".into()).await;
    assert!(matches!(write, Err(RunError::Failed(_))), "{write:?}");
    let back = run("cat /dev/stdin".into()).await.unwrap();
    assert_eq!(back, b"its own spool");
}
