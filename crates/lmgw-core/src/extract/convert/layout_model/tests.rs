//! The layout model without its file: the answer's decoding and the
//! render's unpacking against fixed inputs, the file's checks on small
//! stand-in files, and the notes a conversion gives. One test runs the
//! real model, and only when `LMGW_LAYOUT_MODEL` names its file:
//!
//! ```sh
//! LMGW_LAYOUT_MODEL=/path/to/model.onnx scripts/build-slot.sh cargo nextest run \
//!   --workspace --run-ignored ignored-only the_real_model
//! ```
//!
//! With `LMGW_LAYOUT_PDF` naming a PDF as well it prints that file's
//! capture (its lines and the model's regions), which is how
//! `pdf_layout/tests/example_paper.txt` was recorded.

use std::path::Path;

use lmgw_api_types::documents::models::DocModelState;

use bytes::Bytes;

use super::super::caps::ToolId;
#[cfg(unix)]
use super::super::pool;
#[cfg(unix)]
use super::super::tools::Fact;
use super::super::tools::ToolFacts;
use super::files::{locate, read_checked, Located, ModelFile};
use super::*;
#[cfg(unix)]
use crate::extract::pdf;

/// What the pinned export answered for three queries of a page, as its
/// outputs hold them: class ids, corners scaled to 640, scores.
const LABELS: [i64; 6] = [9, 7, 8, 9, 99, 6];
const SCORES: [f32; 6] = [0.98, 0.94, 0.95, 0.31, 0.9, f32::NAN];
#[rustfmt::skip]
const BOXES: [f32; 24] = [
    60.2, 112.6, 307.2, 163.8,
    60.8, 98.6, 113.3, 106.2,
    332.2, 99.8, 579.2, 156.2,
    60.8, 400.0, 307.2, 420.0,
    10.0, 10.0, 20.0, 20.0,
    10.0, 10.0, 20.0, 20.0,
];

#[test]
fn the_answer_is_decoded_as_docling_filters_it() {
    let got = decode(&LABELS, &BOXES, &SCORES, 640.0);
    // The text at 0.31 is under its label's 0.5, class 99 is no class, and
    // a score that is no number is no region.
    let labels: Vec<Label> = got.iter().map(|r| r.label).collect();
    assert_eq!(
        labels,
        vec![Label::Text, Label::SectionHeader, Label::Table]
    );
    let r = got[0].rect;
    let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
    assert!(close(r.x0, 0.09406) && close(r.y0, 0.17594), "{r:?}");
    assert!(close(r.x1, 0.48) && close(r.y1, 0.25594), "{r:?}");
    assert!(close(got[0].score, 0.98));
    // In points on an A4 page.
    let pt = in_points(&r, 595.0, 842.0);
    assert!(
        (pt.x0 - 55.967).abs() < 0.01 && (pt.y1 - 215.499).abs() < 0.01,
        "{pt:?}"
    );
    // Outputs of different lengths: as many as all three hold.
    assert_eq!(decode(&LABELS[..2], &BOXES, &SCORES, 640.0).len(), 2);
    assert_eq!(decode(&LABELS, &BOXES[..7], &SCORES, 640.0).len(), 1);
    assert!(decode(&LABELS, &BOXES, &SCORES, 0.0).is_empty());
    assert!(decode(&LABELS, &BOXES, &SCORES, f32::NAN).is_empty());
    // The class table is the model's (`config.json`), in its order.
    assert_eq!(Label::of(0), Some(Label::Caption));
    assert_eq!(Label::of(16), Some(Label::KeyValueRegion));
    assert_eq!(Label::of(17), None);
    assert_eq!(Label::of(-1), None);
    for (i, l) in Label::ALL.iter().enumerate() {
        assert_eq!(Label::of(i as i64), Some(*l));
        assert_eq!(Label::named(l.as_str()), Some(*l));
    }
    assert_eq!(Label::SectionHeader.threshold(), 0.45);
    assert_eq!(Label::Text.threshold(), 0.5);
}

