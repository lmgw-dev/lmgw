//! A cabinet (`.cab`) reader for the cabinets an MSI embeds ([`super::msi`]):
//! every folder is decompressed once, front to back, and its files are cut
//! out of it. (A reader that decompresses a folder from its start for every
//! file, as the `cab` crate's `read_file` does, took 69 s for eSpeak NG's
//! cabinet in a debug build.) Uncompressed and MSZIP folders are read;
//! Quantum and LZX are refused with their name.
//!
//! The format is Microsoft's "Cabinet File Format": a `CFHEADER`, its
//! `CFFOLDER`s, the `CFFILE`s at `coffFiles`, and per folder its `CFDATA`
//! blocks from `coffCabStart`. An MSZIP block is `CK` and a raw deflate
//! stream whose back-references reach into the previous block's last
//! 32 KiB.

use std::io::Write;

/// One file of a cabinet, its bytes.
pub struct CabFile {
    pub name: String,
    pub data: Vec<u8>,
}

fn u16_at(b: &[u8], at: usize) -> Result<u16, String> {
    b.get(at..at + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| format!("the cabinet ends at byte {at}"))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| format!("the cabinet ends at byte {at}"))
}

fn cstr_at(b: &[u8], at: usize) -> Result<(String, usize), String> {
    let rest = b
        .get(at..)
        .ok_or_else(|| format!("the cabinet ends at byte {at}"))?;
    let end = rest
        .iter()
        .position(|&c| c == 0)
        .ok_or("a cabinet name has no end")?;
    Ok((
        String::from_utf8_lossy(&rest[..end]).into_owned(),
        at + end + 1,
    ))
}

/// Every file of the cabinet `b`.
pub fn files(b: &[u8]) -> Result<Vec<CabFile>, String> {
    if b.get(..4) != Some(b"MSCF") {
        return Err("not a cabinet (no MSCF signature)".into());
    }
    let coff_files = u32_at(b, 16)? as usize;
    let n_folders = u16_at(b, 26)? as usize;
    let n_files = u16_at(b, 28)? as usize;
    let flags = u16_at(b, 30)?;
    let mut at = 36;
    let (mut folder_reserve, mut data_reserve) = (0usize, 0usize);
    if flags & 0x0004 != 0 {
        let header_reserve = u16_at(b, at)? as usize;
        folder_reserve = *b.get(at + 2).ok_or("the cabinet ends in its header")? as usize;
        data_reserve = *b.get(at + 3).ok_or("the cabinet ends in its header")? as usize;
        at += 4 + header_reserve;
    }
    if flags & 0x0003 != 0 {
        return Err("the cabinet is part of a set spanning several cabinets".into());
    }
    let mut folders = Vec::with_capacity(n_folders);
    for _ in 0..n_folders {
        let start = u32_at(b, at)? as usize;
        let blocks = u16_at(b, at + 4)? as usize;
        let compression = u16_at(b, at + 6)?;
        folders.push(folder(b, start, blocks, compression, data_reserve)?);
        at += 8 + folder_reserve;
    }
    let mut out = Vec::with_capacity(n_files);
    let mut at = coff_files;
    for _ in 0..n_files {
        let size = u32_at(b, at)? as usize;
        let offset = u32_at(b, at + 4)? as usize;
        let index = u16_at(b, at + 8)? as usize;
        let (name, next) = cstr_at(b, at + 16)?;
        at = next;
        let data = folders
            .get(index)
            .and_then(|f: &Vec<u8>| f.get(offset..offset + size))
            .ok_or_else(|| format!("{name} lies outside its folder"))?;
        out.push(CabFile {
            name,
            data: data.to_vec(),
        });
    }
    Ok(out)
}

/// One folder, decompressed.
fn folder(
    b: &[u8],
    mut at: usize,
    blocks: usize,
    compression: u16,
    reserve: usize,
) -> Result<Vec<u8>, String> {
    let kind = compression & 0x000f;
    let mut out: Vec<u8> = Vec::new();
    for _ in 0..blocks {
        let compressed = u16_at(b, at + 4)? as usize;
        let uncompressed = u16_at(b, at + 6)? as usize;
        let start = at + 8 + reserve;
        let data = b
            .get(start..start + compressed)
            .ok_or("a data block lies outside the cabinet")?;
        at = start + compressed;
        match kind {
            0 if uncompressed != compressed => {
                return Err(format!(
                    "an uncompressed data block holds {compressed} bytes, its header says \
                     {uncompressed}"
                ))
            }
            0 => out.extend_from_slice(data),
            1 => mszip_block(data, uncompressed, &mut out)?,
            2 => return Err("the cabinet is Quantum-compressed, which is not read yet".into()),
            3 => return Err("the cabinet is LZX-compressed, which is not read yet".into()),
            k => return Err(format!("the cabinet's compression type {k} is unknown")),
        }
    }
    Ok(out)
}

