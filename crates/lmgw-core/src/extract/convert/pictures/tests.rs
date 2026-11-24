use lmgw_api_types::documents::convert::{Output, Pictures};

use super::*;

fn png_picture() -> Picture {
    let mut p = Picture::new(
        2,
        Some(Rect {
            x0: 10.04,
            y0: 20.0,
            x1: 110.0,
            y1: 70.0,
        }),
        Source::TextLayer,
    );
    p.caption = Some("Figure 1: Example".into());
    p.image = Some(Image {
        bytes: Bytes::from_static(b"\x89PNG example"),
        mime: "image/png",
        ext: "png",
        width_px: Some(200),
        height_px: Some(100),
    });
    p
}

#[test]
fn artifact_names_are_docling_s_with_the_bytes_hash_and_nothing_else_passes() {
    let p = png_picture();
    let name = artifact(0, p.image.as_ref().unwrap());
    assert!(name.starts_with("artifacts/image_000000_"), "{name}");
    assert!(name.ends_with(".png"));
    assert!(is_artifact(&name));
    for bad in [
        "artifacts/../entry.json",
        "artifacts/image_000000_abc.png",
        "/etc/passwd",
        "artifacts/image_00000x_0000000000000000000000000000000000000000000000000000000000000000.png",
        "artifacts/image_000000_0000000000000000000000000000000000000000000000000000000000000000./..",
        "",
    ] {
        assert!(!is_artifact(bad), "{bad}");
    }
    assert_eq!(id(0), "p1");
}

#[test]
fn each_mode_writes_docling_s_line_and_a_picture_without_bytes_is_a_placeholder() {
    let p = png_picture();
    let mut out = String::new();
    line(Pictures::Placeholder, 0, Some(&p), &mut out);
    assert_eq!(out, "<!-- image -->");
    out.clear();
    line(Pictures::Embedded, 0, Some(&p), &mut out);
    assert!(
        out.starts_with("![Image](data:image/png;base64,iVBORy"),
        "{out}"
    );
    out.clear();
    line(Pictures::Referenced, 3, Some(&p), &mut out);
    assert!(out.starts_with("![Image](artifacts/image_000003_"), "{out}");
    out.clear();
    let mut bare = p;
    bare.image = None;
    line(Pictures::Referenced, 0, Some(&bare), &mut out);
    assert_eq!(out, "<!-- image -->");
}

#[test]
fn the_list_carries_bytes_only_where_the_text_does_not() {
    let found = Found {
        pictures: vec![png_picture()],
        ..Default::default()
    };
    let s = |mode| Settings {
        mode,
        ..Default::default()
    };
    let l = wire(&found, &s(Pictures::Placeholder), Output::Markdown);
    assert_eq!(l[0].id, "p1");
    assert_eq!(l[0].bbox, Some([10.0, 20.0, 110.0, 70.0]));
    assert_eq!(l[0].caption.as_deref(), Some("Figure 1: Example"));
    assert!(l[0].data_base64.is_none() && l[0].file.is_none());
    let l = wire(&found, &s(Pictures::Embedded), Output::Markdown);
    assert!(l[0].data_base64.is_none(), "the markdown holds it");
    let l = wire(&found, &s(Pictures::Embedded), Output::Json);
    assert!(l[0].data_base64.is_some());
    let l = wire(&found, &s(Pictures::Referenced), Output::Markdown);
    assert!(is_artifact(l[0].file.as_deref().unwrap()));
    assert_eq!(l[0].bytes, Some(12));
    assert!(wire_size(&l) > l[0].data_base64.as_ref().unwrap().len());
}

#[test]
fn left_out_pictures_are_counted_by_kind() {
    let s = Settings::default();
    assert!(LeftOut::default().warning(&s, Vec::new()).is_none());
    let w = LeftOut {
        small: 2,
        repeated: 3,
        cover: 1,
    }
    .warning(&s, vec![1, 2])
    .unwrap();
    assert_eq!(w.code, WarningCode::PicturesLeftOut);
    assert!(
        w.message.contains("2 smaller than picture_min_area (0.05"),
        "{}",
        w.message
    );
    assert!(
        w.message.contains("3 the same picture in the same place"),
        "{}",
        w.message
    );
    assert!(w.message.contains("1 covering"), "{}", w.message);
}
