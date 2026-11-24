//! The WP7 handlers through the conversion: pandoc's formats and a Word
//! file with equations with and without pandoc, an old Word file through
//! LibreOffice, audio and video without a chosen model or ffmpeg. The
//! tests that need pandoc or LibreOffice skip without them (`ci/check.sh`
//! says which document tools the suite found); the ASR call itself is
//! tested over HTTP against a mock upstream (`tests/it/documents_media.rs`).

use lmgw_api_types::documents::convert::Output;

use super::*;

/// What this machine has, its own reader included.
async fn here() -> std::sync::Arc<ToolFacts> {
    DocTools::on_path_with_own(std::env::var_os("PATH").unwrap_or_default())
        .current()
        .await
}

fn found(facts: &ToolFacts, id: ToolId) -> bool {
    matches!(facts.fact(id), Some(Fact::Found(_)))
}

/// `facts` without `id`.
fn without(facts: &ToolFacts, id: ToolId) -> ToolFacts {
    let mut f = facts.clone();
    if let Some(t) = f.tools.get_mut(&id) {
        t.fact = Fact::Missing;
    }
    f
}

fn named(name: &str, bytes: Vec<u8>) -> Input {
    Input {
        index: 0,
        name: name.into(),
        bytes: Ok(Bytes::from(bytes)),
    }
}

/// A docx whose second paragraph holds an OMML equation (E = mc²).
fn docx_with_equation() -> Vec<u8> {
    test_files::docx_xml(
        r#"<w:p><w:r><w:t>Energy</w:t></w:r></w:p><w:p><m:oMath><m:r><m:t>E</m:t></m:r><m:r><m:t>=</m:t></m:r><m:r><m:t>m</m:t></m:r><m:sSup><m:e><m:r><m:t>c</m:t></m:r></m:e><m:sup><m:r><m:t>2</m:t></m:r></m:sup></m:sSup></m:oMath></w:p>"#,
        None,
    )
}

#[tokio::test]
async fn a_word_file_with_equations_reads_them_as_latex_through_pandoc() {
    let facts = here().await;
    let plan = plan_of(Default::default());
    // Without pandoc: lmgw's own reader, the equations named.
    let none = without(&facts, ToolId::Pandoc);
    let d = convert(named("eq.docx", docx_with_equation()), &plan, &none)
        .await
        .doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    assert_eq!(d.format.as_deref(), Some("docx_math"));
    let w = d
        .warnings
        .iter()
        .find(|w| w.code == WarningCode::EquationsDropped)
        .expect("equations_dropped");
    assert!(w.message.contains("install pandoc"), "{}", w.message);
    if !found(&facts, ToolId::Pandoc) {
        eprintln!("skipped: pandoc is not on PATH");
        return;
    }
    // A docx as Word writes one (pandoc's, with its styles and relations).
    let t = tempfile::tempdir().unwrap();
    let md = t.path().join("eq.md");
    let docx = t.path().join("eq.docx");
    std::fs::write(&md, "Energy\n\n$E = mc^2$\n").unwrap();
    let made = std::process::Command::new("pandoc")
        .arg(&md)
        .arg("-o")
        .arg(&docx)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let bytes = std::fs::read(&docx).unwrap();
    assert_eq!(
        crate::extract::sniff::sniff_convert(&bytes).map(|s| s.sub),
        Ok("docx+math")
    );
    let done = convert(named("eq.docx", bytes), &plan, &facts).await;
    let d = done.doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    let md = d.content.unwrap();
    assert!(md.contains("Energy"), "{md}");
    assert!(md.contains("$E = mc^{2}$"), "{md}");
    assert!(d
        .warnings
        .iter()
        .all(|w| w.code != WarningCode::EquationsDropped));
    assert_eq!(done.tool, Some(ToolId::Pandoc));
    assert_eq!(done.also, vec![ToolId::Reader]);
}

