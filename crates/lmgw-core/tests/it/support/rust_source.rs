//! Rust source read as code: which bytes are comments, string or char
//! literals, and test code (`#[cfg(test)]` items, wherever in a file they
//! stand), so a scan of the workspace's sources never takes a comment for
//! code, nor a literal for a call (review CL-17). The Chat's repository
//! seam scan (`chat_repo_seam_scan`) and the Chat page's delete-elsewhere
//! scan (`chat_page_gone_scan`) read the sources through it.

use std::ops::Range;
use std::path::{Path, PathBuf};

use regex::Regex;

/// Every `.rs` file under `dir`, build output and hidden directories left
/// out.
pub fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !name.starts_with('.')
                && !matches!(name.as_str(), "target" | "dist" | "node_modules")
            {
                collect_rs(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

pub fn fn_header() -> Regex {
    Regex::new(
        r"^\s*(pub(\([^)]*\))?\s+)?(const\s+)?(async\s+)?(unsafe\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)",
    )
    .unwrap()
}

/// What a byte of Rust source is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Code,
    Comment,
    /// A string, byte string or char literal, its quotes included.
    Literal,
}

/// Each byte of `text` as code, comment or literal: line and block
/// comments (nested), strings with escapes, raw strings, char literals
/// (and lifetimes, which are code).
pub fn classify(text: &str) -> Vec<Class> {
    let b = text.as_bytes();
    let mut out = vec![Class::Code; b.len()];
    let mut i = 0;
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    // The `#`s of a raw string (`r"…"`, `r#"…"#`, `br"…"`) starting at `i`.
    let raw = |i: usize| -> Option<usize> {
        let starts = b[i] == b'r'
            && (i == 0 || !ident(b[i - 1]) || (b[i - 1] == b'b' && (i == 1 || !ident(b[i - 2]))));
        let hashes = b[i + 1..].iter().take_while(|c| **c == b'#').count();
        (starts && b.get(i + 1 + hashes) == Some(&b'"')).then_some(hashes)
    };
    while i < b.len() {
        let rest = &b[i..];
        if rest.starts_with(b"//") {
            let end = rest
                .iter()
                .position(|c| *c == b'\n')
                .map_or(b.len(), |n| i + n);
            out[i..end].fill(Class::Comment);
            i = end;
        } else if rest.starts_with(b"/*") {
            let (mut depth, mut j) = (0usize, i);
            while j < b.len() {
                if b[j..].starts_with(b"/*") {
                    depth += 1;
                    j += 2;
                } else if b[j..].starts_with(b"*/") {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            out[i..j].fill(Class::Comment);
            i = j;
        } else if let Some(hashes) = raw(i) {
            let close: Vec<u8> = std::iter::once(b'"')
                .chain(std::iter::repeat_n(b'#', hashes))
                .collect();
            let body = i + 2 + hashes;
            let end = b[body..]
                .windows(close.len())
                .position(|w| w == close.as_slice())
                .map_or(b.len(), |n| body + n + close.len());
            out[i..end].fill(Class::Literal);
            i = end;
        } else if b[i] == b'"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(b.len());
            out[i..end].fill(Class::Literal);
            i = end;
        } else if b[i] == b'\'' {
            // A char literal ('x', '\n', '\u{..}', a multi-byte char), or
            // a lifetime, which is code.
            let end = if rest.get(1) == Some(&b'\\') {
                rest[2..]
                    .iter()
                    .position(|c| *c == b'\'')
                    .map(|n| i + 2 + n + 1)
            } else {
                text[i + 1..]
                    .chars()
                    .next()
                    .map(|c| i + 1 + c.len_utf8())
                    .filter(|e| b.get(*e) == Some(&b'\''))
                    .map(|e| e + 1)
            };
            match end {
                Some(end) => {
                    out[i..end].fill(Class::Literal);
                    i = end;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    out
}

/// `text` with the bytes whose class `blank` names turned to spaces,
/// newlines kept, so offsets and line numbers stay `text`'s.
pub fn blanked(text: &str, classes: &[Class], blank: impl Fn(Class) -> bool) -> String {
    let bytes: Vec<u8> = text
        .bytes()
        .zip(classes)
        .map(|(c, k)| if blank(*k) && c != b'\n' { b' ' } else { c })
        .collect();
    String::from_utf8(bytes).expect("only whole characters are blanked")
}

/// The end of the bracket opened at `open` in `code` (comments and
/// literals blanked): one past its closing one.
pub fn close_of(code: &[u8], open: usize) -> usize {
    let (o, c) = match code[open] {
        b'{' => (b'{', b'}'),
        b'[' => (b'[', b']'),
        _ => (b'(', b')'),
    };
    let mut depth = 0usize;
    for (j, ch) in code.iter().enumerate().skip(open) {
        if *ch == o {
            depth += 1;
        } else if *ch == c {
            depth -= 1;
            if depth == 0 {
                return j + 1;
            }
        }
    }
    code.len()
}

/// The byte ranges of `code` (comments and literals blanked) that are test
/// code: each item under `#[cfg(test)]`, from the attribute to the end of
/// the item (its block, or its `;`), wherever in the file it stands.
pub fn test_ranges(code: &str) -> Vec<Range<usize>> {
    let attr = Regex::new(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]").unwrap();
    let b = code.as_bytes();
    let mut out: Vec<Range<usize>> = Vec::new();
    for m in attr.find_iter(code) {
        if out.iter().any(|r| r.contains(&m.start())) {
            continue;
        }
        let mut j = m.end();
        loop {
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            // Further attributes on the same item.
            if b.get(j) == Some(&b'#') {
                match b[j..].iter().position(|c| *c == b'[') {
                    Some(n) => j = close_of(b, j + n),
                    None => break,
                }
            } else {
                break;
            }
        }
        let end = b[j..]
            .iter()
            .position(|c| *c == b'{' || *c == b';')
            .map_or(b.len(), |n| {
                let at = j + n;
                if b[at] == b';' {
                    at + 1
                } else {
                    close_of(b, at)
                }
            });
        out.push(m.start()..end);
    }
    out
}

/// A source file's production code: `with_literals` has its comments and
/// test code blanked (for SQL), `code` its literals too (for calls).
pub struct Production {
    pub with_literals: String,
    pub code: String,
}

pub fn production(text: &str) -> Production {
    let classes = classify(text);
    let code = blanked(text, &classes, |k| k != Class::Code);
    let tests = test_ranges(&code);
    let out = |s: String| {
        let mut bytes = s.into_bytes();
        for r in &tests {
            for c in &mut bytes[r.clone()] {
                if *c != b'\n' {
                    *c = b' ';
                }
            }
        }
        String::from_utf8(bytes).expect("only whole characters are blanked")
    };
    Production {
        with_literals: out(blanked(text, &classes, |k| k == Class::Comment)),
        code: out(code),
    }
}

/// The 1-based line of byte `at` in `text`.
pub fn line_of(text: &str, at: usize) -> usize {
    text[..at].bytes().filter(|c| *c == b'\n').count() + 1
}
