//! Every transaction that writes begins with `store::begin_write` (or
//! quickdoc-core's), which takes the write lock at its BEGIN (`BEGIN
//! IMMEDIATE`), and clippy.toml refuses the sqlx calls that begin a
//! deferred one. Two spellings of a deferred BEGIN pass that lint, since
//! the statement is a string to it: `begin_with` with a plain or `DEFERRED`
//! BEGIN, and a BEGIN written by hand into a query (the begin-write
//! re-check's R-6). This scan refuses both in every crate of the workspace.
//!
//! The lint's own escape applies: a read-only snapshot marked
//! `#[allow(clippy::disallowed_methods)]` on the line before, with its
//! reason, may begin deferred.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, build output and hidden directories left
/// out.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
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

/// Whether `sql` begins a deferred transaction: a BEGIN that is neither
/// IMMEDIATE nor EXCLUSIVE, in any case, with or without `;`.
fn begins_deferred(sql: &str) -> bool {
    let words: Vec<String> = sql
        .trim()
        .trim_end_matches(';')
        .split_whitespace()
        .map(str::to_ascii_uppercase)
        .collect();
    match words.split_first() {
        Some((begin, rest)) if begin == "BEGIN" => !matches!(
            rest.first().map(String::as_str),
            Some("IMMEDIATE" | "EXCLUSIVE")
        ),
        _ => false,
    }
}

/// The string literals on `line`, as the text between its quotes.
fn literals(line: &str) -> impl Iterator<Item = &str> {
    line.split('"').skip(1).step_by(2)
}

/// What one line holds that begins deferred, said for the failure.
fn deferred_begins(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let call = "begin_with(";
    for (at, _) in line.match_indices(call) {
        let rest = line[at + call.len()..].trim_start();
        match rest.strip_prefix('"').and_then(|r| r.split('"').next()) {
            Some(sql) if !begins_deferred(sql) => {}
            Some(sql) => found.push(format!("begin_with with {sql:?}")),
            None => found.push(
                "begin_with without its statement as a literal on the same line, which this \
                 scan cannot read"
                    .to_string(),
            ),
        }
    }
    for sql in literals(line).filter(|sql| begins_deferred(sql)) {
        found.push(format!("the statement {sql:?}"));
    }
    found
}

#[test]
fn the_scan_knows_a_deferred_begin() {
    for sql in [
        "BEGIN",
        "begin;",
        "BEGIN DEFERRED",
        "BEGIN TRANSACTION",
        " Begin Deferred Transaction ",
    ] {
        assert!(begins_deferred(sql), "{sql:?}");
    }
    for sql in [
        "BEGIN IMMEDIATE",
        "begin exclusive;",
        "SELECT 1",
        "-----BEGIN KEY",
        "",
    ] {
        assert!(!begins_deferred(sql), "{sql:?}");
    }
    let q = '"';
    assert_eq!(
        deferred_begins(&format!("pool.begin_with({q}BEGIN IMMEDIATE{q})")).len(),
        0
    );
    assert_eq!(
        deferred_begins(&format!("pool.begin_with({q}BEGIN{q})")).len(),
        2
    );
    assert_eq!(deferred_begins("pool.begin_with(sql)").len(), 1);
    assert_eq!(
        deferred_begins(&format!("sqlx::query({q}BEGIN DEFERRED{q})")).len(),
        1
    );
}

#[test]
fn no_transaction_begins_deferred_where_the_lint_cannot_see() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    collect_rs(crates, &mut files);
    // This file spells what it looks for.
    files.retain(|f| !f.ends_with("tests/it/store_begin_scan.rs"));
    assert!(
        files.len() > 500,
        "the scan found only {} files under {}, which means it stopped working",
        files.len(),
        crates.display()
    );
    let mut refused = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let allowed = i > 0 && lines[i - 1].contains("#[allow(clippy::disallowed_methods)]");
            if allowed {
                continue;
            }
            for what in deferred_begins(line) {
                refused.push(format!("{}:{}: {what}", file.display(), i + 1));
            }
        }
    }
    assert!(
        refused.is_empty(),
        "a transaction that begins deferred, where clippy.toml's lint cannot see it. A write \
         transaction begins with store::begin_write (BEGIN IMMEDIATE); a read-only snapshot \
         may begin deferred under #[allow(clippy::disallowed_methods)] with its reason:\n{}",
        refused.join("\n")
    );
}
