//! An office file's pictures (design §5.3): the markers its reader wrote
//! where the document references a picture (`extract::office_media`), made
//! into the file's [`Found`] list, its picture lines (markdown) or its
//! `figure` blocks (`json`).
//!
//! - **Order** is the document's: the marker sits where the reference was
//!   (after its paragraph or table).
//! - **Size:** a picture smaller than `picture_min_area` of the page the
//!   document declares (docx `w:pgSz`, pptx `p:sldSz`, ODF page layout),
//!   by the size the document gives it (`wp:extent`, `a:ext`,
//!   `svg:width`/`svg:height`), is left out; without both sizes it stays.
//! - **Caption:** the paragraph right after the picture, else the one
//!   before, when it reads as a caption; the paragraph before is a
//!   describer's context.
//! - **Bytes** (embedded and referenced only): the media entry as stored
//!   (`word/media/…`, `ppt/media/…`, `Pictures/…`), resolved through the
//!   part's relationships for docx/pptx, read against
//!   `MAX_UNCOMPRESSED_BYTES` for all of them together. Never decoded: the
//!   format and the pixel size come from the first bytes (`sniff.rs`). A
//!   linked picture (outside the file), a missing entry, a format a
//!   browser does not show (EMF, WMF), and SVG (a document that can carry
//!   scripts and outside links; making a picture of it is not built yet)
//!   keep their placeholder and are counted in a `pictures_partial` note.
//! - Spreadsheets' drawings (`xl/media`, ODS `Pictures/`) are not read yet:
//!   a workbook that has any gets a `not_yet` note.

use std::collections::HashMap;
use std::io::Cursor;

use bytes::Bytes;
use lmgw_api_types::documents::convert::WarningCode;

use super::super::doc::{BlockKind, Doc, Figure, Source};
use super::{caption, needs_bytes, sniff, warning, Found, Image, LeftOut, Picture, Settings};
use crate::extract::office::{read_entry_limited, OfficeText, MAX_UNCOMPRESSED_BYTES};
use crate::extract::office_media::{read_marker, OfficeMedia};
use crate::extract::OfficeFormat;

/// The paragraphs of a part's markdown (blank-line separated; a slide's
/// lines are its paragraphs).
fn paragraphs(body: &str) -> Vec<&str> {
    body.split('\n').filter(|l| !l.trim().is_empty()).collect()
}

/// Why an office picture has no bytes.
#[derive(Default)]
struct Missing {
    linked: u32,
    absent: u32,
    format: u32,
    svg: u32,
    budget: u32,
}

/// The file's pictures from its reader's markers and `media`; their bytes
/// read from `archive` when `s` asks for them. CPU-bound: runs in the
/// reader's slot.
pub fn collect(
    of: OfficeFormat,
    archive: &[u8],
    text: &OfficeText,
    media: OfficeMedia,
    s: &Settings,
) -> Found {
    let mut found = Found::default();
    let mut map: Vec<Option<u32>> = vec![None; media.media.len()];
    let mut left = LeftOut::default();
    let page_area = media
        .page
        .map(|(w, h)| w as f64 * h as f64)
        .filter(|a| *a > 0.0);
    for (pi, part) in text.parts.iter().enumerate() {
        let paras = paragraphs(&part.body);
        for (k, para) in paras.iter().enumerate() {
            let Some(id) = read_marker(para) else {
                continue;
            };
            let Some(m) = media.media.get(id as usize) else {
                continue;
            };
            let share = match (m.extent, page_area) {
                (Some((w, h)), Some(area)) => Some((w as f64 * h as f64) / area),
                _ => None,
            };
            if share.is_some_and(|a| a < s.min_area) {
                left.small += 1;
                continue;
            }
            let text_at = |i: usize| paras.get(i).filter(|p| read_marker(p).is_none());
            let next = text_at(k + 1).filter(|p| caption::is_caption(p));
            let prev = k
                .checked_sub(1)
                .and_then(text_at)
                .filter(|p| caption::is_caption(p));
            let cap = next.or(prev);
            let before = k
                .checked_sub(1)
                .and_then(text_at)
                .filter(|p| Some(*p) != cap);
            let number = u32::try_from(pi + 1).unwrap_or(u32::MAX);
            let mut p = Picture::new(number, None, Source::Native);
            p.share = share;
            p.caption = cap.map(|c| c.trim().to_string());
            p.before = before.map(|b| b.trim().to_string());
            p.entry = m.href.clone().or_else(|| m.rel.clone());
            map[id as usize] = u32::try_from(found.pictures.len()).ok();
            found.pictures.push(p);
        }
    }
    if needs_bytes(s) {
        read_bytes(of, archive, &media, &map, &mut found);
    }
    if let Some(w) = left.warning(s, Vec::new()) {
        found.warnings.push(w);
    }
    if of.is_spreadsheet() && has_drawings(archive) {
        let mut w = warning(
            WarningCode::NotYet,
            "this workbook has pictures; pictures in spreadsheets are not exported yet".to_string(),
            Vec::new(),
            None,
        );
        w.option = Some("pictures".to_string());
        found.warnings.push(w);
    }
    found.markers = map;
    found
}