#[tokio::test]
async fn text_markups_go_through_pandoc_or_stay_text_naming_it() {
    let facts = here().await;
    let tex = b"\\section{Results}\n\nSome \\textbf{bold} words.\n\n\\begin{itemize}\\item one\\end{itemize}\n".to_vec();
    let plan = plan_of(Default::default());
    let none = without(&facts, ToolId::Pandoc);
    let d = convert(named("paper.tex", tex.clone()), &plan, &none)
        .await
        .doc;
    assert_eq!(d.format.as_deref(), Some("latex"));
    assert_eq!(
        d.content.as_deref(),
        Some(std::str::from_utf8(&tex).unwrap())
    );
    assert_eq!(d.warnings[0].code, WarningCode::ToolMissing);
    assert_eq!(d.warnings[0].tool.as_deref(), Some("pandoc"));
    if !found(&facts, ToolId::Pandoc) {
        eprintln!("skipped: pandoc is not on PATH");
        return;
    }
    let d = convert(named("paper.tex", tex), &plan, &facts).await.doc;
    let md = d.content.unwrap();
    assert!(md.starts_with("# Results"), "{md}");
    assert!(md.contains("**bold**") && md.contains("one"), "{md}");
    // A document that asks pandoc to read a file gets nothing of it.
    let include = b"\\input{/etc/hostname}\n\nafter\n".to_vec();
    let d = convert(named("x.tex", include), &plan, &facts).await.doc;
    assert_eq!(d.content.as_deref().map(str::trim), Some("after"));
    // RTF by its bytes, json too.
    let rtf = b"{\\rtf1\\ansi{\\b Title}\\par Body text.\\par}".to_vec();
    let json = plan_of(ConvertOptions {
        output: Output::Json,
        ..Default::default()
    });
    let d = convert(named("x.bin", rtf), &json, &facts).await.doc;
    assert_eq!(d.format.as_deref(), Some("rtf"));
    let s = serde_json::to_string(&d.structure).unwrap();
    assert!(s.contains("Body text."), "{s}");
}

#[tokio::test]
async fn an_old_word_file_reads_through_libreoffice_as_its_docx() {
    let facts = here().await;
    let Some(Fact::Found(lo)) = facts.fact(ToolId::LibreOffice) else {
        eprintln!("skipped: LibreOffice is not on PATH");
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("source.rtf");
    std::fs::write(&src, b"{\\rtf1\\ansi Hello from Word 97\\par Second\\par}").unwrap();
    let made = std::process::Command::new(&lo.located.path)
        .arg("--headless")
        .arg(format!(
            "-env:UserInstallation={}",
            crate::extract::convert::probe::file_url(&t.path().join("maker"))
        ))
        .args(["--convert-to", "doc", "--outdir"])
        .arg(t.path())
        .arg(&src)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let doc = std::fs::read(t.path().join("source.doc")).unwrap();
    let mut plan = plan_of(Default::default());
    plan.spool_dir = Some(t.path().join("spool"));
    let done = convert(named("old.doc", doc.clone()), &plan, &facts).await;
    let d = done.doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    assert_eq!(d.format.as_deref(), Some("doc"));
    let md = d.content.unwrap();
    assert!(
        md.contains("Hello from Word 97") && md.contains("Second"),
        "{md}"
    );
    assert_eq!(done.tool, Some(ToolId::LibreOffice));
    // Without LibreOffice: named, with what to install.
    let none = without(&facts, ToolId::LibreOffice);
    let d = convert(named("old.doc", doc), &plan, &none).await.doc;
    let e = d.error.unwrap();
    assert_eq!(
        (e.code, e.tool.as_deref()),
        (ErrorCode::ToolMissing, Some("libreoffice"))
    );
    assert!(e.message.contains("libreoffice-writer"), "{}", e.message);
}

#[tokio::test]
async fn audio_and_video_need_the_model_and_video_ffmpeg() {
    let none = ToolFacts::all_missing();
    let plan = plan_of(Default::default());
    let wav = convert(named("a.wav", b"RIFF\0\0\0\0WAVE".to_vec()), &plan, &none)
        .await
        .doc;
    let e = wav.error.unwrap();
    assert_eq!(e.code, ErrorCode::ModelNotChosen);
    assert!(e.message.contains("Transcribe audio with"), "{}", e.message);
    let mut mp4 = vec![0, 0, 0, 16];
    mp4.extend_from_slice(b"ftypisom\0\0\0\0");
    let v = convert(named("v.mp4", mp4), &plan, &none).await.doc;
    assert_eq!(v.format.as_deref(), Some("video"));
    assert_eq!(v.error.unwrap().code, ErrorCode::ToolMissing);
}

#[tokio::test]
async fn a_password_protected_office_file_is_named() {
    let mut cfb = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
    cfb.create_stream("/EncryptionInfo").unwrap();
    cfb.create_stream("/EncryptedPackage").unwrap();
    cfb.flush().unwrap();
    let bytes = cfb.into_inner().into_inner();
    let d = convert(
        named("locked.docx", bytes),
        &plan_of(Default::default()),
        &ToolFacts::all_missing(),
    )
    .await
    .doc;
    let e = d.error.unwrap();
    assert_eq!(e.code, ErrorCode::Unreadable);
    assert!(e.message.contains("password"), "{}", e.message);
}
