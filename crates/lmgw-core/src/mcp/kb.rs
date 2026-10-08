//! The `kb__*` built-in toolset (chat-complete design §9.3, §9.4): the
//! owner's knowledge bases, searchable and readable by a model — on the
//! aggregate `/mcp` plane next to `docs__*`, and attachable to a Chat thread,
//! a `/v1/responses` request or an agent run by the reserved label `kb`.
//!
//! **Who sees which bases** is [`KbAccess`]. On `/mcp` (and wherever a
//! caller attaches the label itself) only bases whose `mcp_visible` switch is
//! on exist; the Chat passes the thread's own selection instead, which
//! ignores that switch (§9.4). A base outside the caller's access answers
//! exactly like one that does not exist.
//!
//! **Key scopes need nothing new**: the names are `kb__list`, `kb__search`,
//! `kb__read`, so a client key's tool scope takes `kb__*` like any prefix.

use serde_json::{json, Map, Value};

use crate::knowledge::retrieve::{self, Excerpt, Options};
use crate::knowledge::store::{self, Kb};
use crate::knowledge::{ops, read};
use crate::state::SharedState;

use super::selfadmin::err_result;
use super::CallError;

/// Prefix every tool here carries — reserved from southbound servers by
/// [`super::RESERVED_KB_NAMESPACE`].
pub const PREFIX: &str = super::RESERVED_KB_NAMESPACE;

pub fn owns(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// Which knowledge bases a caller reaches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum KbAccess {
    /// Every base whose `mcp_visible` switch is on — `/mcp`, and any caller
    /// that attached the `kb` label on its own.
    #[default]
    McpVisible,
    /// Exactly these bases, whatever their switch says: a Chat thread's own
    /// selection (§9.3 tool mode).
    Only(Vec<i64>),
}

impl KbAccess {
    /// The bases this access reaches, by name.
    pub async fn bases(&self, state: &SharedState) -> Result<Vec<Kb>, String> {
        let all = store::list_kbs(&state.knowledge.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(match self {
            Self::McpVisible => all.into_iter().filter(|k| k.mcp_visible).collect(),
            Self::Only(ids) => all.into_iter().filter(|k| ids.contains(&k.id)).collect(),
        })
    }

    fn describe(&self) -> &'static str {
        match self {
            Self::McpVisible => "shared on this gateway's /mcp",
            Self::Only(_) => "selected for this conversation",
        }
    }
}

/// `tools/list` entries. Static: which bases exist is `kb__list`'s answer,
/// so a caller's tool list does not change every time a base is added.
pub fn list() -> Vec<Value> {
    vec![
        json!({
            "name": "kb__list",
            "description":
                "List the knowledge bases you can search: the owner's own document \
                 collections (tax papers, contracts, manuals, notes …). Each comes with its \
                 description, how many files and chunks it holds, and whether it is still \
                 ingesting. Call this first to learn their names, then kb__search.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "kb__search",
            "description":
                "Search the owner's knowledge bases and get back numbered excerpts of their \
                 documents, each with its knowledge base, file, page (for PDFs), file_id and \
                 chunk id. Hybrid retrieval: keyword search plus vector search, fused, then \
                 reranked where the base has a reranker. Cite what you use by its number. To \
                 read on in a document around an excerpt, call kb__read with its file_id and \
                 chunk id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What you are looking for, in your own words."
                    },
                    "kb": {
                        "type": "string",
                        "description":
                            "Search only this knowledge base (its name as kb__list gives it, \
                             or its id). Omit it to search every base you can reach."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description":
                            "Return at most roughly this many tokens of excerpts; 0 means no \
                             budget. Omit it and the gateway's knowledge budget applies; either \
                             way the answer says how many tokens it used and how many excerpts \
                             the budget dropped."
                    }
                },
                "required": ["query"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "kb__read",
            "description":
                "Read a document from a knowledge base in order, rather than searching it: \
                 from a chunk id kb__search returned (from_chunk), from the first text of a \
                 PDF page (page), or from the start. Returns the document's text up to the \
                 token budget and the chunk id to continue from.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_id": {
                        "type": "integer",
                        "description": "The file, as kb__search reported it."
                    },
                    "page": {
                        "type": "integer",
                        "description": "Start at this 1-based page (PDF files only)."
                    },
                    "from_chunk": {
                        "type": "string",
                        "description":
                            "Start at this chunk id (from kb__search, or the next_chunk of an \
                             earlier kb__read). Pass either this or page, not both."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description":
                            "Read at most roughly this many tokens; 0 reads to the end of the \
                             document. Omit it and the gateway's knowledge budget applies; the \
                             answer says how much it returned and where to continue."
                    }
                },
                "required": ["file_id"],
                "additionalProperties": false,
            },
        }),
    ]
}

