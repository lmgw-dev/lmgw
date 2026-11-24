//! poppler's lines on a JPEG 2000 image it could not decode, read as they
//! arrive ([`Jpx`], a `run::Watch`): whether openjpeg ran out of memory
//! (run again under the wide limit), or the image is damaged or in a form
//! openjpeg does not read (not run again: more memory decodes nothing).
//!
//! Probed 2026-11-20 on poppler 26.08.0 with openjpeg 2.5.4 (`prlimit
//! --as`; the JPEG 2000 scans of the hostile inputs, and single-tile,
//! tiled, raw-codestream, truncated, zeroed and bit-flipped images made
//! with `opj_compress`). poppler tries three openjpeg decoders and ends
//! every image none could decode with [`JPX_UNDECODED`], whatever the
//! cause. openjpeg's own lines come through poppler ending in `<0a>` (its
//! newline, escaped): its errors as "Syntax Error: …", its warnings as
//! "Syntax Warning: …" (probed: an unknown marker segment in the main
//! header, "Syntax Warning: Unknown marker<0a>", on an image that decodes).
//! A warning says nothing of why a decode failed (review 3, S2: a tiled
//! image with one is still memory when its picture fails silently). Before
//! the end line:
//! - **memory, said:** an openjpeg allocation line ([`ALLOCATION`]), e.g.
//!   "Size of tile data exceeds system limits" for a single-tile image;
//! - **memory, silent:** a tiled image whose whole picture openjpeg could
//!   not allocate says nothing of it: poppler's "Unable to decode image"
//!   follows no openjpeg line but the JP2 wrapper's
//!   ([`WRAPPER`], "Failed to decode the codestream in the JP2 file"), or
//!   none at all for a raw codestream;
//! - **damaged:** an openjpeg error line saying what is wrong ("Stream too short",
//!   "Tile part length size inconsistent with stream length", "A marker ID
//!   was expected", "read: segment too long …"), the same under every
//!   limit.

use std::sync::{Arc, Mutex};

use crate::extract::convert::run::{Watch, Watcher};

/// poppler's last line on a JPEG 2000 image it drew blank, as poppler
/// 26.08 spells it (the decoders it tried before end in "…, trying as …").
pub const JPX_UNDECODED: &str = "Did no succeed opening JPX Stream.";

/// openjpeg 2.5.4's lines on an allocation that failed (`strings
/// libopenjp2.so.2.5.4`; "allocation fail" also reads "Memory allocation
/// failure in opj_jp2_apply_pclr()", which `run`'s out-of-memory words
/// leave to this watch), matched without case.
const ALLOCATION: [&str; 7] = [
    "exceeds system limits",
    "not enough memory",
    "cannot allocate",
    "memory error",
    "allocation fail",
    "failed to allocate",
    "error allocating",
];

/// openjpeg's JP2 wrapper's line on any decode that failed: it says
/// nothing of why.
const WRAPPER: &str = "Failed to decode the codestream in the JP2 file";

/// poppler's line when openjpeg's decode of an image failed.
const UNABLE: &str = "Unable to decode image";

/// How poppler starts an openjpeg error (`errSyntaxError`); its warnings
/// start "Syntax Warning: ".
const ERROR: &[u8] = b"Syntax Error: ";

/// How poppler's JPEG 2000 images fared in one run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Blank {
    /// Drawn blank because openjpeg ran out of memory.
    pub memory: u32,
    /// Drawn blank because they are damaged or not readable by openjpeg.
    pub damaged: u32,
}

/// [`Blank`] read line by line (module doc).
#[derive(Debug, Default)]
pub struct Jpx {
    /// An openjpeg error line that says why since poppler's last line.
    explained: bool,
    /// This image's decode ran out of memory.
    memory: bool,
    pub blank: Blank,
}

impl Jpx {
    /// A fresh watch, to hand a run and read after it.
    pub fn watcher() -> (Arc<Mutex<Jpx>>, Watcher) {
        let j = Arc::new(Mutex::new(Jpx::default()));
        let w: Watcher = j.clone();
        (j, w)
    }
}

