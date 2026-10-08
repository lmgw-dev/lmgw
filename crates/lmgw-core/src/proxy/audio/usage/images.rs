//! The images in an `/v1/images/*` answer, counted as it passes
//! (billable-units design §4.4): lmgw's own count of what came back, so it
//! is read on local sd-server rows too (§4.7). Never the request's `n`.
//!
//! - **A JSON body** goes through [`DataCount`], a byte scanner that tracks
//!   depth, string and escape state and the key at depth 1, and counts the
//!   elements of the top-level `data` array. Its state is a few integers, so
//!   a 20 MB base64 answer is never held: no size cap, none needed (§9.1).
//! - **An event stream** (`stream: true`) counts the events that each carry
//!   one final image ([`FINAL_IMAGE_EVENTS`]); partial images never count.
//!   Each such event is one base64 image of several megabytes, so the stream
//!   is read the same way, line by line and byte by byte ([`FinalEvents`]):
//!   linear in its length, and no event is ever held.
//!
//! Either gives `None` rather than a guess: a body with no top-level `data`
//! array, one that ends mid-document, a stream in which no final event was
//! recognised or which broke off. An empty `data` is a measured 0.

/// The stream events that each carry one final image, from OpenAI's API
/// reference (Images: `ImageGenCompletedEvent`, "emitted when image
/// generation has completed and the final image is available", and its
/// edit twin `ImageEditCompletedEvent`), read 2026-10-07.
pub(super) const FINAL_IMAGE_EVENTS: [&str; 2] =
    ["image_generation.completed", "image_edit.completed"];

/// The longest final-event name: all a [`Name`] needs to keep.
const LONGEST: usize = longest(&FINAL_IMAGE_EVENTS);

const fn longest(names: &[&str]) -> usize {
    let (mut n, mut i) = (0, 0);
    while i < names.len() {
        if names[i].len() > n {
            n = names[i].len();
        }
        i += 1;
    }
    n
}

/// One more byte `b` of a key that may spell `want`: how much of it matched
/// so far, `None` once it cannot.
fn match_on(matched: Option<usize>, want: &[u8], b: u8) -> Option<usize> {
    matched.filter(|&n| want.get(n) == Some(&b)).map(|n| n + 1)
}

/// The elements of a JSON body's top-level `data` array, counted byte by
/// byte. Lenient about everything but what it counts: it does not validate
/// the document, only follows it far enough to know where `data` is.
#[derive(Debug, Default)]
pub(super) struct DataCount {
    /// Containers (`{`, `[`) open around the current byte.
    depth: u64,
    in_string: bool,
    /// The previous byte was a backslash inside a string.
    escaped: bool,
    /// The top-level object has closed: the document is whole.
    whole: bool,
    /// Nothing this reads: the top level is no object, or bytes follow it.
    broken: bool,
    /// At depth 1, the next string is a key.
    expect_key: bool,
    /// A depth-1 key is being read: how much of `data` it matched so far,
    /// `None` once it cannot be `data`.
    key: Option<Option<usize>>,
    /// The last depth-1 key read was `data`.
    key_is_data: bool,
    /// `data:` was read, and its value starts at the next byte.
    data_value_next: bool,
    /// Inside the top-level `data` array.
    in_data: bool,
    /// An element of `data` may start here (after `[` or `,`).
    element_next: bool,
    count: u64,
    /// What the top-level `data` was once read: its element count, or
    /// `Err` when it was no array. The last `data` wins, as in a parser.
    data: Option<Result<u64, ()>>,
}

impl DataCount {
    pub(super) fn feed(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if self.broken {
                return;
            }
            self.byte(b);
        }
    }

    /// The images the answer held; `None` for a body that is not a whole
    /// object with a top-level `data` array.
    pub(super) fn images(&self) -> Option<u64> {
        if !self.whole || self.broken {
            return None;
        }
        self.data.and_then(Result::ok)
    }

    fn byte(&mut self, b: u8) {
        if self.in_string {
            self.string_byte(b);
            return;
        }
        if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
            return;
        }
        if self.whole {
            self.broken = true;
            return;
        }
        if self.depth == 0 {
            if b == b'{' {
                self.depth = 1;
                self.expect_key = true;
            } else {
                self.broken = true;
            }
            return;
        }
        // A value starting inside `data`, at the array's own level: one
        // element. Its inner bytes are deeper, or follow its first.
        if self.in_data && self.depth == 2 && self.element_next && !matches!(b, b',' | b']') {
            self.count += 1;
            self.element_next = false;
        }
        // The value of `data:` starts here.
        if self.data_value_next {
            self.data_value_next = false;
            if b == b'[' {
                self.in_data = true;
                self.element_next = true;
                self.count = 0;
            } else {
                self.data = Some(Err(()));
            }
        }
        match b {
            b'"' => {
                self.in_string = true;
                if self.depth == 1 && self.expect_key {
                    self.expect_key = false;
                    self.key = Some(Some(0));
                }
            }
            b'{' | b'[' => self.depth += 1,
            b'}' | b']' => {
                if self.in_data && self.depth == 2 {
                    self.in_data = false;
                    self.data = Some(Ok(self.count));
                }
                self.depth -= 1;
                self.whole = self.depth == 0;
            }
            b',' => {
                if self.depth == 1 {
                    self.expect_key = true;
                } else if self.in_data && self.depth == 2 {
                    self.element_next = true;
                }
            }
            b':' if self.depth == 1 => {
                self.data_value_next = std::mem::take(&mut self.key_is_data);
            }
            _ => {}
        }
    }

    fn string_byte(&mut self, b: u8) {
        const DATA: &[u8] = b"data";
        if self.escaped {
            self.escaped = false;
            return;
        }
        match b {
            // A key with an escape in it may still spell `data` through a
            // Unicode escape. It is not read as `data`, so such a body counts
            // as having none: `None`.
            b'\\' => {
                self.escaped = true;
                if let Some(k) = self.key.as_mut() {
                    *k = None;
                }
            }
            b'"' => {
                self.in_string = false;
                if let Some(k) = self.key.take() {
                    self.key_is_data = k == Some(DATA.len());
                }
            }
            _ => {
                if let Some(k) = self.key.as_mut() {
                    *k = match_on(*k, DATA, b);
                }
            }
        }
    }
}

