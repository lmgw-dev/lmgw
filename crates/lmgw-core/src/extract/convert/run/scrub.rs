//! A tool's stderr as the caller may read it (security round B, L1): the
//! lines stay (pandoc's parse error, ffmpeg's reason are the caller's
//! answer about its own file), but every absolute path in them is cut to
//! its file name behind `…/`, and heap addresses (`0x563f1426bcc0`) become
//! `0x…`. A tool is given paths of the gateway's own (a run directory under
//! the data dir, whose name holds the gateway's pid; a profile; a spool),
//! and none of them is the caller's to know. The gateway's log keeps where
//! the tool is and where it ran (`run.rs`).
//!
//! The paths the gateway knows are cut first, word for word ([`known`]:
//! the run's directory, lmgw's data dirs, the home directory, the temp
//! directory), so a path with a space in it (`…/Application Support/…`) or
//! a Windows path (`C:\…`) under one of them is cut too; then any other
//! absolute path by its shape.

use std::path::Path;

/// The shortest run of hex digits after `0x` taken for an address.
const ADDRESS_DIGITS: usize = 8;

/// Whether a path may start after `c` (the start of a line, a space, a
/// quote, a bracket, `file:`'s colon, `=`).
fn opens_path(c: Option<char>) -> bool {
    c.is_none_or(|c| c.is_whitespace() || "'\"`([{<=:,".contains(c))
}

/// Whether `c` ends a path token.
fn ends_path(c: char) -> bool {
    c.is_whitespace() || "'\"`)]}>,;".contains(c)
}

