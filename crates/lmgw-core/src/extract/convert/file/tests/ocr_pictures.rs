//! Text recognition beside picture export (WP4 on WP10), with this
//! machine's poppler and tesseract: a scan converts by recognition and its
//! page-sized image is no picture, a smaller figure on a scanned page is
//! exported where it sits among the recognised text, a mixed PDF with the
//! layout model's recorded regions, the file's clock counting a page that
//! works while another waits for its slot, and a conversion stopped while
//! a page is recognised leaving no child and no slot behind.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lmgw_api_types::documents::convert::{ErrorCode, Ocr, Pictures};
use serde_json::Value;

use super::ocr::{scanned, with_ocr, without_osd, PAGE_LINES};
use super::*;
use crate::extract::convert::layout_model::{Label, PageRegions, Region, Regions, RECORDED};
use crate::extract::convert::{batch, pool};

/// One page: a scan drawn over the whole page (grey pixels), a line of text
/// in its text layer, and figures of noise at `[x, y, w, h]` points from the
/// top left.
pub(super) struct Sheet<'a> {
    pub scan: Option<&'a (u32, u32, Vec<u8>)>,
    pub text: Option<&'a str>,
    pub figures: Vec<[f32; 4]>,
}

/// An A4 PDF of `sheets`.
pub(super) fn pdf(sheets: &[Sheet<'_>]) -> Vec<u8> {
    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        Vec::new(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut kids = Vec::new();
    for s in sheets {
        let (mut content, mut xobjs) = (String::new(), String::new());
        if let Some((w, h, px)) = s.scan {
            let mut o = format!(
                "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace \
                 /DeviceGray /BitsPerComponent 8 /Length {} >>\nstream\n",
                px.len()
            )
            .into_bytes();
            o.extend_from_slice(px);
            o.extend_from_slice(b"\nendstream");
            objs.push(o);
            xobjs.push_str(&format!("/Scan {} 0 R ", objs.len()));
            content.push_str("q 595 0 0 842 0 0 cm /Scan Do Q\n");
        }
        for (i, [x, y, w, h]) in s.figures.iter().enumerate() {
            // Noise, so that its crop is a PNG of some size (about 0.5 MiB at
            // the default scale for 200 × 150 pt), as a photo's is.
            let (pw, ph) = (300u32, 200u32);
            let mut seed = 0x2545_f491_u32;
            let px: Vec<u8> = (0..pw * ph * 3)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 24) as u8
                })
                .collect();
            let mut o = format!(
                "<< /Type /XObject /Subtype /Image /Width {pw} /Height {ph} /ColorSpace \
                 /DeviceRGB /BitsPerComponent 8 /Length {} >>\nstream\n",
                px.len()
            )
            .into_bytes();
            o.extend_from_slice(&px);
            o.extend_from_slice(b"\nendstream");
            objs.push(o);
            xobjs.push_str(&format!("/Fig{i} {} 0 R ", objs.len()));
            content.push_str(&format!(
                "q {w} 0 0 {h} {x} {} cm /Fig{i} Do Q\n",
                842.0 - y - h
            ));
        }
        if let Some(t) = s.text {
            content.push_str(&format!("BT /F1 16 Tf 60 760 Td ({t}) Tj ET\n"));
        }
        let mut c = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        c.extend_from_slice(content.as_bytes());
        c.extend_from_slice(b"\nendstream");
        objs.push(c);
        let contents = objs.len();
        objs.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents {contents} 0 R \
                 /Resources << /Font << /F1 3 0 R >> /XObject << {xobjs}>> >> >>"
            )
            .into_bytes(),
        );
        kids.push(format!("{} 0 R", objs.len()));
    }
    objs[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        kids.len()
    )
    .into_bytes();
    let mut out = b"%PDF-1.4\n".to_vec();
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

fn plan(ocr: Ocr, pictures: Pictures, output: Output) -> Plan {
    plan_of(ConvertOptions {
        ocr,
        pictures,
        output,
        ..Default::default()
    })
}

fn left_out(d: &ConvertedDocument) -> Option<&ConvertWarning> {
    d.warnings
        .iter()
        .find(|w| w.code == WarningCode::PicturesLeftOut)
}

