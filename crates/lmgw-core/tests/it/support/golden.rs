//! Golden JSON files under `tests/fixtures/<dir>/<name>.json`, for the egress
//! corpora (`egress_golden.rs`, `route_golden.rs`; llama-egress design §9.1,
//! §9.2).
//!
//! The convention is `chat_golden.rs`'s: `LMGW_BLESS=1` rewrites a file from
//! what the code produces now (the git diff then shows what changed), and
//! without it a missing or different file fails — here with a line diff
//! rather than both files whole, since a fixture is a few hundred lines and a
//! change is usually one key.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

/// Header names whose values are secrets: recorded as `<set>`.
const SECRET_HEADERS: [&str; 4] = ["authorization", "x-api-key", "api-key", "x-goog-api-key"];

pub fn blessing() -> bool {
    std::env::var_os("LMGW_BLESS").is_some()
}

fn dir_path(dir: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
}

/// `value` as a fixture's bytes: pretty JSON (keys sorted, `serde_json::Map`
/// being a `BTreeMap` here) and a final newline.
pub fn to_fixture(value: &Value) -> String {
    let mut s = serde_json::to_string_pretty(value).unwrap();
    s.push('\n');
    s
}

/// Compare `got` with `tests/fixtures/<dir>/<name>.json`, or write it there
/// under `LMGW_BLESS=1`. `Err` is the readable reason.
pub fn check(dir: &str, name: &str, got: &str) -> Result<(), String> {
    let file = dir_path(dir).join(format!("{name}.json"));
    if blessing() {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let unchanged = std::fs::read_to_string(&file).is_ok_and(|want| want == got);
        if !unchanged {
            std::fs::write(&file, got).unwrap();
        }
        return Ok(());
    }
    match std::fs::read_to_string(&file) {
        Err(e) => Err(format!(
            "{}: {e} — run with LMGW_BLESS=1 to capture it",
            file.display()
        )),
        Ok(want) if want == got => Ok(()),
        Ok(want) => Err(format!(
            "{dir}/{name}.json changed (- fixture, + now):\n{}",
            diff(&want, got)
        )),
    }
}

/// The fixtures in `dir` no case wrote — a renamed or dropped case must not
/// leave its old file behind looking guarded. Under `LMGW_BLESS=1` they are
/// deleted instead.
pub fn stale(dir: &str, written: &BTreeSet<String>) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir_path(dir)) else {
        return vec![];
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let Some(stem) = p
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if written.contains(stem) {
            continue;
        }
        if blessing() {
            std::fs::remove_file(&p).unwrap();
        } else {
            out.push(format!(
                "{dir}/{stem}.json is written by no case — delete it, or run with LMGW_BLESS=1"
            ));
        }
    }
    out.sort();
    out
}

/// Run every check in `checks` and fail once with all of them, so one run
/// shows every fixture that moved, not only the first.
pub fn assert_all(what: &str, total: usize, failures: Vec<String>) {
    assert!(
        failures.is_empty(),
        "{} of {total} {what} fixtures differ:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Header map → `{name: value}`, sorted, secrets as `<set>`, without the
/// names in `skip` (transport headers whose values carry a port or a length).
pub fn headers(pairs: impl IntoIterator<Item = (String, String)>, skip: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for (name, value) in pairs {
        let name = name.to_ascii_lowercase();
        if skip.contains(&name.as_str()) {
            continue;
        }
        let value = if SECRET_HEADERS.contains(&name.as_str()) {
            "<set>".to_string()
        } else {
            value
        };
        out.insert(name, Value::String(value));
    }
    Value::Object(out)
}

/// A posted body: its exact bytes as a string — what the corpus guards —
/// and, beside them, the same parsed, so a fixture's diff shows which key
/// moved.
pub fn body(bytes: &[u8]) -> (Value, Value) {
    let text = String::from_utf8(bytes.to_vec()).expect("every egress body is UTF-8 JSON");
    let parsed = serde_json::from_str(&text).unwrap_or(Value::Null);
    (Value::String(text), parsed)
}

/// A line diff of `want` and `got`: the longest common subsequence, with two
/// lines of context around each change.
pub fn diff(want: &str, got: &str) -> String {
    let a: Vec<&str> = want.lines().collect();
    let b: Vec<&str> = got.lines().collect();
    let (n, m) = (a.len(), b.len());
    // lcs[i][j]: the LCS length of a[i..] and b[j..].
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    // (tag, line): ' ' kept, '-' only in want, '+' only in got.
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    const CONTEXT: usize = 2;
    let changed: Vec<usize> = (0..ops.len()).filter(|&k| ops[k].0 != ' ').collect();
    let mut out = String::new();
    let mut last: Option<usize> = None;
    for &k in &changed {
        let from = k.saturating_sub(CONTEXT);
        let start = match last {
            Some(l) if from <= l + 1 => l + 1,
            Some(_) => {
                out.push_str("  ...\n");
                from
            }
            None => from,
        };
        let end = (k + CONTEXT).min(ops.len() - 1);
        for (t, line) in &ops[start..=end] {
            out.push_str(&format!("{t} {line}\n"));
        }
        last = Some(end);
    }
    out
}