/// The final-image events of an image event stream, counted as they pass.
///
/// Read the way `crate::sse::SseDecoder` reads a stream — a CR, LF or CRLF
/// ends a line, a blank line ends an event, `event:` (leading spaces
/// trimmed) and `data:` (one leading space dropped, lines joined by a
/// newline) are the fields read, anything else is skipped — but byte by
/// byte, with nothing kept but the state below: the decoder holds a whole
/// event and searches it again on every chunk, which for a multi-megabyte
/// image is quadratic on the relay path. An event's type is what it was
/// with the decoder: the top-level `type` of its JSON data when that is a
/// string ([`TypeScan`]), else its `event:` name.
#[derive(Default)]
pub(super) struct FinalEvents {
    /// Where in its line the current byte is.
    line: Line,
    /// The last byte was a CR: an LF right after it ends the same line.
    after_cr: bool,
    /// The current event's `event:` name, once it has one.
    name: Option<Name>,
    /// The current event has data: a further `data:` line is joined to it
    /// by a newline.
    has_data: bool,
    /// The current event's data, followed for its `type`.
    data: TypeScan,
    finals: u64,
}

/// Where in its line a stream is.
#[derive(Default)]
enum Line {
    /// At its start: a line end here is a blank line, which ends the event.
    #[default]
    Start,
    /// In the field name, before its colon.
    Field(Name),
    /// In `event:`'s value; `lead` while its leading spaces are skipped.
    Event { lead: bool },
    /// In `data:`'s value; `lead` until its one leading space is dropped.
    Data { lead: bool },
    /// In a line nothing reads: another field, a comment.
    Skip,
}

impl FinalEvents {
    pub(super) fn feed(&mut self, mut chunk: &[u8]) {
        while let Some((&b, rest)) = chunk.split_first() {
            // A data line's bytes up to its end go to the scanner in one go.
            if let Line::Data { lead: false } = self.line {
                let end = chunk
                    .iter()
                    .position(|&c| c == b'\n' || c == b'\r')
                    .unwrap_or(chunk.len());
                if end > 0 {
                    self.data.feed(&chunk[..end]);
                    chunk = &chunk[end..];
                    continue;
                }
            }
            self.byte(b);
            chunk = rest;
        }
    }

    /// The images the stream carried: `None` when no final event was
    /// recognised, or when the relay did not end `whole` — a stream that
    /// broke off may have had more images coming, and nothing in it says
    /// how many.
    pub(super) fn images(mut self, whole: bool) -> Option<u64> {
        // A last event not ended by a blank line is ended by the stream.
        self.feed(b"\n\n");
        (whole && self.finals > 0).then_some(self.finals)
    }

    fn byte(&mut self, b: u8) {
        if std::mem::take(&mut self.after_cr) && b == b'\n' {
            return;
        }
        if b == b'\n' || b == b'\r' {
            self.after_cr = b == b'\r';
            if let Line::Start = self.line {
                self.dispatch();
            }
            self.line = Line::Start;
            return;
        }
        match &mut self.line {
            Line::Start => {
                self.line = Line::Field(Name::default());
                self.field_byte(b);
            }
            Line::Field(_) => self.field_byte(b),
            Line::Event { lead } => {
                if *lead && b == b' ' {
                    return;
                }
                *lead = false;
                if let Some(name) = self.name.as_mut() {
                    name.push(b);
                }
            }
            Line::Data { lead } => {
                let dropped = std::mem::take(lead) && b == b' ';
                if !dropped {
                    self.data.feed(&[b]);
                }
            }
            Line::Skip => {}
        }
    }

    fn field_byte(&mut self, b: u8) {
        let Line::Field(field) = &mut self.line else {
            return;
        };
        if b != b':' {
            field.push(b);
            return;
        }
        let (event, data) = (field.is("event"), field.is("data"));
        self.line = if event {
            self.name = Some(Name::default());
            Line::Event { lead: true }
        } else if data {
            if std::mem::replace(&mut self.has_data, true) {
                self.data.feed(b"\n");
            }
            Line::Data { lead: true }
        } else {
            Line::Skip
        };
    }