/// A `SIDE`-square PPM whose pixel `i` is `(i, i + 1, i + 2)` modulo 251.
fn ppm(header: &str) -> Vec<u8> {
    let n = (SIDE * SIDE) as usize;
    let mut out = header.as_bytes().to_vec();
    for i in 0..n {
        out.extend([
            (i % 251) as u8,
            ((i + 1) % 251) as u8,
            ((i + 2) % 251) as u8,
        ]);
    }
    out
}

#[test]
fn a_render_is_unpacked_into_the_three_planes_and_checked() {
    let n = (SIDE * SIDE) as usize;
    let planes = prep::input(&ppm("P6\n640 640\n255\n")).unwrap();
    assert_eq!(planes.len(), 3 * n);
    for i in [0usize, 1, 639, 640, 250, 251, n - 1] {
        assert_eq!(planes[i], (i % 251) as u8, "red {i}");
        assert_eq!(planes[n + i], ((i + 1) % 251) as u8, "green {i}");
        assert_eq!(planes[2 * n + i], ((i + 2) % 251) as u8, "blue {i}");
    }
    // A comment in the header, as the format allows.
    assert!(prep::input(&ppm("P6\n# made by example\n640 640\n255\n")).is_ok());
    for (bad, why) in [
        (ppm("P5\n640 640\n255\n"), "not a binary PPM"),
        (ppm("P6\n640 480\n255\n"), "640 x 480"),
        (ppm("P6\n640 640\n65535\n"), "65535"),
        (b"P6\n640".to_vec(), "cut short"),
        (Vec::new(), "no header"),
        (b"P6\n640 640\n255\nxx".to_vec(), "cut short or run on"),
        (
            b"P6\n99999999999999999999999 640\n255\n".to_vec(),
            "cut short",
        ),
    ] {
        let e = prep::input(&bad).unwrap_err();
        assert!(e.contains(why), "{why}: {e}");
    }
    let mut long = ppm("P6\n640 640\n255\n");
    long.push(0);
    assert!(prep::input(&long).is_err());
}

const STAND_IN: ModelFile = ModelFile {
    id: "example",
    label: "Example model",
    adds: "Nothing.",
    repo: "example-owner/example-model",
    revision: "0000000000000000000000000000000000000000",
    file: "model.onnx",
    // Of the five bytes `hello`.
    sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
    size_bytes: 5,
    licence: "CC0-1.0",
};

#[test]
fn a_file_is_the_model_by_its_size_and_its_hash() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().display().to_string();
    assert_eq!(locate(&STAND_IN, " "), Located::NoModelsDir);
    let path = dir.path().join("example-owner/example-model/model.onnx");
    assert_eq!(locate(&STAND_IN, &root), Located::Missing(path.clone()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // A download that did not finish.
    std::fs::write(&path, b"hel").unwrap();
    assert!(
        matches!(locate(&STAND_IN, &root), Located::Wrong(_, why) if why.contains("3 bytes")),
        "{:?}",
        locate(&STAND_IN, &root)
    );
    // Another file of the model's size: found by its size, refused by its
    // hash, and remembered as refused.
    std::fs::write(&path, b"world").unwrap();
    assert_eq!(locate(&STAND_IN, &root), Located::Found(path.clone()));
    let e = read_checked(&STAND_IN, &path).unwrap_err();
    assert!(
        e.contains("486ea46224d1bb4fb680f34f7c9ad96a8f24ec88be73ea8e5a6c65260e9cb8a7"),
        "{e}"
    );
    assert!(matches!(locate(&STAND_IN, &root), Located::Wrong(..)));
    // The model's bytes, written anew: checked again at their load.
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"hello").unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert_eq!(read_checked(&STAND_IN, &path), Ok(b"hello".to_vec()));
    assert_eq!(locate(&STAND_IN, &root), Located::Found(path.clone()));
    // A directory at its place.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(
        matches!(locate(&STAND_IN, &root), Located::Wrong(_, why) if why.contains("not a file"))
    );
    assert!(read_checked(&STAND_IN, Path::new("/nonexistent/example")).is_err());
    assert_eq!(
        STAND_IN.dest_rel_path(),
        "example-owner/example-model/model.onnx"
    );
}