/// A scan converts by text recognition (`main` answered `no_reader`), and
/// its page-sized image is the page, not a picture: left out, counted, and
/// said to be read by recognition. With `ocr: off` the page is not read.
#[tokio::test]
async fn a_scan_converts_by_recognition_and_its_page_image_is_no_picture() {
    let Some(facts) = with_ocr().await else {
        return;
    };
    let facts = without_osd(&facts);
    let page = scanned(PAGE_LINES).await;
    let file = pdf(&[Sheet {
        scan: Some(&page),
        text: None,
        figures: Vec::new(),
    }]);
    for mode in [Pictures::Placeholder, Pictures::Referenced] {
        let d = convert(
            input(file.clone()),
            &plan(Ocr::Auto, mode, Output::Markdown),
            &facts,
        )
        .await
        .doc;
        assert_eq!(d.status, DocStatus::Converted, "{mode:?}: {d:?}");
        assert_eq!(d.stats.parts_by_source.ocr, 1);
        assert!(d.content.as_deref().unwrap().contains("find words"));
        assert!(d.pictures.is_empty(), "{:?}", d.pictures);
        let w = left_out(&d).expect("the page image is counted");
        assert!(
            w.message.contains("1 covering their whole page")
                && w.message.contains("read by text recognition"),
            "{}",
            w.message
        );
    }
    let d = convert(
        input(file),
        &plan(Ocr::Off, Pictures::Referenced, Output::Markdown),
        &facts,
    )
    .await
    .doc;
    assert_eq!(d.status, DocStatus::Failed, "{d:?}");
    assert_eq!(d.error.unwrap().code, ErrorCode::NoReader);
}

/// A scanned page with a smaller figure on it: its text is recognised, the
/// scan is the page (left out as a cover), and the figure is exported as a
/// picture placed by its box among the recognised blocks, after the text
/// above it.
#[tokio::test]
async fn a_figure_on_a_scanned_page_is_exported_among_its_recognised_text() {
    let Some(facts) = with_ocr().await else {
        return;
    };
    let facts = without_osd(&facts);
    let page = scanned(PAGE_LINES).await;
    let file = pdf(&[Sheet {
        scan: Some(&page),
        text: None,
        figures: vec![[300.0, 500.0, 200.0, 150.0]],
    }]);
    let d = convert(
        input(file.clone()),
        &plan(Ocr::Auto, Pictures::Referenced, Output::Markdown),
        &facts,
    )
    .await
    .doc;
    assert_eq!(d.status, DocStatus::Converted, "{d:?}");
    assert_eq!(d.stats.parts_by_source.ocr, 1);
    assert_eq!(d.pictures.len(), 1, "{:?}", d.warnings);
    let p = &d.pictures[0];
    let [x0, y0, x1, y1] = p.bbox.unwrap();
    assert!(
        (x0 - 300.0).abs() < 2.0 && (y0 - 500.0).abs() < 2.0,
        "{p:?}"
    );
    assert!(
        (x1 - 500.0).abs() < 2.0 && (y1 - 650.0).abs() < 2.0,
        "{p:?}"
    );
    assert!(p.data_base64.is_some(), "{p:?}");
    assert_eq!(d.stats.pictures, 1);
    let md = d.content.as_deref().unwrap();
    let (text, picture) = (md.find("find words"), md.find("![Image](artifacts/"));
    assert!(text.is_some() && text < picture, "{md}");
    assert!(left_out(&d).unwrap().message.contains("1 covering"));
    // json: the page's blocks are recognised text and one figure naming it.
    let d = convert(
        input(file),
        &plan(Ocr::Auto, Pictures::Referenced, Output::Json),
        &facts,
    )
    .await
    .doc;
    let s = serde_json::to_value(d.structure.unwrap()).unwrap();
    let blocks = s[0]["blocks"].as_array().unwrap();
    let figures: Vec<&Value> = blocks.iter().filter(|b| b["kind"] == "figure").collect();
    assert_eq!(figures.len(), 1, "{blocks:?}");
    assert_eq!(figures[0]["picture"], "p1");
    assert!(blocks
        .iter()
        .filter(|b| b["kind"] != "figure")
        .all(|b| b["source"] == "ocr"));
}

