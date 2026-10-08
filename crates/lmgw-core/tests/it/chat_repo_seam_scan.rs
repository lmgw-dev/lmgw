//! Every message write of a stored thread goes through the Chat's
//! repository seam (`web::chat_repo`), which marks the thread for the
//! dashboard's `chat` frame: that mark is how a Chat page open on the thread
//! hears that another writer changed its messages (client-apps design §3.6,
//! the owner's finding of 2026-10-08, review CL-6). A write that calls the
//! store past the seam changes the messages with no page hearing of it.
//!
//! This scan refuses it in every crate of the workspace:
//! - **The store's message writes** are derived, not listed: every function
//!   of `lmgw-core`'s store that writes `chat_messages` in its own SQL, or
//!   calls one that does. Outside the store (`src/store.rs`, `src/store/`)
//!   only `web/chat_repo.rs` and test code may name them.
//! - **Inside the seam**, each call of one sits inside a `marked(` call, so
//!   a new `ChatRepo` method cannot write without marking (review CL-17).
//!   The one exception, [`NEW_THREAD_WRITES`], makes a new thread.
//! - **No SQL of its own** that writes `chat_messages` anywhere outside the
//!   store, the seam included (CL-17).
//!
//! Test code is left out by where it is (a `tests` directory, a `*_tests`
//! file, a module declared under `#[cfg(test)]`) and by what it is: every
//! item under `#[cfg(test)]`, wherever in a file it stands, to its end;
//! the production code after it is scanned (CL-17). Comments are never
//! read as code, nor string literals as calls.
//! `web/chat_repo/marks_tests.rs` is the other half: each of the seam's
//! writes marks its thread.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::support::rust_source::{collect_rs, fn_header, line_of, production};

/// The store's message writes the seam calls outside `marked(`, with why.
const NEW_THREAD_WRITES: &[(&str, &str)] = &[(
    "insert_kept_chat_thread",
    "a kept temporary chat becomes a new stored thread: its thread.created record names it, \
     and a page reads a new thread whole",
)];

/// SQL that writes `chat_messages`.
fn writes_messages() -> Regex {
    Regex::new(r"(?i)\b(INSERT\s+(OR\s+\w+\s+)?INTO|UPDATE|DELETE\s+FROM|REPLACE\s+INTO)\s+chat_messages\b")
        .unwrap()
}

/// The files of the out-of-line modules `file` declares under
/// `#[cfg(test)]` (`#[cfg(test)] mod name;`): `name.rs` or `name/mod.rs`
/// beside `file`'s own module directory.
fn test_modules_of(file: &Path, lines: &[&str]) -> Vec<PathBuf> {
    let decl = Regex::new(r"^\s*(pub(\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;").unwrap();
    let stem = file.file_stem().unwrap().to_string_lossy();
    let parent = file.parent().unwrap();
    let dir = if matches!(stem.as_ref(), "mod" | "lib" | "main") {
        parent.to_path_buf()
    } else {
        parent.join(stem.as_ref())
    };
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        let Some(next) = lines[i + 1..]
            .iter()
            .find(|l| !l.trim().is_empty() && !l.trim().starts_with("#["))
        else {
            continue;
        };
        if let Some(c) = decl.captures(next) {
            let name = &c[3];
            out.push(dir.join(format!("{name}.rs")));
            out.push(dir.join(name));
        }
    }
    out
}

/// Whether `file` is test code by where it is: a `tests` directory, a
/// `tests.rs`, a `*_tests` file or directory, or a module declared under
/// `#[cfg(test)]` (`test_mods`).
fn is_test_file(file: &Path, test_mods: &[PathBuf]) -> bool {
    if test_mods.iter().any(|m| file.starts_with(m)) {
        return true;
    }
    file.components().any(|c| {
        let c = c.as_os_str().to_string_lossy();
        let c = c.trim_end_matches(".rs");
        c == "tests" || c.ends_with("_tests") || c == "benches"
    })
}