impl Watch for Jpx {
    fn line(&mut self, line: &[u8]) {
        let has = |w: &str| memchr::memmem::find(line, w.as_bytes()).is_some();
        if line.ends_with(b"<0a>") {
            let lower = line.to_ascii_lowercase();
            if ALLOCATION
                .iter()
                .any(|w| memchr::memmem::find(&lower, w.as_bytes()).is_some())
            {
                self.memory = true;
            }
            if line.starts_with(ERROR) && !has(WRAPPER) {
                self.explained = true;
            }
        } else if has(UNABLE) {
            // A decode that failed with no openjpeg line saying why: the
            // whole picture of a tiled image could not be allocated.
            if !self.explained {
                self.memory = true;
            }
            self.explained = false;
        } else if has(JPX_UNDECODED) {
            if self.memory {
                self.blank.memory += 1;
            } else {
                self.blank.damaged += 1;
            }
            (self.memory, self.explained) = (false, false);
        } else {
            self.explained = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank(stderr: &str) -> Blank {
        let mut j = Jpx::default();
        for l in stderr.lines() {
            j.line(l.as_bytes());
        }
        j.blank
    }

    /// The lines poppler 26.08.0 wrote (module doc's probe), word for word.
    #[test]
    fn memory_and_damage_are_told_apart() {
        let single = "Syntax Error: Size of tile data exceeds system limits<0a>
Syntax Error: Failed to decode.<0a>
Syntax Error: Failed to decode tile 1/1<0a>
Syntax Error: Failed to decode the codestream in the JP2 file<0a>
Syntax Warning: Unable to decode image
Syntax Warning: Did no succeed opening JPX Stream as JP2, trying as J2K.
Syntax Error: Expected a SOC marker <0a>
Syntax Warning: Unable to read header
Syntax Warning: Did no succeed opening JPX Stream as J2K, trying as JPT.
Syntax Warning: Unable to create decoder
Syntax Error: Did no succeed opening JPX Stream.";
        let tiled = "Syntax Error: Failed to decode the codestream in the JP2 file<0a>
Syntax Warning: Unable to decode image
Syntax Warning: Did no succeed opening JPX Stream as JP2, trying as J2K.
Syntax Error: Expected a SOC marker <0a>
Syntax Warning: Unable to read header
Syntax Warning: Did no succeed opening JPX Stream as J2K, trying as JPT.
Syntax Warning: Unable to create decoder
Syntax Error: Did no succeed opening JPX Stream.";
        let raw_tiled =
            "Syntax Error: Malformed JP2 file format: first box must be JPEG 2000 signature box<0a>
Syntax Warning: Unable to read header
Syntax Warning: Did no succeed opening JPX Stream as JP2, trying as J2K.
Syntax Warning: Unable to decode image
Syntax Warning: Did no succeed opening JPX Stream as J2K, trying as JPT.
Syntax Warning: Unable to create decoder
Syntax Error: Did no succeed opening JPX Stream.";
        let truncated = "Syntax Error: Tile part length size inconsistent with stream length<0a>
Syntax Error: Failed to decode the codestream in the JP2 file<0a>
Syntax Warning: Unable to decode image
Syntax Warning: Did no succeed opening JPX Stream as JP2, trying as J2K.
Syntax Error: Expected a SOC marker <0a>
Syntax Warning: Unable to read header
Syntax Warning: Did no succeed opening JPX Stream as J2K, trying as JPT.
Syntax Warning: Unable to create decoder
Syntax Error: Did no succeed opening JPX Stream.";
        let header_cut = "Syntax Error: Stream too short<0a>
Syntax Warning: Unable to read header
Syntax Warning: Did no succeed opening JPX Stream as JP2, trying as J2K.
Syntax Error: Expected a SOC marker <0a>
Syntax Warning: Unable to read header
Syntax Warning: Did no succeed opening JPX Stream as J2K, trying as JPT.
Syntax Warning: Unable to create decoder
Syntax Error: Did no succeed opening JPX Stream.";
        let raw_flipped =
            "Syntax Error: Malformed JP2 file format: first box must be JPEG 2000 signature box<0a>
Syntax Warning: Unable to read header
Syntax Warning: Did no succeed opening JPX Stream as JP2, trying as J2K.
Syntax Error: read: segment too long (2889) with max (2239) for codeblock 0 (p=0, b=2, r=5, c=2)<0a>
Syntax Error: Failed to decode.<0a>
Syntax Error: Failed to decode tile 12/36<0a>
Syntax Warning: Unable to decode image
Syntax Warning: Did no succeed opening JPX Stream as J2K, trying as JPT.
Syntax Warning: Unable to create decoder
Syntax Error: Did no succeed opening JPX Stream.";
        let mem = Blank {
            memory: 1,
            damaged: 0,
        };
        let bad = Blank {
            memory: 0,
            damaged: 1,
        };
        assert_eq!(blank(single), mem);
        assert_eq!(blank(tiled), mem);
        assert_eq!(blank(raw_tiled), mem);
        assert_eq!(blank(truncated), bad);
        assert_eq!(blank(header_cut), bad);
        assert_eq!(blank(raw_flipped), bad);
        // Two images in one run, each counted by its own lines.
        assert_eq!(
            blank(&format!("{truncated}\nSyntax Warning: other\n{tiled}")),
            Blank {
                memory: 1,
                damaged: 1
            }
        );
        assert_eq!(blank("Syntax Error: Out of memory"), Blank::default());
    }

    /// Review 3 of the poppler limit: an openjpeg warning before a silent
    /// failure says nothing of why (S2; the lines poppler 26.08.0 wrote for
    /// a tiled 8000² image with an unknown marker segment under
    /// `prlimit --as` 512 MiB, word for word: it decodes at 4 GiB), and
    /// openjpeg's palette allocation line is memory (S1).
    #[test]
    fn a_warning_is_no_reason_and_the_palette_line_is_memory() {
        let warned_tiled = "Syntax Warning: Unknown marker<0a>
Syntax Error: Failed to decode the codestream in the JP2 file<0a>
Syntax Warning: Unable to decode image
Syntax Warning: Did no succeed opening JPX Stream as JP2, trying as J2K.
Syntax Error: Expected a SOC marker <0a>
Syntax Warning: Unable to read header
Syntax Warning: Did no succeed opening JPX Stream as J2K, trying as JPT.
Syntax Warning: Unable to create decoder
Syntax Error: Did no succeed opening JPX Stream.";
        let pclr = "Syntax Error: Memory allocation failure in opj_jp2_apply_pclr().<0a>
Syntax Error: Failed to decode the codestream in the JP2 file<0a>
Syntax Warning: Unable to decode image
Syntax Warning: Did no succeed opening JPX Stream as JP2, trying as J2K.
Syntax Error: Expected a SOC marker <0a>
Syntax Warning: Unable to read header
Syntax Warning: Did no succeed opening JPX Stream as J2K, trying as JPT.
Syntax Warning: Unable to create decoder
Syntax Error: Did no succeed opening JPX Stream.";
        let mem = Blank {
            memory: 1,
            damaged: 0,
        };
        assert_eq!(blank(warned_tiled), mem);
        assert_eq!(blank(pclr), mem);
    }
}