fn ok_text(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

fn ok_json(v: &Value) -> Value {
    let text = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    ok_text(text)
}

fn arg_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("argument '{key}' must be a string")),
    }
}

/// `kb` — a name, or an id (a number is taken as one too: it is still a
/// base the model saw).
fn arg_kb(args: &Map<String, Value>) -> Result<Option<String>, String> {
    match args.get("kb") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(_) => Err("argument 'kb' must be a knowledge base's name or id".into()),
    }
}

fn arg_i64(args: &Map<String, Value>, key: &str) -> Result<Option<i64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("argument '{key}' must be an integer")),
        Some(_) => Err(format!("argument '{key}' must be an integer")),
    }
}

/// The budget a call runs with: its own argument (`0` = none), else the
/// caller's default (a Chat thread's), else the owner's knowledge budget —
/// `chat_kb_budget_tokens`, Settings → Chat, always above zero, so a
/// `kb__read` without a budget reads a visible amount, not a whole book.
fn budget(
    state: &SharedState,
    args: &Map<String, Value>,
    default: Option<usize>,
) -> Result<Option<usize>, String> {
    Ok(match arg_i64(args, "budget_tokens")? {
        Some(b) if b < 0 => return Err("budget_tokens must not be negative".into()),
        Some(0) => None,
        Some(b) => Some(b as usize),
        None => {
            Some(default.unwrap_or(state.snapshot().settings.chat_kb_budget_tokens.max(1) as usize))
        }
    })
}

/// Invoke one `kb__*` tool for a caller that reaches `access`.
/// `default_budget` is the caller's own default for `budget_tokens` (a Chat
/// thread's retrieval budget); `None` falls back to the owner's knowledge
/// budget (`chat_kb_budget_tokens`). `Err(CallError)` means "not one of these tools"; everything else
/// is an `isError` result the model can read and act on.
pub async fn call(
    state: &SharedState,
    name: &str,
    args: Option<Map<String, Value>>,
    access: &KbAccess,
    (default_budget, charged): (Option<usize>, Option<&crate::proxy::RequestCtx>),
) -> Result<Value, CallError> {
    let a = args.unwrap_or_default();
    let out = match name {
        "kb__list" => list_tool(state, access).await.map(|v| ok_json(&v)),
        "kb__search" => search_tool(state, &a, access, (default_budget, charged))
            .await
            .map(ok_text),
        "kb__read" => read_tool(state, &a, access, default_budget)
            .await
            .map(ok_text),
        other => return Err(CallError::ToolNotFound(other.to_string())),
    };
    Ok(out.unwrap_or_else(|e| err_result(&e)))
}

