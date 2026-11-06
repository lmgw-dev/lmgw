//! The header reader: every read is bounded by the real remaining file
//! length, and by the caller's bound when it gives one.

use std::io::Read;

use super::*;

// ---------------------------------------------------------------------------
// Reader — every read is bounded by the real remaining file length
// ---------------------------------------------------------------------------

pub(super) struct Reader<R: Read> {
    pub(super) inner: R,
    /// Bytes consumed so far. Tracked by hand rather than with `Seek` because
    /// the whole point is a single forward pass through a `BufReader`.
    pub(super) pos: u64,
    pub(super) file_size: u64,
    /// The byte no declared length may carry the header past
    /// ([`read_header_within`]); `None`: the file's size alone.
    pub(super) bound: Option<u64>,
}

/// How much of a string is given memory at a time ([`Reader::read_string`]):
/// the read buffer's size, so the memory a string holds follows the bytes
/// that arrived for it. A step, not a limit: a string of any length the file
/// can hold is read whole.
const STRING_STEP: u64 = 64 * 1024;

impl<R: Read> Reader<R> {
    pub(super) fn new(inner: R, file_size: u64) -> Self {
        Self {
            inner,
            pos: 0,
            file_size,
            bound: None,
        }
    }

    pub(super) fn remaining(&self) -> u64 {
        self.file_size.saturating_sub(self.pos)
    }

