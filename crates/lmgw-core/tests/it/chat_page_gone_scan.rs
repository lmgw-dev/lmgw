//! The Chat page's delete-elsewhere path sends nothing (reviews CL-14,
//! CF-7). When another writer deletes the open conversation, lmgw-ui's
//! `chat_sync::Gone::run` decides on plain values and sets the page's
//! signals: a draft waits for a new chat that only the owner's Send makes,
//! and without one the page falls through as its own delete does. A
//! request at the delete (a create, as before CL-14) would make a chat for a
//! draft the owner may never send.
//!
//! The scan reads `pages/chat_sync/gone.rs` and every file under
//! `pages/chat_sync/gone/` as code: comments, literals and test code are
//! left out (`support::rust_source`), so a comment never trips it. It
//! refuses:
//! - **A request outside [`REQUESTERS`].** A function requests when its
//!   body calls the API, spawns a task, starts a conversation or a chat
//!   in a folder, or runs a callback ([`request_tokens`]), or calls a
//!   function of these files that requests, to a fixed point. Only the
//!   functions that make, open and send the rescued draft's chat at Send
//!   may. A request moved into a helper is the helper's, and refused; so is
//!   `Gone::run` once it calls one.
//! - **A `Gone` that could request without a call of its own.** It holds no
//!   `Scope`, and its one `Callback` is `fall_through`, the page's own
//!   fall-through when no draft waits. That callback's `.run(` is the one
//!   `run` may make.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;

use regex::Regex;

use crate::support::rust_source::{close_of, collect_rs, fn_header, line_of, production};

/// The functions that may request, with why.
const REQUESTERS: &[(&str, &str)] = &[
    (
        "new_plain",
        "Send's New chat in no folder: the folder is gone, or there was none",
    ),
    (
        "make",
        "Send makes the chat as the folder's own New chat does",
    ),
    (
        "send_new",
        "the owner's Send: makes the chat, reads the list, opens it",
    ),
    (
        "rescued_open",
        "the made chat's open landed: set the model picked, then the page's send",
    ),
];

/// What a request looks like in code: the API client, a spawned task, a
/// fetch, a resource or an action, the folder module's conversation and
/// chat starts, a callback run.
fn request_tokens() -> Regex {
    Regex::new(
        r"\bapi::[a-z_][A-Za-z0-9_]*|\bspawn[A-Za-z0-9_]*\s*\(|\bfetch\s*\(|\bgloo_net::|\b(Local)?Resource::new\b|\bAction::new\b|\bstart_conversation\b|\bstart_chat_in\b|\.\s*run\s*\(",
    )
    .unwrap()
}

/// A function and the byte range of its body in its file's code.
struct Func {
    file: String,
    name: String,
    body: Range<usize>,
}

/// The functions of `code` (comments and literals blanked): each header,
/// and its body from the first `{` after it. A declaration (`;` first) has
/// none.
fn functions(file: &str, code: &str) -> Vec<Func> {
    let header = fn_header();
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    for line in code.split_inclusive('\n') {
        if let Some(c) = header.captures(line) {
            let from = at + c.get(0).unwrap().end();
            if let Some(n) = b[from..].iter().position(|x| *x == b'{' || *x == b';') {
                if b[from + n] == b'{' {
                    out.push(Func {
                        file: file.to_string(),
                        name: c[6].to_string(),
                        body: from + n..close_of(b, from + n),
                    });
                }
            }
        }
        at += line.len();
    }
    out
}