#[test]
fn the_pinned_file_is_named_whole() {
    let m = files::HERON;
    assert_eq!(m.sha256.len(), 64);
    assert_eq!(m.revision.len(), 40);
    assert!(m
        .sha256
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    assert!(crate::hf::validate_repo(m.repo).is_ok());
    assert!(crate::hf::validate_revision(m.revision).is_ok());
    assert_eq!(
        crate::hf::dest_rel_path(m.repo, m.file).unwrap(),
        m.dest_rel_path()
    );
    assert_eq!(crate::hf::fmt_bytes(m.size_bytes), "163 MiB");
    assert!(m
        .url()
        .starts_with("https://huggingface.co/docling-project/"));
    // Switching it on is refused while it is not there, by what to do.
    let dir = tempfile::tempdir().unwrap();
    let e = status::can_switch_on(&dir.path().display().to_string()).unwrap_err();
    assert!(
        e.contains("doc_model_add id=layout") && e.contains("163 MiB"),
        "{e}"
    );
    assert!(status::can_switch_on("").unwrap_err().contains("no folder"));
    assert!(files::usable("").unwrap_err().contains("no folder"));
    assert!(files::usable(&dir.path().display().to_string())
        .unwrap_err()
        .contains("download it again"));
}

#[test]
fn the_page_s_row_follows_the_file_and_its_download() {
    let path = std::path::PathBuf::from("/models.example/example-owner/example-model/model.onnx");
    let download = |status: &str, error: Option<&str>| crate::store::HfModelRow {
        id: 7,
        repo: STAND_IN.repo.into(),
        file: STAND_IN.file.into(),
        dest_path: STAND_IN.dest_rel_path(),
        target: "aux".into(),
        etag: None,
        size_bytes: None,
        status: status.into(),
        error: error.map(String::from),
        downloaded_at: None,
        requested_revision: Some(STAND_IN.revision.into()),
        resolved_commit: None,
        oid: None,
    };
    let r = status::row(&STAND_IN, Located::Missing(path.clone()), None, false);
    assert_eq!(
        (r.state, r.chosen, r.in_use),
        (DocModelState::Missing, false, false)
    );
    assert_eq!(r.path.as_deref(), path.to_str());
    assert_eq!((r.size_bytes, r.licence.as_str()), (5, "CC0-1.0"));
    assert_eq!(r.threads, LAYOUT_THREADS as u32);
    assert_eq!(r.idle_unload_secs, LAYOUT_IDLE_UNLOAD.as_secs());
    let d = download("downloading", None);
    let r = status::row(&STAND_IN, Located::Missing(path.clone()), Some(&d), false);
    assert_eq!(
        (r.state, r.download_id),
        (DocModelState::Downloading, Some(7))
    );
    // The part file beside it is not the model yet.
    let r = status::row(
        &STAND_IN,
        Located::Wrong(path.clone(), "3 bytes".into()),
        Some(&d),
        true,
    );
    assert_eq!((r.state, r.in_use), (DocModelState::Downloading, false));
    let d = download("failed", Some("example failure"));
    let r = status::row(&STAND_IN, Located::Missing(path.clone()), Some(&d), false);
    assert_eq!(r.state, DocModelState::Missing);
    assert!(r.message.unwrap().contains("example failure"));
    let r = status::row(
        &STAND_IN,
        Located::Wrong(path.clone(), "another file".into()),
        None,
        true,
    );
    assert_eq!((r.state, r.in_use), (DocModelState::WrongFile, false));
    let r = status::row(&STAND_IN, Located::Found(path.clone()), None, true);
    assert_eq!(
        (r.state, r.chosen, r.in_use),
        (DocModelState::Found, true, true)
    );
    let r = status::row(&STAND_IN, Located::Found(path), None, false);
    assert_eq!((r.state, r.in_use), (DocModelState::Found, false));
    let r = status::row(&STAND_IN, Located::NoModelsDir, None, false);
    assert_eq!(r.state, DocModelState::NoFolder);
    assert!(r.path.is_none() && r.message.is_some());
}

#[test]
fn the_notes_name_what_the_model_did_not_do_and_what_it_left_out() {
    let mut regions = Regions::default();
    regions.pages.insert(1, PageRegions::Found(Vec::new()));
    regions
        .pages
        .insert(2, PageRegions::Failed("example reason".into()));
    regions
        .pages
        .insert(3, PageRegions::Failed("another".into()));
    regions
        .pages
        .insert(9, PageRegions::Failed("not picked".into()));
    let mut used = Used {
        implausible: vec![
            (4, Implausible::PictureText),
            (5, Implausible::Orphans),
            (9, Implausible::NoRegions),
        ],
        ..Default::default()
    };
    let furniture = |n: u32| PageUse {
        furniture: vec![
            (true, "Example Report".to_string()),
            (false, format!("Page {n} of 12")),
        ],
        picture_lines: if n == 1 { 3 } else { 0 },
    };
    used.pages.insert(1, furniture(1));
    used.pages.insert(5, furniture(5));
    used.pages.insert(9, furniture(9));
    let picked = [1, 2, 3, 4, 5];
    let notes = notes(&regions, &used, &picked);
    let codes: Vec<WarningCode> = notes.iter().map(|n| n.code).collect();
    assert_eq!(
        codes,
        vec![
            WarningCode::PartFailed,
            WarningCode::PartFailed,
            WarningCode::PartFailed,
            WarningCode::PartFailed,
            WarningCode::HeaderRemoved,
            WarningCode::PictureText
        ]
    );
    assert!(notes.iter().all(|n| n.tool.as_deref() == Some(TOOL)));
    // Each reason its own note, with its own pages.
    assert_eq!(notes[0].parts, vec![2]);
    assert!(
        notes[0].message.contains("page(s) 2 (example reason)"),
        "{}",
        notes[0].message
    );
    assert_eq!(notes[1].parts, vec![3]);
    assert!(
        notes[1].message.contains("(another)"),
        "{}",
        notes[1].message
    );
    assert_eq!(notes[2].parts, vec![5]);
    assert!(
        notes[2].message.contains("ORPHAN_MAX_SHARE (67 %)"),
        "{}",
        notes[2].message
    );
    assert_eq!(notes[3].parts, vec![4]);
    assert!(
        notes[3].message.contains("PICTURE_TEXT_MAX_SHARE (50 %)"),
        "{}",
        notes[3].message
    );
    let notes = &notes[2..];
    // Each removed text once: the page number as #, on both picked pages.
    assert!(
        notes[2].message.contains("2 line(s)")
            && notes[2]
                .message
                .contains("\"Example Report\" (top, 2 pages)")
            && notes[2]
                .message
                .contains("\"Page # of #\" (bottom, 2 pages)"),
        "{}",
        notes[2].message
    );
    assert_eq!(notes[3].parts, vec![1]);
    assert!(
        notes[3].message.starts_with("3 line(s)"),
        "{}",
        notes[3].message
    );
    // A model that could not be used at all.
    let gone = Regions {
        unusable: Some("its file is not there any more".into()),
        ..Default::default()
    };
    let n = super::notes(&gone, &Used::default(), &picked);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].code, WarningCode::ToolMissing);
    assert!(
        n[0].message.contains("its file is not there any more"),
        "{}",
        n[0].message
    );
    // Nothing to say when all went well.
    let mut fine = Regions::default();
    fine.pages.insert(1, PageRegions::Found(Vec::new()));
    let mut all = Used::default();
    all.pages.insert(1, PageUse::default());
    assert!(super::notes(&fine, &all, &picked).is_empty());
}