/// The directories whose paths are cut word for word, longest first: the
/// run's `dir` when it has one, lmgw's data dirs, the home directory and
/// the temp directory, each as given and resolved.
pub fn known(dir: Option<&Path>) -> Vec<String> {
    let mut dirs: Vec<std::path::PathBuf> = dir.map(Path::to_path_buf).into_iter().collect();
    dirs.extend(super::super::confine::own_data_dirs());
    dirs.extend(std::env::var_os("HOME").map(std::path::PathBuf::from));
    dirs.push(std::env::temp_dir());
    let mut out: Vec<String> = Vec::new();
    for d in dirs {
        let resolved = std::fs::canonicalize(&d).ok();
        for p in std::iter::once(d).chain(resolved) {
            let s = p.to_string_lossy();
            let s = s.trim_end_matches(['/', '\\']);
            // A root (`/`, `C:`) is no directory to cut by.
            if s.len() > 3 && Path::new(s).is_absolute() && !out.iter().any(|o| o == s) {
                out.push(s.to_string());
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.len()));
    out
}

/// `text` with the paths under `prefixes` ([`known`]) cut to `…/` and
/// their file name, a path ending at the first character that ends one
/// ([`ends_path`]) after the prefix.
fn cut_known(text: &str, prefixes: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'scan: while i < text.len() {
        let before = text[..i].chars().next_back();
        if opens_path(before) {
            for p in prefixes {
                let Some(after) = text[i..].strip_prefix(p.as_str()) else {
                    continue;
                };
                // The path goes on to the first character that ends one;
                // the prefix must end at a separator or where it does.
                let rest = &after[..after.find(ends_path).unwrap_or(after.len())];
                // A path's trailing `.` or `:` is the sentence's.
                let trimmed = rest.trim_end_matches(['.', ':']);
                if !(trimmed.is_empty() || trimmed.starts_with(['/', '\\'])) {
                    continue;
                }
                let rest_len = trimmed.len();
                let last = trimmed
                    .trim_end_matches(['/', '\\'])
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or("");
                out.push('…');
                if !last.is_empty() {
                    out.push('/');
                    out.push_str(last);
                }
                i += p.len() + rest_len;
                continue 'scan;
            }
        }
        let c = text[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// `text` with its absolute paths cut and its addresses dropped (module
/// doc), the paths under the process's known directories first.
pub fn scrub(text: &str) -> String {
    scrub_in(text, None)
}

/// [`scrub`] for a run in `dir`, whose paths are cut word for word too.
pub fn scrub_in(text: &str, dir: Option<&Path>) -> String {
    let text = cut_known(text, &known(dir));
    let text = text.as_str();
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let before = i.checked_sub(1).map(|j| chars[j]);
        if c == '/' && opens_path(before) {
            let mut end = i;
            while end < chars.len() && !ends_path(chars[end]) {
                end += 1;
            }
            // A path's trailing `.` or `:` is the sentence's.
            while end > i + 1 && matches!(chars[end - 1], '.' | ':') {
                end -= 1;
            }
            let token: String = chars[i..end].iter().collect();
            let inner = token.trim_end_matches('/');
            if inner.matches('/').count() >= 2 {
                let last = inner.rsplit('/').next().unwrap_or("");
                out.push_str("…/");
                out.push_str(last);
            } else {
                out.push_str(&token);
            }
            i = end;
            continue;
        }
        if c == '0'
            && chars.get(i + 1) == Some(&'x')
            && !before.is_some_and(|b| b.is_alphanumeric())
        {
            let digits = chars[i + 2..]
                .iter()
                .take_while(|d| d.is_ascii_hexdigit())
                .count();
            if digits >= ADDRESS_DIGITS {
                out.push_str("0x…");
                i += 2 + digits;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_s_paths_and_addresses_are_cut() {
        let said = "[in#0 @ 0x563f1426bcc0] 0x00 at pos 20 (0x14) invalid as first byte of an \
                    EBML number\nError opening input file \
                    file:/tmp/lmgw-dev-49ui/convert-spool/ff-3577151-dJckbn/in.media.";
        let s = scrub(said);
        assert_eq!(
            s,
            "[in#0 @ 0x…] 0x00 at pos 20 (0x14) invalid as first byte of an EBML number\nError \
             opening input file file:…/in.media."
        );
        assert!(!s.contains("lmgw-dev") && !s.contains("3577151"), "{s}");
    }

    #[test]
    fn other_tools_paths_go_and_their_words_stay() {
        assert_eq!(
            scrub("cat: /srv/example/run/secret.txt: Permission denied"),
            "cat: …/secret.txt: Permission denied"
        );
        assert_eq!(
            scrub("Error: source file could not be loaded (file:///home/someone.example/x/in.doc)"),
            "Error: source file could not be loaded (file:…/in.doc)"
        );
        assert_eq!(scrub("'/var/tmp/a/b' and \"/x/y/z\""), "'…/b' and \"…/z\"");
        // A path of one step, a fraction, a relative path, a short number.
        assert_eq!(
            scrub("/dev and 1/2 and out/in.docx, 0x14"),
            "/dev and 1/2 and out/in.docx, 0x14"
        );
        assert_eq!(scrub("in /tmp/dir/."), "in …/dir.");
        assert_eq!(
            scrub("pandoc: line 3, column 5"),
            "pandoc: line 3, column 5"
        );
        assert_eq!(scrub(""), "");
    }

    /// The review of round B, N1: a known directory with a space in its
    /// name, and a Windows one, are cut whole.
    #[test]
    fn known_directories_are_cut_word_for_word() {
        let mac = "/Users/someone.example/Library/Application Support/lmgw".to_string();
        let win = r"C:\Users\someone.example\AppData\Roaming\lmgw".to_string();
        let prefixes = vec![mac.clone(), win.clone()];
        let said = format!(
            "Error opening input file file:{mac}/convert-spool/ff-123-abc/in.media.\n\
             cannot open {win}\\convert-spool\\lo-9-x\\in.doc: denied"
        );
        let s = cut_known(&said, &prefixes);
        assert_eq!(
            s,
            "Error opening input file file:…/in.media.\ncannot open …/in.doc: denied"
        );
        assert!(!s.contains("Application") && !s.contains("123"), "{s}");
        // A prefix only where a path starts, and only up to a separator.
        assert_eq!(
            cut_known("/srv/lmgwx/a", &["/srv/lmgw".into()]),
            "/srv/lmgwx/a"
        );
        assert_eq!(
            cut_known("x/srv/lmgw/a", &["/srv/lmgw".into()]),
            "x/srv/lmgw/a"
        );
        assert_eq!(cut_known("in /srv/lmgw.", &["/srv/lmgw".into()]), "in ….");
        // The run's own directory, through `scrub_in`.
        let run = std::path::Path::new("/srv/example run/ff-1");
        assert_eq!(
            scrub_in("open /srv/example run/ff-1/out.wav failed", Some(run)),
            "open …/out.wav failed"
        );
    }
}