/// The fields of `struct name` in `code`, each as its name and type.
fn struct_fields(code: &str, name: &str) -> Option<Vec<(String, String)>> {
    let m = Regex::new(&format!(r"\bstruct\s+{name}\b[^{{;]*\{{"))
        .unwrap()
        .find(code)?;
    let open = m.end() - 1;
    let body = &code[open + 1..close_of(code.as_bytes(), open) - 1];
    // Commas inside a type's brackets (`RwSignal<Option<(i64, Stats)>>`) do
    // not end a field.
    let (mut fields, mut cur, mut depth) = (Vec::new(), String::new(), 0i32);
    for ch in body.chars() {
        match ch {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                fields.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    fields.push(cur);
    let vis = Regex::new(r"^(#\[[^\]]*\]\s*)*(pub(\([^)]*\))?\s+)?").unwrap();
    Some(
        fields
            .iter()
            .map(|f| vis.replace(f.trim(), "").to_string())
            .filter_map(|f| {
                let (n, t) = f.split_once(':')?;
                Some((n.trim().to_string(), t.split_whitespace().collect()))
            })
            .collect(),
    )
}

/// `code` with its `use` declarations blanked, offsets kept: an import
/// names a function, it does not call it.
fn without_uses(code: &str) -> String {
    let uses = Regex::new(r"(?m)^[ \t]*(pub(\([^)]*\))?[ \t]+)?use\s[^;]*;").unwrap();
    let mut out = code.to_string();
    for m in uses.find_iter(code) {
        let blank: String = m
            .as_str()
            .chars()
            .map(|c| if c == '\n' { c } else { ' ' })
            .collect();
        out.replace_range(m.range(), &blank);
    }
    out
}

/// What the scan read.
struct Found {
    /// The functions that request, each with the first reason found.
    requesters: BTreeMap<String, String>,
    /// Requests in no function at all.
    loose: Vec<String>,
    /// `Gone`'s fields; `None` when there is no `struct Gone`.
    gone_fields: Option<Vec<(String, String)>>,
    /// `Gone::run` decides through `on_gone`, in code.
    run_decides: bool,
}

/// Read `files` (a name and its text each).
fn read(files: &[(String, String)]) -> Found {
    let tokens = request_tokens();
    let fall_through_run = Regex::new(r"\bfall_through\s*$").unwrap();
    let mut funcs = Vec::new();
    let mut codes = BTreeMap::new();
    let mut found = Found {
        requesters: BTreeMap::new(),
        loose: Vec::new(),
        gone_fields: None,
        run_decides: false,
    };
    for (file, text) in files {
        let code = without_uses(&production(text).code);
        funcs.extend(functions(file, &code));
        if let Some(fields) = struct_fields(&code, "Gone") {
            found.gone_fields = Some(fields);
        }
        codes.insert(file.clone(), (code, text.clone()));
    }
    // Direct requests, each to its innermost function.
    for (file, (code, text)) in &codes {
        for m in tokens.find_iter(code) {
            let tok = m.as_str().trim();
            if tok.starts_with('.') && fall_through_run.is_match(&code[..m.start()]) {
                continue;
            }
            let at = format!("{file}:{}: `{}`", line_of(text, m.start()), tok);
            let inner = funcs
                .iter()
                .filter(|f| f.file == *file && f.body.contains(&m.start()))
                .min_by_key(|f| f.body.len());
            match inner {
                Some(f) => {
                    found.requesters.entry(f.name.clone()).or_insert(at);
                }
                None => found.loose.push(at),
            }
        }
    }
    // A call of a function that requests is a request, to a fixed point.
    loop {
        let calls: Vec<(String, Regex)> = found
            .requesters
            .keys()
            .map(|n| (n.clone(), Regex::new(&format!(r"\b{n}\s*\(")).unwrap()))
            .collect();
        let mut grew = false;
        for f in &funcs {
            if found.requesters.contains_key(&f.name) {
                continue;
            }
            let body = &codes[&f.file].0[f.body.clone()];
            if let Some((callee, _)) = calls.iter().find(|(_, re)| re.is_match(body)) {
                found
                    .requesters
                    .insert(f.name.clone(), format!("calls `{callee}`"));
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    found.run_decides = funcs.iter().any(|f| {
        f.name == "run"
            && Regex::new(r"\bon_gone\s*\(")
                .unwrap()
                .is_match(&codes[&f.file].0[f.body.clone()])
    });
    found
}

/// What the scan refuses in what it read.
fn refusals(found: &Found) -> Vec<String> {
    let allowed: BTreeSet<&str> = REQUESTERS.iter().map(|(n, _)| *n).collect();
    let mut out = Vec::new();
    for (name, why) in &found.requesters {
        if !allowed.contains(name.as_str()) {
            out.push(format!("`{name}` requests ({why})"));
        }
    }
    for at in &found.loose {
        out.push(format!("a request outside any function: {at}"));
    }
    match &found.gone_fields {
        None => out.push("no `struct Gone`".into()),
        Some(fields) => {
            let word = |t: &str, w: &str| Regex::new(&format!(r"\b{w}\b")).unwrap().is_match(t);
            for (n, t) in fields {
                if word(t, "Scope") {
                    out.push(format!("`Gone::{n}` holds a Scope: {t}"));
                }
                if word(t, "Callback") && n != "fall_through" {
                    out.push(format!(
                        "`Gone::{n}` is a callback besides fall_through: {t}"
                    ));
                }
            }
        }
    }
    if !found.run_decides {
        out.push("`Gone::run` no longer decides through `on_gone(`".into());
    }
    out
}

/// A `gone` module as the scan reads it: what the real one does, in
/// little.
const FIXTURE: &str = r#"
use super::super::chat_folders::{
    start_chat_in, start_conversation, FolderEnv,
};
pub(in crate::pages) use landing::{rescued_open, Landing};

/// What the page hands the delete-elsewhere path. It names Scope and
/// Callback<()> in this comment only.
pub(in crate::pages) struct Gone {
    pub current: RwSignal<Option<ChatThread>>,
    pub stats: RwSignal<Option<(i64, Stats)>>,
    /// The page's delete path: the next thread opens.
    pub fall_through: Callback<i64>,
}

impl Gone {
    pub(in crate::pages) fn run(self, id: i64) {
        // Nothing to spawn here, nor an api::post: no request.
        let note = "spawn( api::get start_conversation(";
        let step = on_gone(true, note);
        if !step {
            self.fall_through.run(id);
        }
    }
}

async fn new_plain(alias: &str) -> Result<Made, crate::api::Error> {
    crate::api::post::<ChatThread, _>("/chat/api/threads", &alias).await
}

async fn make(how: Make, alias: &str) -> Result<Made, crate::api::Error> {
    match how {
        Make::Conversation(f) => start_conversation(f).await,
        Make::Plain => new_plain(alias).await,
    }
}

pub(in crate::pages) fn send_new(env: SendNew) {
    env.scope.spawn(async move {
        let _ = make(Make::Plain, "m").await;
        env.open.run((1, 2));
    });
}

pub(in crate::pages) fn rescued_open<F>(env: Landing, set_model: F)
where
    F: FnOnce(i64) -> bool,
{
    env.send.run(());
}

fn step(r: &mut Rescue) -> bool {
    r.making()
}

#[cfg(test)]
mod tests {
    fn helper(scope: Scope) {
        scope.spawn(async {});
    }
}
"#;

fn fixture(text: &str) -> Vec<(String, String)> {
    vec![("gone.rs".to_string(), text.to_string())]
}

/// Review CF-7: comments, literals and test code never trip the scan; a
/// request moved into a helper, a callback run in one, a call of an
/// allowed requester from `Gone::run`, and a `Gone` that holds a Scope or
/// another callback do.
#[test]
fn the_scan_finds_a_request_in_a_helper_and_never_in_a_comment() {
    let found = read(&fixture(FIXTURE));
    assert_eq!(refusals(&found), Vec::<String>::new());
    assert_eq!(
        found.requesters.keys().collect::<Vec<_>>(),
        ["make", "new_plain", "rescued_open", "send_new"]
    );

    let mutated = |from: &str, to: &str| {
        assert!(FIXTURE.contains(from), "{from}");
        refusals(&read(&fixture(&FIXTURE.replacen(from, to, 1))))
    };
    // A request moved into a helper: refused as the helper's, and as
    // run's once run calls it.
    let refused = mutated(
        "async fn new_plain",
        "fn rescue_now(scope: Scope) {\n    scope.spawn(async {});\n}\n\nasync fn new_plain",
    );
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].starts_with("`rescue_now` requests (gone.rs:"),
        "{refused:?}"
    );
    let text = FIXTURE
        .replacen(
            "async fn new_plain",
            "fn rescue_now(scope: Scope) {\n    scope.spawn(async {});\n}\n\nasync fn new_plain",
            1,
        )
        .replacen(
            "        let step = on_gone(true, note);",
            "        rescue_now(self.scope_of());\n        let step = on_gone(true, note);",
            1,
        );
    let refused = refusals(&read(&fixture(&text)));
    assert!(
        refused
            .iter()
            .any(|r| r.starts_with("`rescue_now` requests"))
            && refused
                .iter()
                .any(|r| r.starts_with("`run` requests (calls `rescue_now`)")),
        "{refused:?}"
    );
    // Gone::run calling an allowed requester.
    let refused = mutated(
        "        let step = on_gone(true, note);",
        "        send_new(self.env());\n        let step = on_gone(true, note);",
    );
    assert_eq!(refused, ["`run` requests (calls `send_new`)"]);
    // A callback run in a helper.
    let refused = mutated(
        "fn step(r: &mut Rescue) -> bool {\n    r.making()",
        "fn step(r: &mut Rescue) -> bool {\n    r.open.run(7);\n    r.making()",
    );
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].starts_with("`step` requests (gone.rs:"),
        "{refused:?}"
    );
    // The API called straight from run, in code after a comment.
    let refused = mutated(
        "        let step = on_gone(true, note);",
        "        // a comment\n        let _ = crate::api::get::<Value>(\"/x\");\n        let step = on_gone(true, note);",
    );
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].starts_with("`run` requests (gone.rs:"),
        "{refused:?}"
    );
    // Gone's fields.
    let refused = mutated(
        "    pub fall_through: Callback<i64>,\n",
        "    pub fall_through: Callback<i64>,\n    pub create: Callback<(i64, String)>,\n    pub scope: Scope,\n",
    );
    assert_eq!(
        refused,
        [
            "`Gone::create` is a callback besides fall_through: Callback<(i64,String)>",
            "`Gone::scope` holds a Scope: Scope",
        ]
    );
    // run deciding some other way.
    let refused = mutated("on_gone(true, note)", "decide(true, note)");
    assert_eq!(
        refused,
        ["`Gone::run` no longer decides through `on_gone(`"]
    );
}

