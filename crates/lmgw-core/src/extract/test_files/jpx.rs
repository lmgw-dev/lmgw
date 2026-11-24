//! JPEG 2000 test pages (review 2 of the poppler limit): an image whose
//! decode needs a known amount of memory, whole or damaged.

/// A one-page PDF drawing one `side` × `side` JPEG 2000 image made by
/// `opj_compress` (openjpeg's tools), with only the first `share` of the
/// image's bytes (below 1: a damaged image); `None` where `opj_compress`
/// is not installed.
pub fn page(side: u32, share: f64) -> Option<Vec<u8>> {
    let dir = tempfile::tempdir().unwrap();
    let ppm = dir.path().join("in.ppm");
    let mut raw = format!("P6\n{side} {side}\n255\n").into_bytes();
    raw.extend(std::iter::repeat_n([200u8, 220, 240], (side * side) as usize).flatten());
    std::fs::write(&ppm, raw).unwrap();
    let jp2 = dir.path().join("out.jp2");
    let made = std::process::Command::new("opj_compress")
        .arg("-i")
        .arg(&ppm)
        .arg("-o")
        .arg(&jp2)
        .stdout(std::process::Stdio::null())
        .status();
    if !made.is_ok_and(|s| s.success()) {
        return None;
    }
    let mut jpx = std::fs::read(jp2).unwrap();
    jpx.truncate((jpx.len() as f64 * share) as usize);
    let content = b"q 595 0 0 842 0 0 cm /Im0 Do Q";
    let mut image = format!(
        "<< /Type /XObject /Subtype /Image /Width {side} /Height {side} /Filter /JPXDecode \
         /Length {} >>\nstream\n",
        jpx.len()
    )
    .into_bytes();
    image.extend_from_slice(&jpx);
    image.extend_from_slice(b"\nendstream");
    let mut page = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    page.extend_from_slice(content);
    page.extend_from_slice(b"\nendstream");
    Some(pdf_of(&[
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] \
          /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>",
        &image,
        &page,
    ]))
}

/// A PDF of `objects`, numbered from 1, with its cross-reference table.
fn pdf_of(objects: &[&[u8]]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for body in objects {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", offsets.len()).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
    );
    for o in &offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    out
}