/// Whether a workbook's archive holds media.
fn has_drawings(archive: &[u8]) -> bool {
    zip::ZipArchive::new(Cursor::new(archive)).is_ok_and(|z| {
        z.file_names()
            .any(|n| n.starts_with("xl/media/") || n.starts_with("Pictures/"))
    })
}

/// A relationship target resolved against its part's directory.
fn resolve(part: &str, target: &str) -> Option<String> {
    if let Some(abs) = target.strip_prefix('/') {
        return Some(abs.to_string());
    }
    let mut at: Vec<&str> = part.split('/').collect();
    at.pop();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                at.pop()?;
            }
            s => at.push(s),
        }
    }
    Some(at.join("/"))
}

/// The relationships of `part` (`<dir>/_rels/<name>.rels`): id → (target,
/// external).
fn rels<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    part: &str,
    left: &mut u64,
) -> HashMap<String, (String, bool)> {
    let (dir, name) = part.rsplit_once('/').unwrap_or(("", part));
    let path = if dir.is_empty() {
        format!("_rels/{name}.rels")
    } else {
        format!("{dir}/_rels/{name}.rels")
    };
    let mut out = HashMap::new();
    let Ok(Some(xml)) = read_entry_limited(zip, &path, *left) else {
        return out;
    };
    *left = left.saturating_sub(xml.len() as u64);
    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e))
                if e.local_name().as_ref() == b"Relationship" =>
            {
                let (mut id, mut target, mut ext) = (None, None, false);
                for a in e.attributes().flatten() {
                    let v = String::from_utf8_lossy(&a.value).into_owned();
                    match a.key.as_ref() {
                        b"Id" => id = Some(v),
                        b"Target" => target = Some(v),
                        b"TargetMode" => ext = v == "External",
                        _ => {}
                    }
                }
                if let (Some(id), Some(t)) = (id, target) {
                    out.insert(id, (t, ext));
                }
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return out,
            _ => {}
        }
    }
}

