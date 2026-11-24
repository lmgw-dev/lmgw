//! What a run's stderr says, read over the whole stream as it arrives
//! (review 2 of the poppler limit, S2): the bytes kept for the message stop
//! at `STDERR_KEEP_BYTES`, but a damaged file can make poppler write a
//! warning a bad object for far longer before it says "Out of memory", or
//! before a later page's JPEG 2000 image fails. So the out-of-memory words
//! are matched over every byte ([`Scan`], a window of the longest word less
//! one byte carried across chunk borders), and a [`Watch`] is handed every
//! line (each cut to [`LINE_KEEP_BYTES`]). Nothing grows with the stream.

use std::sync::{Arc, Mutex};

/// The words a tool's failed allocation says (`run::ran_out_of_memory`),
/// matched without case: a tool's own message (poppler's and libtiff's "Out
/// of memory", ffmpeg's and libpng's "Memory allocation failed" and
/// "memory allocation error", pandoc's "memory allocation error: block too
/// big") or a C++ `bad_alloc`. Not "memory alloc" alone (review 3 of the
/// poppler limit, S1): poppler's "Bogus memory allocation size" is a damaged
/// object's overflowing size, which poppler skips and goes on (exit 0), and
/// openjpeg's "Memory allocation failure in opj_jp2_apply_pclr()" is a
/// JPEG 2000 image's, read by `pdf::wide::jpx` (`strings` on libpoppler
/// 26.08.0, libopenjp2 2.5.4, libavcodec.so.63, libpng16, libtiff.so.6
/// and pandoc). A Rust tool's "memory allocation of N bytes failed" is
/// followed by its abort, which [`MEMORY`] catches.
const OUT_OF_MEMORY: [&str; 4] = [
    "bad_alloc",
    "out of memory",
    "memory allocation failed",
    "memory allocation error",
];

/// The word an abort or segfault that follows a failed allocation says.
const MEMORY: &str = "memory";

/// poppler's line on a size past what it allocates at all (`gmallocn`'s
/// overflow check): a damaged object's, skipped, or a page bitmap too large
/// to render, where pdftoppm writes a 1 × 1 stub (`run::stub`).
const BOGUS_SIZE: &str = "bogus memory allocation size";

/// How much of one stderr line a [`Watch`] is handed: 1 KiB. poppler's and
/// openjpeg's lines are under 200 bytes; a longer line is handed its first
/// 1 KiB, the rest of it is not kept.
pub const LINE_KEEP_BYTES: usize = 1 << 10;

/// What the out-of-memory words said, over the whole stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Said {
    /// One of [`OUT_OF_MEMORY`].
    pub out_of_memory: bool,
    /// [`MEMORY`].
    pub memory: bool,
    /// [`BOGUS_SIZE`].
    pub bogus_size: bool,
}

impl Said {
    /// What `text` says, all of it at once (a run whose stderr was read
    /// whole, the confinement check's).
    pub fn of(text: &[u8]) -> Self {
        let mut s = Scan::default();
        s.feed(text);
        s.said
    }
}

/// [`Said`] over a stream fed chunk by chunk.
#[derive(Debug, Default)]
pub struct Scan {
    /// The last bytes of the stream so far, lowercased: one byte fewer than
    /// the longest word, so a word across a chunk border is still seen.
    carry: Vec<u8>,
    pub said: Said,
}

impl Scan {
    pub fn feed(&mut self, chunk: &[u8]) {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend(chunk.iter().map(u8::to_ascii_lowercase));
        let has = |w: &str| memchr::memmem::find(&buf, w.as_bytes()).is_some();
        self.said.out_of_memory |= OUT_OF_MEMORY.iter().any(|w| has(w));
        self.said.memory |= has(MEMORY);
        self.said.bogus_size |= has(BOGUS_SIZE);
        let longest = OUT_OF_MEMORY
            .iter()
            .chain([&MEMORY, &BOGUS_SIZE])
            .map(|w| w.len())
            .max()
            .unwrap_or(1);
        let keep = buf.len().min(longest - 1);
        buf.drain(..buf.len() - keep);
        self.carry = buf;
    }
}