    pub(super) fn read_array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N]> {
        let mut buf = [0u8; N];
        match self.inner.read_exact(&mut buf) {
            Ok(()) => {
                self.pos += N as u64;
                Ok(buf)
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                Err(GgufError::UnexpectedEof(what))
            }
            Err(e) => Err(GgufError::Io(e)),
        }
    }

    pub(super) fn read_u32(&mut self, what: &'static str) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>(what)?))
    }

    pub(super) fn read_u64(&mut self, what: &'static str) -> Result<u64> {
        Ok(u64::from_le_bytes(self.read_array::<8>(what)?))
    }

    /// Refuse a declared count of `count` items that each occupy at least
    /// `min_each` bytes when they cannot fit in what is left of the file.
    /// This is the single guard that keeps a corrupt length from turning into
    /// a multi-gigabyte allocation, and its ceiling is the real file size —
    /// never a made-up maximum.
    pub(super) fn check_fits(&self, what: &'static str, count: u64, min_each: u64) -> Result<()> {
        // Saturating, not checked: an overflowing product is by definition
        // larger than any file and must be rejected, not wrapped.
        let need = count.saturating_mul(min_each);
        if need > self.remaining() {
            return Err(GgufError::TooLarge {
                what,
                len: count,
                remaining: self.remaining(),
            });
        }
        if let Some(bound) = self.bound {
            let declared_end = self.pos.saturating_add(need);
            if declared_end > bound {
                return Err(GgufError::OverBound {
                    what,
                    at: self.pos,
                    declared_end,
                    bound,
                });
            }
        }
        Ok(())
    }

    /// GGUF strings are a `u64` byte length followed by unterminated UTF-8.
    /// Decoded lossily: a mojibake token name should not fail a whole model.
    ///
    /// The bytes are given memory in [`STRING_STEP`]s as they arrive, each
    /// through `try_reserve` (find-a-model design D129): a length the file's
    /// total allows but the stream never delivers (a hostile header claiming
    /// a 19 GB string) holds only what came, and memory the system can't
    /// give is [`GgufError::OutOfMemory`], not an abort of the process.
    pub(super) fn read_string(&mut self, what: &'static str) -> Result<String> {
        let len = self.read_u64(what)?;
        self.check_fits(what, len, 1)?;
        let mut buf: Vec<u8> = Vec::new();
        let mut left = len;
        while left > 0 {
            let step = left.min(STRING_STEP) as usize;
            buf.try_reserve(step).map_err(|_| GgufError::OutOfMemory {
                what,
                len,
                got: len - left,
            })?;
            let at = buf.len();
            buf.resize(at + step, 0);
            match self.inner.read_exact(&mut buf[at..]) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Err(GgufError::UnexpectedEof(what))
                }
                Err(e) => return Err(GgufError::Io(e)),
            }
            self.pos += step as u64;
            left -= step as u64;
        }
        // Reuse the buffer when it is already valid UTF-8 — which it
        // essentially always is. Going through `from_utf8_lossy`
        // unconditionally kept the raw bytes and a copy alive at once, and
        // each invalid byte expands to a 3-byte U+FFFD, so a corrupt length
        // field cost ~4x the declared size in RAM.
        Ok(match String::from_utf8(buf) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        })
    }

    pub(super) fn read_value(&mut self, ty: u32, depth: u32) -> Result<GgufValue> {
        Ok(match ty {
            VT_U8 => GgufValue::U8(self.read_array::<1>("a u8")?[0]),
            VT_I8 => GgufValue::I8(i8::from_le_bytes(self.read_array::<1>("an i8")?)),
            VT_U16 => GgufValue::U16(u16::from_le_bytes(self.read_array::<2>("a u16")?)),
            VT_I16 => GgufValue::I16(i16::from_le_bytes(self.read_array::<2>("an i16")?)),
            VT_U32 => GgufValue::U32(self.read_u32("a u32")?),
            VT_I32 => GgufValue::I32(i32::from_le_bytes(self.read_array::<4>("an i32")?)),
            VT_F32 => GgufValue::F32(f32::from_le_bytes(self.read_array::<4>("an f32")?)),
            // Any non-zero byte is true; the spec says 1 but writers have
            // shipped 0xff.
            VT_BOOL => GgufValue::Bool(self.read_array::<1>("a bool")?[0] != 0),
            VT_STRING => GgufValue::String(self.read_string("a string")?),
            VT_U64 => GgufValue::U64(self.read_u64("a u64")?),
            VT_I64 => GgufValue::I64(i64::from_le_bytes(self.read_array::<8>("an i64")?)),
            VT_F64 => GgufValue::F64(f64::from_le_bytes(self.read_array::<8>("an f64")?)),
            VT_ARRAY => {
                let elem_type = self.read_u32("an array element type")?;
                let len = self.read_u64("an array length")?;
                self.check_fits("an array", len, min_encoded_size(elem_type)?)?;
                let keep = if depth >= MAX_PREVIEW_DEPTH {
                    0
                } else if scalar_size(elem_type).is_some() && len <= NUMERIC_ARRAY_RETAIN {
                    len
                } else {
                    ARRAY_PREVIEW as u64
                };
                let mut preview = Vec::with_capacity(keep.min(len) as usize);
                let previewed = len.min(keep);
                for _ in 0..previewed {
                    preview.push(self.read_value(elem_type, depth + 1)?);
                }
                // Skip the remainder in one operation when the element width
                // is fixed. Element-by-element cost ~19s per GiB — each step
                // allocating and running its own `io::copy` — so a declared
                // multi-billion-element array of bytes (which passes the size
                // check, since one byte is one byte) turned a header read into
                // minutes on a blocking thread.
                let rest = len - previewed;
                match scalar_size(elem_type) {
                    Some(w) => {
                        let bytes = rest.checked_mul(w).ok_or(GgufError::TooLarge {
                            what: "an array payload",
                            len: rest,
                            remaining: self.remaining(),
                        })?;
                        self.check_fits("an array payload", bytes, 1)?;
                        self.discard(bytes, "an array payload")?;
                    }
                    // Strings and nested arrays have no fixed width.
                    None => {
                        for _ in 0..rest {
                            self.skip_value(elem_type)?;
                        }
                    }
                }
                GgufValue::Array {
                    elem_type,
                    len,
                    preview,
                }
            }
            other => return Err(GgufError::BadValueType(other)),
        })
    }

    /// Consume one value of `ty` without building a [`GgufValue`].
    ///
    /// Nested arrays are walked with an explicit stack instead of recursion:
    /// a crafted file can nest arbitrarily deep (each level costs only 12
    /// header bytes), and recursing there would be a stack overflow — i.e. a
    /// crash — rather than an error. The stack's depth is bounded by the file
    /// length, same as everything else here.
    pub(super) fn skip_value(&mut self, ty: u32) -> Result<()> {
        // (element type, elements still to consume)
        let mut stack: Vec<(u32, u64)> = vec![(ty, 1)];
        while let Some(&(elem_type, todo)) = stack.last() {
            if todo == 0 {
                stack.pop();
                continue;
            }
            if let Some(top) = stack.last_mut() {
                top.1 -= 1;
            }
            match elem_type {
                VT_ARRAY => {
                    let inner_type = self.read_u32("an array element type")?;
                    let len = self.read_u64("an array length")?;
                    self.check_fits("an array", len, min_encoded_size(inner_type)?)?;
                    stack.push((inner_type, len));
                }
                VT_STRING => {
                    let len = self.read_u64("a string")?;
                    self.check_fits("a string", len, 1)?;
                    self.discard(len, "a string")?;
                }
                t => {
                    let n = scalar_size(t).ok_or(GgufError::BadValueType(t))?;
                    self.discard(n, "a value")?;
                }
            }
        }
        Ok(())
    }

    /// Read and throw away `n` bytes. Seeking is not an option: the values
    /// being skipped are variable-length, so their end is only known by
    /// walking them — and the underlying reader is a forward-only
    /// `BufReader` on purpose.
    pub(super) fn discard(&mut self, n: u64, what: &'static str) -> Result<()> {
        let copied = std::io::copy(&mut self.inner.by_ref().take(n), &mut std::io::sink())?;
        self.pos += copied;
        if copied < n {
            return Err(GgufError::UnexpectedEof(what));
        }
        Ok(())
    }

    /// Read one array value, the type tag (`VT_ARRAY`) already consumed by
    /// the caller, and fold every element into a hash instead of retaining it
    /// — [`ladder`](crate::ladder)'s tokenizer-identity check (design §4.3
    /// rule 5) needs the vocabulary and merge tables *in full*, but
    /// `read_value`'s `preview` deliberately truncates exactly those arrays
    /// (`ARRAY_PREVIEW`/`NUMERIC_ARRAY_RETAIN`) because a quarter-million-entry
    /// table is too large to keep in memory just to compare two files.
    /// Streaming a hash costs the same one forward pass `read_value` already
    /// pays to skip an array it does not retain. Returns `(true length,
    /// hash)`; the hash only has to agree with itself within one run of lmgw
    /// (two files compared in the same validation call), never across
    /// versions or processes, so a plain [`DefaultHasher`](std::collections::hash_map::DefaultHasher)
    /// is enough.
    pub(super) fn read_and_hash_array(&mut self) -> Result<(u64, u64)> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let elem_type = self.read_u32("an array element type")?;
        let len = self.read_u64("an array length")?;
        self.check_fits("an array", len, min_encoded_size(elem_type)?)?;
        let mut hasher = DefaultHasher::new();
        len.hash(&mut hasher);
        elem_type.hash(&mut hasher);
        for _ in 0..len {
            match elem_type {
                VT_STRING => self.read_string("an array string")?.hash(&mut hasher),
                VT_U8 => self.read_array::<1>("a u8")?[0].hash(&mut hasher),
                VT_I8 => (self.read_array::<1>("an i8")?[0] as i8).hash(&mut hasher),
                VT_U16 => u16::from_le_bytes(self.read_array::<2>("a u16")?).hash(&mut hasher),
                VT_I16 => i16::from_le_bytes(self.read_array::<2>("an i16")?).hash(&mut hasher),
                VT_U32 => self.read_u32("a u32")?.hash(&mut hasher),
                VT_I32 => i32::from_le_bytes(self.read_array::<4>("an i32")?).hash(&mut hasher),
                VT_U64 => self.read_u64("a u64")?.hash(&mut hasher),
                VT_I64 => i64::from_le_bytes(self.read_array::<8>("an i64")?).hash(&mut hasher),
                VT_BOOL => (self.read_array::<1>("a bool")?[0] != 0).hash(&mut hasher),
                VT_F32 => f32::from_le_bytes(self.read_array::<4>("an f32")?)
                    .to_bits()
                    .hash(&mut hasher),
                VT_F64 => f64::from_le_bytes(self.read_array::<8>("an f64")?)
                    .to_bits()
                    .hash(&mut hasher),
                // No known tokenizer array nests one, but folding its own
                // hash in is honest and cheap where refusing the whole file
                // would not be.
                VT_ARRAY => self.read_and_hash_array()?.hash(&mut hasher),
                other => return Err(GgufError::BadValueType(other)),
            }
        }
        Ok((len, hasher.finish()))
    }
}

/// Encoded byte size of a fixed-width value type, `None` for string/array.
pub(super) fn scalar_size(ty: u32) -> Option<u64> {
    Some(match ty {
        VT_U8 | VT_I8 | VT_BOOL => 1,
        VT_U16 | VT_I16 => 2,
        VT_U32 | VT_I32 | VT_F32 => 4,
        VT_U64 | VT_I64 | VT_F64 => 8,
        _ => return None,
    })
}

/// Smallest number of bytes one value of `ty` can occupy — the multiplier that
/// turns a declared array length into a byte requirement we can bounds-check.
pub(super) fn min_encoded_size(ty: u32) -> Result<u64> {
    match ty {
        VT_STRING => Ok(8), // u64 length, possibly zero bytes of text
        VT_ARRAY => Ok(12), // element type u32 + count u64
        t => scalar_size(t).ok_or(GgufError::BadValueType(t)),
    }
}