/// One region on an A4 page, in points.
fn reg(label: Label, x0: f32, y0: f32, x1: f32, y1: f32) -> Region {
    Region::checked(label, 0.9, x0 / 595.0, y0 / 842.0, x1 / 595.0, y1 / 842.0).unwrap()
}

/// A PDF of a text page and a scanned page with the layout model on (its
/// answer recorded, for both pages): the text page is laid out from its
/// regions, the scanned one is recognised, and neither stage trips the
/// other (regions for OCR pages are not built yet).
#[tokio::test]
async fn a_mixed_pdf_with_the_layout_model_reads_both_kinds_of_page() {
    let Some(facts) = with_ocr().await else {
        return;
    };
    let facts = without_osd(&facts);
    let page = scanned(PAGE_LINES).await;
    let file = pdf(&[
        Sheet {
            scan: None,
            text: Some("A page with its own text layer"),
            figures: Vec::new(),
        },
        Sheet {
            scan: Some(&page),
            text: None,
            figures: Vec::new(),
        },
    ]);
    let mut r = Regions::default();
    r.pages.insert(
        1,
        PageRegions::Found(vec![reg(Label::Text, 55.0, 60.0, 400.0, 90.0)]),
    );
    r.pages.insert(
        2,
        PageRegions::Found(vec![
            reg(Label::Text, 55.0, 70.0, 560.0, 200.0),
            reg(Label::Picture, 0.0, 0.0, 595.0, 842.0),
        ]),
    );
    for (pictures, output) in [
        (Pictures::Placeholder, Output::Markdown),
        (Pictures::Referenced, Output::Json),
    ] {
        let d = RECORDED
            .scope(
                r.clone(),
                convert(
                    input(file.clone()),
                    &plan(Ocr::Auto, pictures, output),
                    &facts,
                ),
            )
            .await
            .doc;
        assert_eq!(d.status, DocStatus::Converted, "{output:?}: {d:?}");
        let by = &d.stats.parts_by_source;
        assert_eq!((by.text_layer, by.ocr), (1, 1), "{d:?}");
        if output == Output::Json {
            let s = serde_json::to_value(d.structure.as_ref().unwrap()).unwrap();
            assert!(s[0]["blocks"][0]["text"]
                .as_str()
                .unwrap()
                .contains("own text layer"));
            let second = s[1]["blocks"].as_array().unwrap();
            assert!(!second.is_empty());
            assert!(second.iter().all(|b| b["source"] == "ocr"), "{second:?}");
        } else {
            let md = d.content.unwrap();
            assert!(
                md.contains("own text layer") && md.contains("find words"),
                "{md}"
            );
        }
    }
}

/// `name` on this machine's `PATH`.
fn real(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Stand-ins for poppler and tesseract in `dir`: each writes its pid to
/// `dir/pids` and runs the real tool, except tesseract's recognition runs
/// (all but `--version` and `--list-langs`), which write theirs to
/// `dir/held` and sleep for `hold` seconds. `None` without the tools.
fn stand_ins(dir: &Path, hold: u32) -> Option<()> {
    for name in [
        "pdftotext",
        "pdftohtml",
        "pdftoppm",
        "pdfinfo",
        "pdfimages",
        "tesseract",
    ] {
        let Some(bin) = real(name) else {
            eprintln!("skipped: {name} is not on PATH");
            return None;
        };
        // The tools run confined (`confine.rs`): they can write nothing, so
        // each stand-in stays a shell whose command line names it (its own
        // path), found in `/proc` ([`pids_of`]), with the real tool, or
        // tesseract's wait, as its child.
        let hold = if name == "tesseract" {
            format!(
                "case \" $* \" in *\" --version \"*|*\" --list-langs \"*) ;; *) sleep {hold}; \
                 exit;; esac\n"
            )
        } else {
            String::new()
        };
        let path = dir.join(name);
        let script = format!("#!/bin/sh\n{hold}'{}' \"$@\"\n", bin.display());
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    Some(())
}

/// The processes whose command line names a stand-in in `dir` (and, with
/// `with`, holds that word too: tesseract's `stdin` is a page's read).
fn pids_of(dir: &Path, with: Option<&str>) -> Vec<u32> {
    let mark = dir.display().to_string();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                return false;
            };
            let words: Vec<String> = cmd
                .split(|b| *b == 0)
                .map(|w| String::from_utf8_lossy(w).into_owned())
                .collect();
            words.iter().any(|w| w.starts_with(&mark))
                && with.is_none_or(|x| words.iter().any(|w| w == x))
        })
        .filter(|pid| !ended(*pid))
        .collect()
}