/// Reads a run's stderr line by line as it arrives (poppler's JPEG 2000
/// lines, `pdf::wide::jpx`).
pub trait Watch: Send + std::fmt::Debug {
    /// One line, without its end, cut to [`LINE_KEEP_BYTES`].
    fn line(&mut self, line: &[u8]);
}

/// A [`Watch`] the run hands its lines to and its caller reads after.
pub type Watcher = Arc<Mutex<dyn Watch>>;

/// The lines of a stream fed chunk by chunk, each handed to a [`Watch`].
#[derive(Default)]
pub struct Lines {
    line: Vec<u8>,
}

impl Lines {
    pub fn feed(&mut self, chunk: &[u8], watch: &Watcher) {
        let mut rest = chunk;
        while let Some(at) = memchr::memchr(b'\n', rest) {
            self.take(&rest[..at]);
            self.hand(watch);
            rest = &rest[at + 1..];
        }
        self.take(rest);
    }

    /// The last line, which had no end.
    pub fn end(&mut self, watch: &Watcher) {
        if !self.line.is_empty() {
            self.hand(watch);
        }
    }

    fn take(&mut self, part: &[u8]) {
        let room = LINE_KEEP_BYTES.saturating_sub(self.line.len());
        self.line.extend_from_slice(&part[..part.len().min(room)]);
    }

    fn hand(&mut self, watch: &Watcher) {
        let line = self.line.strip_suffix(b"\r").unwrap_or(&self.line);
        watch.lock().unwrap_or_else(|e| e.into_inner()).line(line);
        self.line.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_across_a_chunk_border_and_past_any_bound_is_seen() {
        let mut s = Scan::default();
        // 1 MiB of other lines first, far past the 64 KiB kept to be shown.
        for _ in 0..(1 << 20) / 32 {
            s.feed(b"Syntax Warning: bad object 1 0\n");
        }
        assert_eq!(s.said, Said::default());
        s.feed(b"Syntax Error: Out of mem");
        assert!(!s.said.out_of_memory);
        s.feed(b"ory\n");
        assert!(s.said.out_of_memory && s.said.memory);
        assert!(s.carry.len() < 64, "nothing grows with the stream");
        assert!(
            Said::of(b"terminate called after throwing an instance of 'std::bad_alloc'")
                .out_of_memory
        );
    }

    /// Review 3 of the poppler limit, S1: only a failed allocation is out
    /// of memory, not poppler's damaged size nor openjpeg's palette line
    /// (that one is the JPEG 2000 watch's, `pdf::wide::jpx`).
    #[test]
    fn a_bogus_size_or_openjpeg_s_line_is_not_out_of_memory() {
        for line in [
            &b"Syntax Error: Bogus memory allocation size"[..],
            b"Syntax Error: Memory allocation failure in opj_jp2_apply_pclr().<0a>",
        ] {
            assert!(!Said::of(line).out_of_memory, "{line:?}");
        }
        assert!(Said::of(b"Syntax Error: Bogus memory allocation size").bogus_size);
        for line in [
            &b"Error: memory allocation failed"[..],
            b"[mjpeg @ 0x0] Memory allocation failed.",
            b"pandoc: memory allocation error: block too big",
        ] {
            assert!(Said::of(line).out_of_memory, "{line:?}");
        }
    }

    #[derive(Debug, Default)]
    struct Kept(Vec<Vec<u8>>);

    impl Watch for Kept {
        fn line(&mut self, line: &[u8]) {
            self.0.push(line.to_vec());
        }
    }

    #[test]
    fn lines_across_chunks_are_handed_whole_and_a_long_one_cut() {
        let kept = Arc::new(Mutex::new(Kept::default()));
        let w: Watcher = kept.clone();
        let mut l = Lines::default();
        l.feed(b"one\ntw", &w);
        l.feed(b"o\r\n", &w);
        l.feed(&vec![b'x'; 3 * LINE_KEEP_BYTES], &w);
        l.feed(b"\nlast", &w);
        l.end(&w);
        let k = &kept.lock().unwrap().0;
        assert_eq!(k.len(), 4);
        assert_eq!(
            (&k[0][..], &k[1][..], &k[3][..]),
            (&b"one"[..], &b"two"[..], &b"last"[..])
        );
        assert_eq!(k[2].len(), LINE_KEEP_BYTES);
    }
}