#[test]
fn the_delete_elsewhere_path_sends_no_request() {
    let core = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sync = core.join("../lmgw-ui/src/pages/chat_sync");
    let mut paths = vec![sync.join("gone.rs")];
    collect_rs(&sync.join("gone"), &mut paths);
    paths.sort();
    assert!(
        paths.len() >= 3,
        "the scan found only {paths:?}, which means it stopped reading the gone module"
    );
    let files: Vec<(String, String)> = paths
        .iter()
        .map(|p| {
            let name = p.strip_prefix(&sync).unwrap().display().to_string();
            (name, std::fs::read_to_string(p).unwrap())
        })
        .collect();
    let found = read(&files);
    let refused = refusals(&found);
    assert!(
        refused.is_empty(),
        "the Chat page's delete-elsewhere path (lmgw-ui pages/chat_sync/gone) may request \
         only in {:?}, at the owner's Send (reviews CL-14, CF-7):\n{}",
        REQUESTERS.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        refused.join("\n")
    );
    // Each allowed requester is found requesting: if one is not, the scan
    // stopped seeing requests, or the function changed and the list is to
    // be read again.
    for (name, why) in REQUESTERS {
        assert!(
            found.requesters.contains_key(*name),
            "the scan no longer finds `{name}` ({why}) requesting: {:?}",
            found.requesters
        );
    }
}
