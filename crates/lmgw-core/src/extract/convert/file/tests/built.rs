//! Files built in code for the tests: BIFF8 workbooks, OLE2 compound files
//! laid out by hand (the `cfb` crate writes only well-formed ones), TIFFs.
//! Small generators, kept readable: they are the regression suite for the
//! hostile files of the WP4 review.

// The tests that use most of them run the reader child, on Unix.
#![cfg_attr(not(unix), allow(dead_code))]

use std::io::Write as _;

/// One BIFF record.
pub fn rec(typ: u16, data: &[u8]) -> Vec<u8> {
    let mut v = typ.to_le_bytes().to_vec();
    v.extend_from_slice(&(data.len() as u16).to_le_bytes());
    v.extend_from_slice(data);
    v
}

fn bof(substream: u16) -> Vec<u8> {
    let mut d = 0x0600u16.to_le_bytes().to_vec();
    d.extend_from_slice(&substream.to_le_bytes());
    d.extend_from_slice(&[0; 12]);
    rec(0x0809, &d)
}

/// A `Number` cell.
pub fn number(row: u16, col: u16, v: f64) -> Vec<u8> {
    let mut d = row.to_le_bytes().to_vec();
    d.extend_from_slice(&col.to_le_bytes());
    d.extend_from_slice(&0u16.to_le_bytes());
    d.extend_from_slice(&v.to_le_bytes());
    rec(0x0203, &d)
}

/// A BIFF8 Workbook stream: `globals` (records after the BOF, such as a
/// shared-string table) and one sheet `name` holding `cells` (records).
pub fn workbook(globals: &[u8], name: &str, cells: &[u8]) -> Vec<u8> {
    let name = name.as_bytes();
    let sheet_rec_len = 4 + 6 + 2 + name.len();
    let globals_len = (4 + 16) + sheet_rec_len + globals.len() + 4;
    let mut bs = (globals_len as u32).to_le_bytes().to_vec();
    bs.extend_from_slice(&[0, 0, name.len() as u8, 0]);
    bs.extend_from_slice(name);
    let mut s = bof(0x0005);
    s.extend(rec(0x0085, &bs));
    s.extend_from_slice(globals);
    s.extend(rec(0x000A, &[]));
    assert_eq!(s.len(), globals_len);
    s.extend(bof(0x0010));
    s.extend_from_slice(cells);
    s.extend(rec(0x000A, &[]));
    s
}

/// `stream` as the Workbook stream of a well-formed compound file. A small
/// stream stands beside it, as Excel's summary streams do: calamine takes a
/// file without a mini stream for one with an empty root.
pub fn xls_of(stream: &[u8]) -> Vec<u8> {
    let mut file = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
    file.create_stream("/Workbook")
        .unwrap()
        .write_all(stream)
        .unwrap();
    file.create_stream("/Note")
        .unwrap()
        .write_all(b"an example workbook")
        .unwrap();
    file.flush().unwrap();
    file.into_inner().into_inner()
}

const SECTOR: usize = 512;
const END_OF_CHAIN: u32 = 0xFFFF_FFFE;
const FAT_SECTOR: u32 = 0xFFFF_FFFD;
const FREE: u32 = 0xFFFF_FFFF;
const NO_STREAM: u32 = 0xFFFF_FFFF;

/// One 128-byte directory entry.
fn dir_entry(name: &str, kind: u8, child: u32, start: u32, size: u32) -> Vec<u8> {
    let mut e = vec![0u8; 128];
    let utf16: Vec<u16> = name.encode_utf16().collect();
    for (i, u) in utf16.iter().enumerate() {
        e[2 * i..2 * i + 2].copy_from_slice(&u.to_le_bytes());
    }
    e[64..66].copy_from_slice(&((utf16.len() as u16 + 1) * 2).to_le_bytes());
    e[66] = kind;
    e[67] = 1; // black
    e[68..72].copy_from_slice(&NO_STREAM.to_le_bytes()); // left sibling
    e[72..76].copy_from_slice(&NO_STREAM.to_le_bytes()); // right sibling
    e[76..80].copy_from_slice(&child.to_le_bytes());
    e[116..120].copy_from_slice(&start.to_le_bytes());
    e[120..124].copy_from_slice(&size.to_le_bytes());
    e
}