    /// A blank line: the event ends, and is counted when it is a final one.
    fn dispatch(&mut self) {
        let data = std::mem::take(&mut self.data);
        let name = self.name.take();
        let had_data = std::mem::take(&mut self.has_data);
        if !had_data && name.is_none() {
            return;
        }
        if data.kind().or(name).is_some_and(|k| k.is_final()) {
            self.finals += 1;
        }
    }
}

/// A short name — an SSE field's, an event's type — matched exactly as its
/// bytes pass. It keeps at most [`LONGEST`] bytes: every name it is compared
/// with fits, so a longer one is none of them, and an exact match needs no
/// more.
#[derive(Debug, Clone, Copy, Default)]
struct Name {
    bytes: [u8; LONGEST],
    len: usize,
    /// Longer than [`LONGEST`], or spelled with an escape: none of the
    /// names.
    other: bool,
}

impl Name {
    fn push(&mut self, b: u8) {
        match self.bytes.get_mut(self.len) {
            Some(slot) => {
                *slot = b;
                self.len += 1;
            }
            None => self.other = true,
        }
    }

    fn is(&self, s: &str) -> bool {
        !self.other && &self.bytes[..self.len] == s.as_bytes()
    }

    fn is_final(&self) -> bool {
        FINAL_IMAGE_EVENTS.iter().any(|f| self.is(f))
    }
}

/// The top-level `"type"` of an event's JSON data, followed byte by byte
/// the way [`DataCount`] follows `data`: its string value, once the object
/// is whole — what a parser would read, without holding the image beside
/// it. A value spelled with an escape is none of the final names.
#[derive(Debug, Default)]
struct TypeScan {
    depth: u64,
    in_string: bool,
    escaped: bool,
    whole: bool,
    broken: bool,
    expect_key: bool,
    /// A depth-1 key is being read: how much of `type` it matched so far.
    key: Option<Option<usize>>,
    key_is_type: bool,
    /// `"type":` was read, and its value starts at the next byte.
    value_next: bool,
    /// Reading the type's string value.
    reading: Option<Name>,
    /// The last top-level `type` read, when it was a string.
    found: Option<Name>,
}

impl TypeScan {
    fn feed(&mut self, mut s: &[u8]) {
        while let Some((&b, rest)) = s.split_first() {
            if self.broken {
                return;
            }
            // A string nothing is read from — the base64 image — is passed
            // over to its next quote or backslash in one go.
            if self.in_string && !self.escaped && self.key.is_none() && self.reading.is_none() {
                let skip = s
                    .iter()
                    .position(|&c| c == b'"' || c == b'\\')
                    .unwrap_or(s.len());
                if skip > 0 {
                    s = &s[skip..];
                    continue;
                }
            }
            self.byte(b);
            s = rest;
        }
    }

    fn byte(&mut self, b: u8) {
        if self.in_string {
            self.string_byte(b);
            return;
        }
        if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
            return;
        }
        if self.whole {
            self.broken = true;
            return;
        }
        if self.depth == 0 {
            if b == b'{' {
                self.depth = 1;
                self.expect_key = true;
            } else {
                self.broken = true;
            }
            return;
        }
        if std::mem::take(&mut self.value_next) {
            // Whatever `type` was before, it is this value now.
            self.found = None;
            if b == b'"' {
                self.reading = Some(Name::default());
            }
        }
        match b {
            b'"' => {
                self.in_string = true;
                if self.depth == 1 && self.expect_key {
                    self.expect_key = false;
                    self.key = Some(Some(0));
                }
            }
            b'{' | b'[' => self.depth += 1,
            b'}' | b']' => {
                self.depth -= 1;
                self.whole = self.depth == 0;
            }
            b',' if self.depth == 1 => self.expect_key = true,
            b':' if self.depth == 1 => {
                self.value_next = std::mem::take(&mut self.key_is_type);
            }
            _ => {}
        }
    }

    fn string_byte(&mut self, b: u8) {
        const TYPE: &[u8] = b"type";
        if self.escaped {
            self.escaped = false;
            return;
        }
        match b {
            b'\\' => {
                self.escaped = true;
                if let Some(k) = self.key.as_mut() {
                    *k = None;
                }
                if let Some(name) = self.reading.as_mut() {
                    name.other = true;
                }
            }
            b'"' => {
                self.in_string = false;
                if let Some(k) = self.key.take() {
                    self.key_is_type = k == Some(TYPE.len());
                }
                if let Some(name) = self.reading.take() {
                    self.found = Some(name);
                }
            }
            _ => {
                if let Some(k) = self.key.as_mut() {
                    *k = match_on(*k, TYPE, b);
                }
                if let Some(name) = self.reading.as_mut() {
                    name.push(b);
                }
            }
        }
    }

    /// The type, when the data was a whole object whose top-level `type`
    /// is a string.
    fn kind(&self) -> Option<Name> {
        if self.whole && !self.broken {
            self.found
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests;