#[test]
fn off_is_no_stage_at_all() {
    // The choice is the process's: this test only reads the default.
    if choice().on {
        return;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let facts = ToolFacts::all_missing().with_found(ToolId::Poppler);
    let got = rt.block_on(async {
        let src =
            crate::extract::convert::ocr::spool::Spool::of(None, Bytes::from_static(b"%PDF-1.4"))
                .await;
        stage(&src, None, &facts).await
    });
    assert_eq!(got, None);
}

/// The real model (module doc).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the model file: set LMGW_LAYOUT_MODEL to it"]
async fn the_real_model_reads_a_page_and_the_pass_uses_it() {
    use super::super::tools::DocTools;
    use crate::extract::pdf::geometry;
    let model = std::env::var_os("LMGW_LAYOUT_MODEL").expect("LMGW_LAYOUT_MODEL names the file");
    // The file where the gateway looks for it: a models directory with a
    // link at the pinned place.
    let dir = tempfile::tempdir().unwrap();
    let place = dir.path().join(files::HERON.dest_rel_path());
    std::fs::create_dir_all(place.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&model, &place).unwrap();
    configure(true, &dir.path().display().to_string());
    let facts = DocTools::on_path(std::env::var_os("PATH").unwrap_or_default())
        .check()
        .await;
    let given = std::env::var_os("LMGW_LAYOUT_PDF");
    let pdf = match &given {
        Some(p) => Bytes::from(std::fs::read(p).unwrap()),
        None => Bytes::from(crate::extract::test_files::pdf(&[
            Some("An example line of text for the layout model to find."),
            None,
        ])),
    };
    let started = std::time::Instant::now();
    let src = crate::extract::convert::ocr::spool::Spool::of(None, pdf.clone()).await;
    let regions = stage(&src, None, &facts)
        .await
        .expect("the model is chosen");
    assert_eq!(regions.unusable, None);
    eprintln!(
        "{} page(s) in {:?}, session {:?}",
        regions.pages.len(),
        started.elapsed(),
        session::now()
    );
    assert!(matches!(session::now(), session::Now::Loaded { .. }));
    let Some(Fact::Found(found)) = facts.fact(ToolId::Poppler) else {
        panic!("poppler is needed");
    };
    let slot = pool::global().acquire().await;
    let bbox = geometry::bbox_layout_with(&slot, &found.located.path, &src, None, pdf::PDF_TIMEOUT)
        .await
        .unwrap();
    drop(slot);
    let lay = super::super::pdf_layout::layout_with(&bbox, None, 1, Some(&regions)).unwrap();
    if given.is_some() {
        // The capture: each page's regions (its lines are poppler's).
        for (n, page) in &regions.pages {
            println!("page {n}");
            if let PageRegions::Found(list) = page {
                for r in list {
                    println!(
                        "@ {} {:.2} {:.3} {:.3} {:.3} {:.3}",
                        r.label.as_str(),
                        r.score,
                        r.rect.x0,
                        r.rect.y0,
                        r.rect.x1,
                        r.rect.y1
                    );
                }
            }
        }
        return;
    }
    // The blank page has no words: it was not rendered.
    assert_eq!(regions.pages.keys().copied().collect::<Vec<_>>(), vec![1]);
    let found = regions.page(1).expect("page 1 was read");
    assert!(
        found
            .iter()
            .any(|r| r.label == Label::Text || r.label == Label::SectionHeader),
        "{found:?}"
    );
    assert_eq!(lay.model.pages.keys().copied().collect::<Vec<_>>(), vec![1]);
    assert!(lay.model.implausible.is_empty());
}