async fn list_tool(state: &SharedState, access: &KbAccess) -> Result<Value, String> {
    let bases = access.bases(state).await?;
    if bases.is_empty() {
        return Ok(json!({
            "bases": [],
            "message": format!(
                "there are no knowledge bases {} — answer without them",
                access.describe()
            ),
        }));
    }
    let mut out = Vec::with_capacity(bases.len());
    for kb in &bases {
        let v = ops::view(state, kb).await?;
        out.push(json!({
            "id": kb.id,
            "name": kb.name,
            "description": kb.description,
            "files": v.counts.files,
            "ready_files": v.counts.ready,
            "waiting_files": v.counts.pending + v.counts.ingesting,
            "chunks": v.counts.chunks,
            "status": if v.counts.pending + v.counts.ingesting > 0 {
                "ingesting"
            } else {
                kb.status.as_str()
            },
        }));
    }
    Ok(json!({
        "bases": out,
        "next_step": "kb__search",
        "hint": "pass a base's name as `kb` to kb__search to search only that one, or omit it \
                 to search all of these",
    }))
}

/// Resolve the `kb` argument within the caller's access: a name
/// (case-insensitive) or an id. Outside the access reads as absent.
async fn pick_bases(
    state: &SharedState,
    access: &KbAccess,
    wanted: Option<&str>,
) -> Result<Vec<Kb>, String> {
    let bases = access.bases(state).await?;
    if bases.is_empty() {
        return Err(format!(
            "there are no knowledge bases {} — answer without them",
            access.describe()
        ));
    }
    let Some(w) = wanted.map(str::trim).filter(|w| !w.is_empty()) else {
        return Ok(bases);
    };
    let hit = bases
        .iter()
        .find(|k| k.name.eq_ignore_ascii_case(w) || w.parse::<i64>().is_ok_and(|id| id == k.id))
        .cloned();
    hit.map(|k| vec![k]).ok_or_else(|| {
        format!(
            "no knowledge base '{w}' — the ones you can search are: {}",
            bases
                .iter()
                .map(|k| k.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// `charged`: whose request the search's embedder and reranker run for
/// (`retrieve::Options::caller`).
async fn search_tool(
    state: &SharedState,
    args: &Map<String, Value>,
    access: &KbAccess,
    (default_budget, charged): (Option<usize>, Option<&crate::proxy::RequestCtx>),
) -> Result<String, String> {
    let query = arg_str(args, "query")?
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("query is required")?;
    let bases = pick_bases(state, access, arg_kb(args)?.as_deref()).await?;
    let budget = budget(state, args, default_budget)?;
    let ids: Vec<i64> = bases.iter().map(|k| k.id).collect();
    let r = retrieve::retrieve(
        state,
        &ids,
        query,
        &Options {
            budget_tokens: budget,
            params: None,
            caller: charged.cloned(),
        },
    )
    .await;
    Ok(render_search(query, budget, &r))
}

/// `kb__search`'s answer: markdown, which small models read better than JSON.
pub fn render_search(query: &str, budget: Option<usize>, r: &retrieve::Retrieval) -> String {
    let mut out = format!("# Knowledge search: \"{query}\"\n\n");
    out.push_str(&format!(
        "Searched: {} · {} excerpt(s) · {} tokens{}{}\n",
        if r.searched.is_empty() {
            "nothing".to_string()
        } else {
            r.searched.join(", ")
        },
        r.excerpts.len(),
        r.tokens,
        budget.map(|b| format!(" (budget {b})")).unwrap_or_default(),
        if r.dropped > 0 {
            format!(" · {} more dropped by the budget", r.dropped)
        } else {
            String::new()
        }
    ));
    for (i, e) in r.excerpts.iter().enumerate() {
        out.push_str(&format!("\n## [{}] {}\n", i + 1, citation(e)));
        out.push_str(&format!("file_id {} · chunk {}\n\n", e.file_id, e.chunk_id));
        if !e.heading_path.is_empty() {
            out.push_str(&format!("_{}_\n\n", e.heading_path));
        }
        // The excerpt is a document's own text: fenced, so nothing in it can
        // pose as one of this answer's `## [n]` headings or `file_id` lines.
        out.push_str(&fenced(e.text.trim_end()));
    }
    if r.excerpts.is_empty() {
        out.push_str("\nNothing matched.\n");
    }
    if !r.notes.is_empty() {
        out.push_str("\nNotes:\n");
        for n in &r.notes {
            out.push_str(&format!("- {n}\n"));
        }
    }
    if !r.excerpts.is_empty() {
        out.push_str(
            "\nCite excerpts by their number. To read on in a document, call kb__read with \
             its file_id and from_chunk set to the chunk id.\n",
        );
    }
    out
}

/// `text` in a code fence longer than any run of backticks inside it, so no
/// line of the text can close the fence and everything after it stays quoted.
fn fenced(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}text\n{text}\n{fence}\n")
}

/// `Taxes · Steuer 2025.pdf · page 3`
fn citation(e: &Excerpt) -> String {
    match e.page {
        Some(p) => format!("{} · {} · page {p}", e.kb, e.file),
        None => format!("{} · {}", e.kb, e.file),
    }
}

async fn read_tool(
    state: &SharedState,
    args: &Map<String, Value>,
    access: &KbAccess,
    default_budget: Option<usize>,
) -> Result<String, String> {
    let file_id = arg_i64(args, "file_id")?.ok_or("file_id is required")?;
    let page = arg_i64(args, "page")?;
    let from_chunk = arg_str(args, "from_chunk")?;
    if page.is_some() && from_chunk.is_some_and(|c| !c.trim().is_empty()) {
        return Err("pass page or from_chunk, not both".into());
    }
    let budget = budget(state, args, default_budget)?;
    let missing = || format!("no file {file_id} in the knowledge bases you can read");
    let file = store::get_file(&state.knowledge.pool, file_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(missing)?;
    let bases = access.bases(state).await?;
    let kb = bases
        .iter()
        .find(|k| k.id == file.kb_id)
        .ok_or_else(missing)?;
    let r = read::read(state, &file, &kb.name, page, from_chunk, budget).await?;
    let pages = match (r.first_page, r.last_page) {
        (Some(a), Some(b)) if a == b => format!(" · page {a}"),
        (Some(a), Some(b)) => format!(" · pages {a}–{b}"),
        _ => String::new(),
    };
    let mut out = format!(
        "# {} · {}{}\n\nfile_id {} · {} tokens{}\n\n",
        r.kb,
        r.file,
        pages,
        r.file_id,
        r.tokens,
        budget.map(|b| format!(" (budget {b})")).unwrap_or_default()
    );
    // Fenced like an excerpt: the document cannot forge the continuation
    // instruction below it.
    out.push_str(&fenced(r.text.trim_end()));
    out.push_str(&match &r.next_chunk {
        Some(next) => format!(
            "\n{} more chunk(s) follow. Continue with kb__read {{\"file_id\": {}, \
             \"from_chunk\": \"{next}\"}}.\n",
            r.remaining_chunks, r.file_id
        ),
        None => "\nEnd of the document.\n".to_string(),
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_is_in_the_reserved_namespace_and_declares_a_closed_schema() {
        for t in list() {
            let name = t["name"].as_str().unwrap();
            assert!(owns(name), "{name} must carry the reserved prefix");
            assert!(name.len() <= super::super::MAX_TOOL_NAME_LEN);
            assert_eq!(
                t["inputSchema"]["additionalProperties"],
                Value::Bool(false),
                "{name} must reject arguments it does not know"
            );
            assert!(
                t["description"].as_str().unwrap_or_default().len() > 80,
                "{name}'s description is the only documentation its caller gets"
            );
        }
    }

    #[test]
    fn a_fence_is_longer_than_any_backtick_run_in_the_text() {
        let f = fenced("plain");
        assert_eq!(f, "```text\nplain\n```\n");
        let f = fenced("a\n`````\nb");
        assert!(
            f.starts_with("``````text\n") && f.ends_with("\n``````\n"),
            "{f}"
        );
    }

    #[test]
    fn the_namespaces_do_not_overlap() {
        assert!(owns("kb__search"));
        assert!(!owns("docs__query"));
        assert!(!super::super::docs::owns("kb__search"));
        assert!(!super::super::selfadmin::owns("kb__search"));
    }
}