/// Each kept picture's bytes from the archive.
fn read_bytes(
    of: OfficeFormat,
    archive: &[u8],
    media: &OfficeMedia,
    map: &[Option<u32>],
    found: &mut Found,
) {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(archive)) else {
        return;
    };
    let mut left = MAX_UNCOMPRESSED_BYTES;
    let mut miss = Missing::default();
    let mut part_rels: HashMap<usize, HashMap<String, (String, bool)>> = HashMap::new();
    // Each entry read once, however often the document shows it.
    let mut read: HashMap<String, Option<Bytes>> = HashMap::new();
    for (id, m) in media.media.iter().enumerate() {
        let Some(index) = map.get(id).copied().flatten() else {
            continue;
        };
        let path = match (&m.href, &m.rel) {
            (Some(h), _) if h.contains("://") => None,
            (Some(h), _) => Some(h.trim_start_matches("./").to_string()),
            (None, Some(r)) => {
                let Some(part) = media.part_paths.get(m.part) else {
                    miss.absent += 1;
                    continue;
                };
                let rs = part_rels
                    .entry(m.part)
                    .or_insert_with(|| rels(&mut zip, part, &mut left));
                match rs.get(r) {
                    Some((_, true)) => None,
                    Some((t, false)) => resolve(part, t),
                    None => {
                        miss.absent += 1;
                        continue;
                    }
                }
            }
            (None, None) => continue,
        };
        let Some(path) = path else {
            miss.linked += 1;
            continue;
        };
        let bytes = match read.get(&path) {
            Some(b) => b.clone(),
            None => {
                let b = match read_entry_limited(&mut zip, &path, left) {
                    Ok(Some(b)) => {
                        left = left.saturating_sub(b.len() as u64);
                        Some(Bytes::from(b))
                    }
                    Ok(None) => None,
                    Err(_) => {
                        miss.budget += 1;
                        continue;
                    }
                };
                read.insert(path.clone(), b.clone());
                b
            }
        };
        let Some(bytes) = bytes else {
            miss.absent += 1;
            continue;
        };
        let Some(kind) = sniff::kind(&bytes) else {
            miss.format += 1;
            continue;
        };
        // SVG is a document of its own: it can carry scripts and links to
        // outside the file, so it is not passed on as it is.
        if kind.ext == "svg" {
            miss.svg += 1;
            continue;
        }
        let size = sniff::size(&bytes);
        found.pictures[index as usize].image = Some(Image {
            bytes,
            mime: kind.mime,
            ext: kind.ext,
            width_px: size.map(|s| s.0),
            height_px: size.map(|s| s.1),
        });
    }
    let mut said = Vec::new();
    if miss.linked > 0 {
        said.push(format!(
            "{} linked from outside the file (lmgw fetches nothing)",
            miss.linked
        ));
    }
    if miss.absent > 0 {
        said.push(format!(
            "{} whose media the file does not hold",
            miss.absent
        ));
    }
    if miss.format > 0 {
        said.push(format!(
            "{} in a format lmgw does not export yet (EMF, WMF or unknown)",
            miss.format
        ));
    }
    if miss.svg > 0 {
        said.push(format!(
            "{} in SVG, which can carry scripts and links to outside the file (exporting a \
             picture made of it is not built yet)",
            miss.svg
        ));
    }
    if miss.budget > 0 {
        said.push(format!(
            "{} past MAX_UNCOMPRESSED_BYTES ({}) of media read from this {} file",
            miss.budget,
            crate::hf::fmt_bytes(MAX_UNCOMPRESSED_BYTES),
            of.as_str()
        ));
    }
    if !said.is_empty() {
        found.warnings.push(warning(
            WarningCode::PicturesPartial,
            format!(
                "pictures kept as placeholders without their picture: {}",
                said.join("; ")
            ),
            Vec::new(),
            None,
        ));
    }
}

/// What writes picture `index`'s line.
pub type LineFn<'a> = &'a dyn Fn(u32, &mut String);

/// What is written after picture `index` and its caption, when anything is
/// (its description, `super::super::describe::place`).
pub type AfterFn<'a> = &'a dyn Fn(u32) -> Option<String>;