/// Whether process `pid` has ended (gone, or a zombie its parent reaps).
fn ended(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| {
        s.rsplit_once(") ")
            .is_some_and(|(_, r)| r.starts_with('Z') || r.starts_with('X'))
    })
}

/// Wait until no stand-in of `dir` runs any more and the pool runs nothing.
async fn all_gone(dir: &Path) {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if pids_of(dir, None).is_empty() && pool::global().facts().running == 0 {
            return;
        }
        assert!(Instant::now() < until, "a child or a slot is still held");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A scanned PDF with a picture (`ocr: auto`, `pictures: referenced`) whose
/// conversion is dropped (the client went) while tesseract recognises a
/// page: every child of the file ends, the picture runs' included, and
/// every slot comes back.
#[tokio::test]
async fn a_conversion_stopped_mid_recognition_leaves_no_child_and_no_slot() {
    if with_ocr().await.is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    if stand_ins(dir.path(), 60).is_none() {
        return;
    }
    let facts = crate::extract::convert::tools::DocTools::on_path_with_own(
        dir.path().as_os_str().to_owned(),
    )
    .current()
    .await;
    let page = scanned(PAGE_LINES).await;
    let sheet = || Sheet {
        scan: Some(&page),
        text: None,
        figures: vec![[300.0, 500.0, 200.0, 150.0]],
    };
    let file = pdf(&[sheet(), sheet()]);
    let p = plan(Ocr::Auto, Pictures::Referenced, Output::Markdown);
    let task = tokio::spawn(async move { convert(input(file), &p, &facts).await });
    let until = Instant::now() + Duration::from_secs(30);
    while pids_of(&dir.path().join("tesseract"), Some("stdin")).is_empty() {
        assert!(Instant::now() < until, "no page reached tesseract");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    all_gone(dir.path()).await;
}

/// The file's clock counts while any of its lanes works: a page being
/// recognised counts while another page waits for its slot, so a
/// `document_timeout` ends the file after its own time, not after the
/// working page is done (each page is a lane of its own).
#[tokio::test]
async fn a_page_that_works_counts_while_another_waits_for_its_slot() {
    if with_ocr().await.is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    if stand_ins(dir.path(), 60).is_none() {
        return;
    }
    let facts = crate::extract::convert::tools::DocTools::on_path_with_own(
        dir.path().as_os_str().to_owned(),
    )
    .current()
    .await;
    let page = scanned(&["A page that waits"]).await;
    let sheet = || Sheet {
        scan: Some(&page),
        text: None,
        figures: Vec::new(),
    };
    let file = pdf(&[sheet(), sheet()]);
    // Two slots, one of them held here: the first page works, the second
    // waits for the other slot the whole time.
    pool::global().set_by_hand(2);
    // Held by another caller: one caller alone holds one of two slots.
    let other = pool::as_caller("other", pool::global().acquire()).await;
    let each = batch::Each {
        deadline: Some(Duration::from_secs(2)),
        ..Default::default()
    };
    let started = Instant::now();
    let (_, answer) = batch::convert_each(
        vec![input(file)],
        &plan(Ocr::Auto, Pictures::Placeholder, Output::Text),
        &facts,
        Vec::new(),
        OutputBudget::request(),
        &each,
    )
    .await;
    let took = started.elapsed();
    drop(other);
    pool::global().set_by_hand(0);
    let d = &answer.documents[0];
    assert_eq!(
        d.error.as_ref().map(|e| e.code),
        Some(ErrorCode::DeadlineExceeded),
        "{d:?}"
    );
    // The held page sleeps 60 s: a clock stopped by the waiting page would
    // only have ended the file after it.
    assert!(took < Duration::from_secs(20), "{took:?}");
    all_gone(dir.path()).await;
}