/// A version-3 compound file laid out by hand: the header (the directory
/// starts at sector `dir_start`, the one FAT sector is sector `fat_at`),
/// then `sectors` as they are, each 512 bytes.
pub fn raw_cfb(dir_start: u32, fat_at: u32, sectors: &[Vec<u8>]) -> Vec<u8> {
    let mut h = vec![0u8; SECTOR];
    h[..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
    h[24..26].copy_from_slice(&0x003Eu16.to_le_bytes());
    h[26..28].copy_from_slice(&3u16.to_le_bytes());
    h[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes());
    h[30..32].copy_from_slice(&9u16.to_le_bytes()); // 512-byte sectors
    h[32..34].copy_from_slice(&6u16.to_le_bytes()); // 64-byte mini sectors
    h[44..48].copy_from_slice(&1u32.to_le_bytes()); // one FAT sector
    h[48..52].copy_from_slice(&dir_start.to_le_bytes());
    h[56..60].copy_from_slice(&4096u32.to_le_bytes()); // mini stream cutoff
    h[60..64].copy_from_slice(&END_OF_CHAIN.to_le_bytes()); // no mini FAT
    h[68..72].copy_from_slice(&END_OF_CHAIN.to_le_bytes()); // no DIFAT sector
    for i in 0..109 {
        let v = if i == 0 { fat_at } else { FREE };
        h[76 + 4 * i..80 + 4 * i].copy_from_slice(&v.to_le_bytes());
    }
    let mut file = h;
    for s in sectors {
        assert_eq!(s.len(), SECTOR);
        file.extend_from_slice(s);
    }
    file
}

/// A FAT sector: entry `i` says what follows sector `i`; the rest is free.
fn fat_sector(fat: &[u32]) -> Vec<u8> {
    let mut s = vec![0xFFu8; SECTOR];
    for (i, v) in fat.iter().enumerate() {
        s[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
    }
    s
}

/// A directory sector: the root (its child is entry `root_child`), then
/// the streams `(name, first sector, size)`.
fn dir_sector(root_child: u32, entries: &[(&str, u32, u32)]) -> Vec<u8> {
    let mut dir = dir_entry("Root Entry", 5, root_child, END_OF_CHAIN, 0);
    for (name, start, size) in entries {
        dir.extend(dir_entry(name, 2, NO_STREAM, *start, *size));
    }
    dir.resize(SECTOR, 0);
    dir
}

/// The reviewed directory loop (6,144 bytes): sector 1 is the directory,
/// and the FAT says it is followed by sector 11, one past the file's last,
/// that one by sector 0, and sector 0 by sector 1 again. A reader that
/// follows the chain without a loop check goes round for ever, growing its
/// list. The `Workbook` stream is 4,096 zero bytes at sector 3.
pub fn cfb_directory_loop() -> Vec<u8> {
    let mut fat = vec![1, 11, FAT_SECTOR];
    fat.extend((4..=10).chain([END_OF_CHAIN, 0]));
    let mut sectors = vec![
        vec![0u8; SECTOR],
        dir_sector(1, &[("Workbook", 3, 4096)]),
        fat_sector(&fat),
    ];
    sectors.extend(std::iter::repeat_n(vec![0u8; SECTOR], 8));
    let file = raw_cfb(1, 2, &sectors);
    assert_eq!(file.len(), 6144);
    file
}

/// The reviewed hidden stream: two directory entries named `Workbook`. The
/// first is linked from nowhere and holds `hidden`; the root's child is
/// the second, which holds `shown`. A reader that takes the first entry of
/// that name reads what a reader that walks the tree never sees.
pub fn cfb_hidden_workbook(hidden: &[u8], shown: &[u8]) -> Vec<u8> {
    let pad = |s: &[u8]| {
        let mut v = s.to_vec();
        v.resize(s.len().max(4096).div_ceil(SECTOR) * SECTOR, 0);
        v
    };
    let (hidden, shown) = (pad(hidden), pad(shown));
    let (h, s) = (hidden.len() / SECTOR, shown.len() / SECTOR);
    assert!(2 + h + s <= SECTOR / 4, "one FAT sector holds 128 entries");
    let chain = |from: usize, n: usize| {
        (from + 1..from + n)
            .map(|i| i as u32)
            .chain([END_OF_CHAIN])
            .collect::<Vec<u32>>()
    };
    // Sector 0 is the FAT, sector 1 the directory, the streams follow.
    let mut fat = vec![FAT_SECTOR, END_OF_CHAIN];
    fat.extend(chain(2, h));
    fat.extend(chain(2 + h, s));
    let mut sectors = vec![
        fat_sector(&fat),
        dir_sector(
            2,
            &[
                ("Workbook", 2, hidden.len() as u32),
                ("Workbook", (2 + h) as u32, shown.len() as u32),
            ],
        ),
    ];
    for stream in [&hidden, &shown] {
        sectors.extend(stream.chunks(SECTOR).map(<[u8]>::to_vec));
    }
    raw_cfb(1, 0, &sectors)
}

/// A little-endian TIFF with one page: `entries` (tag, type, count, value
/// or offset) after `data`, which starts at byte 8.
fn tiff(data: &[u8], entries: &[(u16, u16, u32, u32)]) -> Vec<u8> {
    let mut f = b"II*\0".to_vec();
    f.extend_from_slice(&(8 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(data);
    f.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, kind, count, value) in entries {
        f.extend_from_slice(&tag.to_le_bytes());
        f.extend_from_slice(&kind.to_le_bytes());
        f.extend_from_slice(&count.to_le_bytes());
        f.extend_from_slice(&value.to_le_bytes());
    }
    f.extend_from_slice(&0u32.to_le_bytes());
    f
}

const SHORT: u16 = 3;
const LONG: u16 = 4;

/// The reviewed TIFF: `side` × `side` pixels of four 16-bit samples, in
/// strips of 100 rows that all point at one deflated strip of zeros. A few
/// kilobytes of file, 8 bytes a pixel to decode.
pub fn rgba16_tiff(side: u32) -> Vec<u8> {
    let rows = 100u32;
    let strips = side / rows;
    let raw = vec![0u8; (rows * side * 8) as usize];
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&raw).unwrap();
    let blob = z.finish().unwrap();
    // After the header: the strip, then the arrays the directory points at.
    let mut data = blob.clone();
    let at = |d: &Vec<u8>| 8 + d.len() as u32;
    let offsets_at = at(&data);
    for _ in 0..strips {
        data.extend_from_slice(&8u32.to_le_bytes());
    }
    let counts_at = at(&data);
    for _ in 0..strips {
        data.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    }
    let bits_at = at(&data);
    for _ in 0..4 {
        data.extend_from_slice(&16u16.to_le_bytes());
    }
    tiff(
        &data,
        &[
            (256, LONG, 1, side),
            (257, LONG, 1, side),
            (258, SHORT, 4, bits_at),
            (259, SHORT, 1, 8), // deflate
            (262, SHORT, 1, 2), // RGB
            (273, LONG, strips, offsets_at),
            (277, SHORT, 1, 4),
            (278, LONG, 1, rows),
            (279, LONG, strips, counts_at),
            (338, SHORT, 1, 2), // the fourth sample is alpha
        ],
    )
}

/// A TIFF of uncompressed 8-bit grey pages, each `width` × `height` with
/// its own `pixels`.
pub fn grey_tiff(width: u32, height: u32, pages: &[&[u8]]) -> Vec<u8> {
    let mut f = b"II*\0".to_vec();
    f.extend_from_slice(&8u32.to_le_bytes());
    for (i, px) in pages.iter().enumerate() {
        assert_eq!(px.len() as u32, width * height);
        let ifd_at = f.len() as u32;
        let pixels_at = ifd_at + 2 + 8 * 12 + 4;
        let entries: [(u16, u16, u32, u32); 8] = [
            (256, LONG, 1, width),
            (257, LONG, 1, height),
            (258, SHORT, 1, 8),
            (259, SHORT, 1, 1),
            (262, SHORT, 1, 1),
            (273, LONG, 1, pixels_at),
            (278, LONG, 1, height),
            (279, LONG, 1, width * height),
        ];
        f.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (tag, kind, count, value) in entries {
            f.extend_from_slice(&tag.to_le_bytes());
            f.extend_from_slice(&kind.to_le_bytes());
            f.extend_from_slice(&count.to_le_bytes());
            f.extend_from_slice(&value.to_le_bytes());
        }
        let next = if i + 1 == pages.len() {
            0
        } else {
            pixels_at + width * height
        };
        f.extend_from_slice(&next.to_le_bytes());
        f.extend_from_slice(px);
    }
    f
}