/// The store's functions in `text` (production code only, comments left
/// out): each one's name, whether it is `pub` in any form, and its body.
fn functions(text: &str) -> Vec<(String, bool, String)> {
    let header = fn_header();
    let prod = production(text).with_literals;
    let mut out: Vec<(String, bool, String)> = Vec::new();
    for line in prod.lines() {
        if let Some(c) = header.captures(line) {
            out.push((c[6].to_string(), c.get(1).is_some(), String::new()));
        }
        if let Some((_, _, body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    out
}

/// The message writes among `fns`: a function whose own SQL writes
/// `chat_messages`, or that calls one that does — to a fixed point.
fn message_writers(fns: &[(String, bool, String)]) -> BTreeMap<String, bool> {
    let sql = writes_messages();
    let mut writers: BTreeMap<String, bool> = BTreeMap::new();
    for (name, public, body) in fns {
        if sql.is_match(body) {
            writers.insert(name.clone(), *public);
        }
    }
    loop {
        let known: Vec<String> = writers.keys().cloned().collect();
        let calls: Vec<Regex> = known
            .iter()
            .map(|n| Regex::new(&format!(r"\b{n}\s*\(")).unwrap())
            .collect();
        let mut grew = false;
        for (name, public, body) in fns {
            if writers.contains_key(name) {
                continue;
            }
            // The body less its own header line, which names the function.
            let rest = body.split_once('\n').map_or("", |(_, r)| r);
            if calls.iter().any(|c| c.is_match(rest)) {
                writers.insert(name.clone(), *public);
                grew = true;
            }
        }
        if !grew {
            return writers;
        }
    }
}

/// The calls whose parentheses enclose byte `at` of `code` (comments and
/// literals blanked), innermost last, each by the name before its `(`.
fn enclosing_calls(code: &str, at: usize) -> Vec<String> {
    let b = code.as_bytes();
    let mut stack: Vec<String> = Vec::new();
    for (i, c) in b.iter().enumerate().take(at) {
        match c {
            b'(' => {
                let name_end = i;
                let name_start = b[..name_end]
                    .iter()
                    .rposition(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
                    .map_or(0, |p| p + 1);
                stack.push(code[name_start..name_end].to_string());
            }
            b')' => {
                stack.pop();
            }
            _ => {}
        }
    }
    stack
}

/// What a non-store file does past the seam: each refusal as
/// `line: what`. `seam` is `web/chat_repo.rs`, which may call `writers`
/// inside `marked(` (or one of [`NEW_THREAD_WRITES`]); any other file may
/// not name them at all; no file may write `chat_messages` in SQL of its
/// own.
fn past_the_seam(text: &str, writers: &BTreeSet<&str>, seam: bool) -> Vec<String> {
    let prod = production(text);
    let mut refused = Vec::new();
    for m in writes_messages().find_iter(&prod.with_literals) {
        refused.push(format!(
            "{}: SQL of its own: {}",
            line_of(text, m.start()),
            m.as_str().split_whitespace().collect::<Vec<_>>().join(" ")
        ));
    }
    for name in writers {
        let re = Regex::new(&format!(r"\b{name}\b")).unwrap();
        for m in re.find_iter(&prod.code) {
            let line = line_of(text, m.start());
            if !seam {
                refused.push(format!("{line}: store::{name}"));
                continue;
            }
            let exempt = NEW_THREAD_WRITES.iter().any(|(n, _)| n == name);
            if !exempt
                && !enclosing_calls(&prod.code, m.start())
                    .iter()
                    .any(|c| c == "marked")
            {
                refused.push(format!("{line}: store::{name} outside `marked(`"));
            }
        }
    }
    refused
}

#[test]
fn the_scan_finds_a_message_write_and_what_calls_it() {
    let text = r#"
/// Inserts a message.
pub async fn add(pool: &SqlitePool) -> DbResult<i64> {
    sqlx::query("INSERT INTO chat_messages (thread_id) VALUES (?1)")
}

fn helper(tx: &mut Tx) {
    sqlx::query(
        "UPDATE chat_messages
         SET content = ?1",
    )
}

pub(crate) async fn wrapper(pool: &SqlitePool) {
    helper(&mut tx).await
}

pub async fn reads(pool: &SqlitePool) {
    sqlx::query("SELECT * FROM chat_messages")
}

pub async fn other(pool: &SqlitePool) {
    sqlx::query("UPDATE chat_messages_fts SET x = 1")
}

// fn commented() { sqlx::query("DELETE FROM chat_messages") }

#[cfg(test)]
mod tests {
    async fn fixture() {
        sqlx::query("DELETE FROM chat_messages")
    }
}

/// After a test module mid-file: production code all the same.
pub async fn after(pool: &SqlitePool) {
    sqlx::query("DELETE FROM chat_messages WHERE id = ?1")
}
"#;
    let writers = message_writers(&functions(text));
    assert_eq!(
        writers.into_iter().collect::<Vec<_>>(),
        vec![
            ("add".to_string(), true),
            ("after".to_string(), true),
            ("helper".to_string(), false),
            ("wrapper".to_string(), true),
        ]
    );
    let lines = ["#[cfg(test)]", "mod seam_tests;", "fn f() {}"];
    let mods = test_modules_of(Path::new("/w/src/web/chat_turn.rs"), &lines);
    assert!(is_test_file(
        Path::new("/w/src/web/chat_turn/seam_tests/spoken.rs"),
        &mods
    ));
    assert!(!is_test_file(
        Path::new("/w/src/web/chat_turn/save.rs"),
        &mods
    ));
}

/// Review CL-17: what the scan reads as test code, comment and literal, and
/// what it refuses outside the store and inside the seam.
#[test]
fn the_scan_reads_test_code_comments_and_literals_for_what_they_are() {
    let text = r##"
fn a() { let s = "a { brace ) in a string"; let c = '{'; let r = r#"raw " { "#; }
// a comment with a { brace
#[cfg(test)]
#[allow(dead_code)]
mod tests {
    fn t() { let x = "}"; store::append_chat_reply(); }
}
fn b<'a>(x: &'a str) { store::append_chat_reply(x) }
#[cfg(test)]
mod more;
fn c() {}
"##;
    let prod = production(text);
    assert!(prod.code.contains("fn a()") && prod.code.contains("fn b<'a>"));
    assert!(prod.code.contains("fn c()"), "after `mod more;`");
    assert!(!prod.code.contains("fn t()") && !prod.code.contains("comment"));
    assert!(
        !prod.code.contains("brace ) in"),
        "literals are blanked in code"
    );
    assert!(
        prod.with_literals.contains("brace ) in"),
        "and kept for SQL"
    );
    assert_eq!(prod.code.len(), text.len(), "offsets are the text's");
    let writers: BTreeSet<&str> = ["append_chat_reply", "insert_kept_chat_thread"].into();
    // Outside the seam: the call after the test module, and raw SQL.
    let outside = format!("{text}\nfn d() {{ sqlx::query(\"DELETE  FROM\n chat_messages\") }}\n");
    let refused = past_the_seam(&outside, &writers, false);
    assert_eq!(
        refused,
        vec![
            "14: SQL of its own: DELETE FROM chat_messages".to_string(),
            "9: store::append_chat_reply".to_string(),
        ]
    );
    // Inside the seam: only a call outside `marked(` is refused.
    let seam = r#"
fn ok(s: &S) -> R { match x { Db => marked(s, id, store::append_chat_reply(&s.db).await) } }
fn nested(s: &S) -> R { marked(s, id, f(g(store::append_chat_reply(&s.db)))) }
fn bad(s: &S) -> R { let r = store::append_chat_reply(&s.db).await; marked(s, id, r) }
fn kept(s: &S) -> R { store::insert_kept_chat_thread(&s.db).await }
fn sql(s: &S) { sqlx::query("UPDATE chat_messages SET content = ''") }
"#;
    assert_eq!(
        past_the_seam(seam, &writers, true),
        vec![
            "6: SQL of its own: UPDATE chat_messages".to_string(),
            "4: store::append_chat_reply outside `marked(`".to_string(),
        ]
    );
}

#[test]
fn no_message_write_bypasses_the_chat_repository() {
    let core = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = core.parent().unwrap().parent().unwrap();
    let store = core.join("src/store");
    let store_root = core.join("src/store.rs");
    let seam = core.join("src/web/chat_repo.rs");

    let mut store_files = vec![store_root.clone()];
    collect_rs(&store, &mut store_files);
    let mut fns = Vec::new();
    for f in &store_files {
        fns.extend(functions(&std::fs::read_to_string(f).unwrap()));
    }
    let writers = message_writers(&fns);
    // The derivation still finds the ones the seam wraps: if it stopped
    // working, this says so rather than passing on an empty list.
    for known in [
        "append_chat_message",
        "append_user_message_with_attachments",
        "append_user_message_with_kb_refs",
        "append_user_message_with_voice",
        "append_chat_reply",
        "continue_chat_reply",
        "update_chat_message",
        "rewrite_chat_user_message",
        "set_chat_message_knowledge",
        "delete_chat_message",
        "set_chat_message_voice",
        "cut_chat_reply",
        "truncate_chat_messages",
        "insert_kept_chat_thread",
    ] {
        assert!(
            writers.get(known) == Some(&true),
            "the scan no longer finds store::{known} as a message write: {writers:?}"
        );
    }
    for (name, _) in NEW_THREAD_WRITES {
        assert!(writers.contains_key(*name), "{name} is no message write");
    }
    let public: BTreeSet<&str> = writers
        .iter()
        .filter(|(_, p)| **p)
        .map(|(n, _)| n.as_str())
        .collect();

    let mut files = Vec::new();
    collect_rs(root, &mut files);
    // This file spells what it looks for.
    files.retain(|f| !f.ends_with("tests/it/chat_repo_seam_scan.rs"));
    assert!(
        files.len() > 500,
        "the scan found only {} files under {}, which means it stopped working",
        files.len(),
        root.display()
    );
    let texts: Vec<(PathBuf, String)> = files
        .into_iter()
        .map(|f| {
            let t = std::fs::read_to_string(&f).unwrap();
            (f, t)
        })
        .collect();
    let test_mods: Vec<PathBuf> = texts
        .iter()
        .flat_map(|(f, t)| test_modules_of(f, &t.lines().collect::<Vec<_>>()))
        .collect();

    let mut refused = Vec::new();
    let mut seam_calls = 0;
    for (file, text) in &texts {
        let in_store = file.starts_with(&store) || *file == store_root;
        if in_store || is_test_file(file, &test_mods) {
            continue;
        }
        if *file == seam {
            seam_calls = public
                .iter()
                .map(|n| {
                    let re = Regex::new(&format!(r"\bstore::{n}\s*\(")).unwrap();
                    re.find_iter(&production(text).code).count()
                })
                .sum();
        }
        for r in past_the_seam(text, &public, *file == seam) {
            refused.push(format!("{}:{r}", file.display()));
        }
    }
    assert!(
        seam_calls >= 13,
        "the scan saw {seam_calls} store message writes in the seam, which means it stopped \
         reading it"
    );
    assert!(
        refused.is_empty(),
        "a message write past the Chat's repository seam: no Chat page open on the thread \
         hears of it. Write through web::chat_repo::ChatRepo, whose message writes mark the \
         thread (`marked`):\n{}",
        refused.join("\n")
    );
}