/// One MSZIP block appended to `out`, with `out`'s last 32 KiB as its
/// history: handed to the decompressor first as a stored deflate block,
/// whose output is dropped.
fn mszip_block(data: &[u8], uncompressed: usize, out: &mut Vec<u8>) -> Result<(), String> {
    let deflate = data
        .strip_prefix(b"CK")
        .ok_or("an MSZIP block lacks its CK signature")?;
    let mut d = flate2::write::DeflateDecoder::new(Vec::with_capacity(uncompressed + 32 * 1024));
    let history = &out[out.len().saturating_sub(32 * 1024)..];
    if !history.is_empty() {
        let n = history.len() as u16;
        let mut stored = vec![0u8];
        stored.extend_from_slice(&n.to_le_bytes());
        stored.extend_from_slice(&(!n).to_le_bytes());
        stored.extend_from_slice(history);
        d.write_all(&stored)
            .map_err(|e| format!("MSZIP history: {e}"))?;
    }
    d.write_all(deflate)
        .map_err(|e| format!("MSZIP block: {e}"))?;
    let all = d.finish().map_err(|e| format!("MSZIP block: {e}"))?;
    let new = &all[history.len()..];
    if new.len() != uncompressed {
        return Err(format!(
            "an MSZIP block gave {} bytes, its header says {uncompressed}",
            new.len()
        ));
    }
    out.extend_from_slice(new);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-folder, uncompressed cabinet with two files.
    fn stored_cab(files: &[(&str, &[u8])]) -> Vec<u8> {
        let names: usize = files.iter().map(|(n, _)| 16 + n.len() + 1).sum();
        let data: Vec<u8> = files.iter().flat_map(|(_, d)| d.iter().copied()).collect();
        let coff_files = 36 + 8;
        let coff_data = coff_files + names;
        let mut b = Vec::new();
        b.extend_from_slice(b"MSCF");
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&((coff_data + 8 + data.len()) as u32).to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&(coff_files as u32).to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&[3, 1]);
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&(files.len() as u16).to_le_bytes());
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        // CFFOLDER: data start, one block, no compression.
        b.extend_from_slice(&(coff_data as u32).to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        let mut off = 0u32;
        for (n, d) in files {
            b.extend_from_slice(&(d.len() as u32).to_le_bytes());
            b.extend_from_slice(&off.to_le_bytes());
            b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0x20, 0]);
            b.extend_from_slice(n.as_bytes());
            b.push(0);
            off += d.len() as u32;
        }
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&(data.len() as u16).to_le_bytes());
        b.extend_from_slice(&(data.len() as u16).to_le_bytes());
        b.extend_from_slice(&data);
        b
    }

    #[test]
    fn a_stored_cabinet_gives_its_files() {
        let cab = stored_cab(&[("a.txt", b"hello"), ("b.bin", b"\x00\x01")]);
        let f = files(&cab).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(
            (f[0].name.as_str(), f[0].data.as_slice()),
            ("a.txt", &b"hello"[..])
        );
        assert_eq!(
            (f[1].name.as_str(), f[1].data.as_slice()),
            ("b.bin", &b"\x00\x01"[..])
        );
        assert!(files(b"nope").is_err());
    }

    #[test]
    fn a_stored_block_whose_sizes_differ_is_refused() {
        let mut cab = stored_cab(&[("a.txt", b"hello")]);
        // The CFDATA's cbUncomp, the last two bytes before the data.
        let at = cab.len() - 5 - 2;
        cab[at..at + 2].copy_from_slice(&4u16.to_le_bytes());
        let err = files(&cab).err().unwrap();
        assert!(err.contains("header says 4"), "{err}");
    }

    /// Deflate's bits, least significant first; a Huffman code goes in
    /// from its most significant bit (RFC 1951 §3.1.1).
    #[derive(Default)]
    struct Bits {
        out: Vec<u8>,
        n: u32,
    }

    impl Bits {
        fn put(&mut self, value: u32, count: u32) {
            for i in 0..count {
                if self.n.is_multiple_of(8) {
                    self.out.push(0);
                }
                let bit = (value >> i) & 1;
                *self.out.last_mut().unwrap() |= (bit as u8) << (self.n % 8);
                self.n += 1;
            }
        }

        fn code(&mut self, code: u32, len: u32) {
            for i in (0..len).rev() {
                self.put((code >> i) & 1, 1);
            }
        }
    }

    #[test]
    fn an_mszip_block_reaches_into_the_previous_blocks_history() {
        let phrase = b"the quick brown fox jumps over the lazy dog ";
        let first = phrase.repeat(10);
        let mut c = flate2::Compress::new(flate2::Compression::best(), false);
        let mut buf = Vec::with_capacity(4096);
        c.compress_vec(&first, &mut buf, flate2::FlushCompress::Finish)
            .unwrap();
        let mut block1 = b"CK".to_vec();
        block1.extend_from_slice(&buf);
        let mut out = Vec::new();
        mszip_block(&block1, first.len(), &mut out).unwrap();
        assert_eq!(out, first);

        // Block 2 is one fixed-Huffman match and nothing else: length 10 at
        // distance 44 (one phrase back), which only block 1's output holds.
        let mut b = Bits::default();
        b.put(1, 1); // BFINAL
        b.put(1, 2); // BTYPE 01, fixed codes
        b.code(264 - 256, 7); // length 10: code 264, no extra bits
        b.code(10, 5); // distance 33..=48: code 10, 4 extra bits
        b.put(44 - 33, 4);
        b.code(0, 7); // end of block: code 256
        let mut block2 = b"CK".to_vec();
        block2.extend_from_slice(&b.out);
        let mut alone = Vec::new();
        assert!(
            mszip_block(&block2, 10, &mut alone).is_err(),
            "without the history the match reaches before the start"
        );
        mszip_block(&block2, 10, &mut out).unwrap();
        assert_eq!(&out[first.len()..], b"the quick ");
    }
}
