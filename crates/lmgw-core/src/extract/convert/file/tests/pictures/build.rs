//! PDFs with raster images for the picture tests: A4 pages (or rotated
//! ones), each image an uncompressed RGB XObject drawn where the test says,
//! mirrored when asked.

/// One image drawn on a page: where (points from the top-left, as the page
/// is drawn before its `/Rotate`), its pixels (`w` × `h`, one RGB colour),
/// and whether its matrix mirrors it.
pub(in super::super) struct Drawn {
    pub x: f32,
    pub y: f32,
    pub w_pt: f32,
    pub h_pt: f32,
    pub px: (u32, u32),
    pub rgb: [u8; 3],
    pub flip_x: bool,
    pub flip_y: bool,
}

impl Drawn {
    pub fn at(x: f32, y: f32, w_pt: f32, h_pt: f32, px: (u32, u32), rgb: [u8; 3]) -> Self {
        Self {
            x,
            y,
            w_pt,
            h_pt,
            px,
            rgb,
            flip_x: false,
            flip_y: false,
        }
    }
}

/// A page: its lines of text (x, y from the top, size, text), its images,
/// its `/Rotate`.
pub(in super::super) struct Page<'a> {
    pub lines: Vec<(f32, f32, f32, &'a str)>,
    pub images: Vec<Drawn>,
    pub rotate: i32,
}

impl<'a> Page<'a> {
    pub fn new(lines: Vec<(f32, f32, f32, &'a str)>, images: Vec<Drawn>) -> Self {
        Self {
            lines,
            images,
            rotate: 0,
        }
    }
}

/// A PDF of A4 pages.
pub(in super::super) fn pdf(pages: &[Page<'_>]) -> Vec<u8> {
    let (width, height) = (595.0f32, 842.0f32);
    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        Vec::new(), // the page tree, filled in below
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut kids = Vec::new();
    for page in pages {
        let mut xobjs = String::new();
        let mut content = String::new();
        for (x, y, size, t) in &page.lines {
            content.push_str(&format!(
                "BT /F1 {size} Tf {x} {} Td ({t}) Tj ET\n",
                height - y - size
            ));
        }
        for (i, d) in page.images.iter().enumerate() {
            let pixels: Vec<u8> = (0..d.px.0 * d.px.1).flat_map(|_| d.rgb).collect();
            let mut o = format!(
                "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceRGB \
                 /BitsPerComponent 8 /Length {} >>\nstream\n",
                d.px.0,
                d.px.1,
                pixels.len()
            )
            .into_bytes();
            o.extend_from_slice(&pixels);
            o.extend_from_slice(b"\nendstream");
            objs.push(o);
            let id = objs.len();
            xobjs.push_str(&format!("/Im{i} {id} 0 R "));
            // A mirrored image starts at the far side and runs back.
            let (a, e) = if d.flip_x {
                (-d.w_pt, d.x + d.w_pt)
            } else {
                (d.w_pt, d.x)
            };
            let bottom = height - d.y - d.h_pt;
            let (dd, f) = if d.flip_y {
                (-d.h_pt, bottom + d.h_pt)
            } else {
                (d.h_pt, bottom)
            };
            content.push_str(&format!("q {a} 0 0 {dd} {e} {f} cm /Im{i} Do Q\n"));
        }
        objs.push(
            format!(
                "<< /Length {} >>\nstream\n{content}endstream",
                content.len()
            )
            .into_bytes(),
        );
        let contents = objs.len();
        objs.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /Rotate {} \
                 /Contents {contents} 0 R /Resources << /Font << /F1 3 0 R >> /XObject << \
                 {xobjs}>> >> >>",
                page.rotate
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
        out.extend(format!("{} 0 obj\n", i + 1).into_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).into_bytes());
    for o in offsets {
        out.extend(format!("{o:010} 00000 n \n").into_bytes());
    }
    out.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .into_bytes(),
    );
    out
}