/// The line after which picture `index`'s `after` text goes, for the marker
/// at `k` of `lines`: the end of the caption paragraph right after it, when
/// that paragraph is the picture's caption; else the marker's own line.
fn after_line(lines: &[&str], k: usize, caption: Option<&str>) -> usize {
    let Some(caption) = caption else {
        return k;
    };
    let Some(start) = (k + 1..lines.len()).find(|&j| !lines[j].trim().is_empty()) else {
        return k;
    };
    let end = (start..lines.len())
        .find(|&j| lines[j].trim().is_empty())
        .unwrap_or(lines.len());
    let para = &lines[start..end];
    if para.iter().any(|l| read_marker(l).is_some()) {
        return k;
    }
    if super::super::describe::is_caption_of(&para.join("\n"), caption) {
        end - 1
    } else {
        k
    }
}

/// `md` (a reader's markdown with markers) with each kept picture's line
/// written by `line` and the markers of pictures left out dropped with the
/// blank line beside them; `line` `None` drops every marker (plain text).
/// What `after` gives for a picture is written after it and its caption,
/// a blank line before it (in place of the marker when the picture writes
/// no line).
pub fn lines(
    md: &str,
    found: &Found,
    line: Option<LineFn<'_>>,
    after: Option<AfterFn<'_>>,
) -> String {
    let all: Vec<&str> = md.split('\n').collect();
    let mut out: Vec<std::borrow::Cow<'_, str>> = Vec::new();
    let mut skip_blank = false;
    // Text written after a line: (the line, the text).
    let mut pending: Vec<(usize, String)> = Vec::new();
    for (k, l) in all.iter().enumerate() {
        let index = read_marker(l).map(|id| found.markers.get(id as usize).copied().flatten());
        // A kept picture's line; one whose line is empty (text: a picture
        // without a description) is dropped like one left out.
        let marker = index.map(|i| i.and_then(|i| line.and_then(|write| written(write, i))));
        let more = index.flatten().and_then(|i| {
            let text = after.and_then(|a| a(i))?;
            let caption = found
                .pictures
                .get(i as usize)
                .and_then(|p| p.caption.as_deref());
            Some((after_line(&all, k, caption), text))
        });
        match (marker, more) {
            // In place of a picture that writes no line.
            (Some(None), Some((at, text))) if at == k => {
                out.push(text.into());
                skip_blank = false;
            }
            (Some(Some(s)), more) => {
                out.push(s.into());
                skip_blank = false;
                if let Some(m) = more {
                    pending.push(m);
                }
            }
            // Dropped, with the blank line before it, or else the one after.
            (Some(None), more) => {
                if out.last().is_some_and(|x| x.trim().is_empty()) {
                    out.pop();
                } else {
                    skip_blank = true;
                }
                if let Some(m) = more {
                    pending.push(m);
                }
            }
            (None, _) => {
                if std::mem::take(&mut skip_blank) && l.trim().is_empty() {
                    continue;
                }
                out.push((*l).into());
            }
        }
        // What goes after this line.
        while let Some(at) = pending.iter().position(|(j, _)| *j == k) {
            let (_, text) = pending.remove(at);
            out.push("".into());
            out.push(text.into());
        }
    }
    out.join("\n")
}

/// What `write` writes for picture `index`, when it writes anything.
fn written(write: LineFn<'_>, index: u32) -> Option<String> {
    let mut s = String::new();
    write(index, &mut s);
    (!s.is_empty()).then_some(s)
}

/// `doc`'s marker paragraphs (an office file read for `json`) made `figure`
/// blocks of the kept pictures, the others dropped.
pub fn figures(doc: &mut Doc, found: &Found) {
    for part in &mut doc.parts {
        part.blocks.retain_mut(|b| {
            if b.kind != BlockKind::Paragraph {
                return true;
            }
            let Some(id) = read_marker(&b.text) else {
                return true;
            };
            let Some(index) = found.markers.get(id as usize).copied().flatten() else {
                return false;
            };
            let p = &found.pictures[index as usize];
            b.kind = BlockKind::Figure;
            b.text = String::new();
            b.figure = Some(Box::new(Figure {
                index,
                caption: p.caption.clone(),
                description: p.description.clone(),
            }));
            true
        });
    }
}

#[cfg(test)]
mod tests;
