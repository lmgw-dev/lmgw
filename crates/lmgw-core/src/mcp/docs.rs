//! The `docs__*` built-in toolset (quickdoc §7): versioned library
//! documentation, served to every agent on the northbound `/mcp` plane.
//!
//! **Why this plane and not `/mcp/admin`.** The self-admin tools are the
//! gateway's configuration API and live behind their own token; these are the
//! opposite — they are for ordinary callers, they mutate nothing a caller can
//! see except a request counter, and a coding agent that cannot reach them is
//! an agent quickdoc exists to help and does not. So they ride the aggregate
//! plane next to the registered MCP servers' tools, under a namespace reserved
//! from those servers exactly the way `lmgw__` is.
//!
//! **The shape is context7's, minus its three defects** (§7): the two-call
//! `resolve` → `query` flow, version-pinned corpus ids and metadata-rich resolve
//! are kept; the payload is verbatim rather than model-rewritten, prose sections
//! are first-class, and the response budget is a visible parameter rather than a
//! server-side cap nobody can see.
//!
//! **Descriptions are the documentation.** The caller has no access to this
//! source, so what it must know — that a corpus id is `library@version`, that a
//! resolve miss leads to `docs__request` rather than a dead end, that the badges
//! degrade an answer without blocking it — is in the descriptions and in the
//! results themselves.

use quickdoc_core::store as qstore;
use serde_json::{json, Map, Value};

use crate::quickdoc::query;
use crate::state::SharedState;

use super::selfadmin::err_result;
use super::CallError;

/// Prefix every tool here carries — reserved from southbound servers by
/// [`super::RESERVED_DOCS_NAMESPACE`].
pub const PREFIX: &str = super::RESERVED_DOCS_NAMESPACE;

pub fn owns(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// `tools/list` entries. No mode gate: unlike the self-admin plane there is
/// nothing here an ordinary caller should not have, and hiding a read tool
/// behind a setting would only make agents fall back to guessing from
/// pretraining — the failure quickdoc exists to fix.
pub fn list() -> Vec<Value> {
    vec![
        json!({
            "name": "docs__resolve",
            "description":
                "Find which documentation corpora this gateway has for a library, before \
                 querying one. Returns version-pinned corpus ids (`library@version`) with the \
                 metadata needed to choose between them: chunk count, crawl date, source kind, \
                 the models it was ingested and embedded with, and its measured eval score. \
                 Status flags (re-embed required, eval regression) are reported but never \
                 block a query — prefer another version or caveat your answer. If nothing \
                 matches, the reply says so and points at docs__request.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "library": {
                        "type": "string",
                        "description":
                            "Library or framework name, e.g. 'axum'. Matched exactly first, \
                             then as a case-insensitive substring."
                    },
                    "query": {
                        "type": "string",
                        "description":
                            "What you intend to look up. Not used for matching; it is carried \
                             into the docs__request hint when nothing matches."
                    }
                },
                "required": ["library"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "docs__query",
            "description":
                "Search one corpus and get back the matching documentation sections as \
                 markdown: heading path, the verbatim source text (never rewritten), and a \
                 deep link to the page it came from. Hybrid retrieval — BM25 plus vector \
                 search, fused, then optionally reranked. Call docs__resolve first to get the \
                 corpus_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "corpus_id": {
                        "type": "string",
                        "description": "`library@version`, exactly as docs__resolve reported it."
                    },
                    "query": {
                        "type": "string",
                        "description": "What you are looking for, in your own words."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description":
                            "Cap the answer at roughly this many tokens. Omit it and the \
                             gateway's configured default applies, which may well be 'no \
                             budget'; either way the answer states how many tokens it used \
                             and how many chunks the budget dropped. Nothing is trimmed \
                             silently."
                    },
                    "params": {
                        "type": "object",
                        "description":
                            "Per-request retrieval stage overrides. Ordinary callers should \
                             omit this; it exists for the search playground and for agents \
                             tuning retrieval. Anything not given keeps the gateway's \
                             configured default.",
                        "properties": {
                            "k_fts": { "type": "integer", "description": "BM25 candidates." },
                            "k_vec": { "type": "integer", "description": "Vector candidates." },
                            "rrf_k": { "type": "number", "description": "RRF damping constant." },
                            "rerank": { "type": "boolean", "description": "Run the rerank stage." },
                            "k_rerank": { "type": "integer", "description": "Rerank depth." },
                            "limit": { "type": "integer", "description": "Chunks returned." },
                            "fts_weights": {
                                "type": "object",
                                "description": "BM25 per-column weights.",
                                "properties": {
                                    "payload": { "type": "number" },
                                    "heading_path": { "type": "number" },
                                    "derived_title": { "type": "number" },
                                    "derived_summary": { "type": "number" }
                                },
                                "additionalProperties": false
                            }
                        },
                        "additionalProperties": false
                    }
                },
                "required": ["corpus_id", "query"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "docs__request",
            "description":
                "Ask this gateway's owner to ingest documentation it does not have yet. Use it \
                 when docs__resolve finds nothing. This files a request in the owner's queue — \
                 it does not start an ingest, and no corpus appears immediately; ingestion is \
                 always the owner's decision. Asking again for the same library@version bumps \
                 a counter instead of piling up, and if a matching corpus already exists you \
                 get its id back instead of filing anything.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "library": { "type": "string", "description": "Library or framework name." },
                    "version": {
                        "type": "string",
                        "description":
                            "Version you need, if it matters. Omit for 'any current version'."
                    },
                    "reason": {
                        "type": "string",
                        "description":
                            "One line on what you were trying to do. It is what the owner sees \
                             when deciding, so it is worth writing."
                    }
                },
                "required": ["library"],
                "additionalProperties": false,
            },
        }),
    ]
}

fn ok_json(v: &Value) -> Value {
    let text = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

/// `docs__query` answers in markdown rather than JSON: small models parse it
/// better, and the payload is already markdown from the source document (§7).
fn ok_text(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

fn arg_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("argument '{key}' must be a string")),
    }
}

fn arg_usize(args: &Map<String, Value>, key: &str) -> Result<Option<usize>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|v| Some(v as usize))
            .ok_or_else(|| format!("argument '{key}' must be a non-negative integer")),
        Some(_) => Err(format!("argument '{key}' must be an integer")),
    }
}

/// Invoke one `docs__*` tool.
///
/// `client_name` is the MCP client's `initialize` name when the transport knew
/// one; `docs__request` records it so the owner's queue says who asked.
/// `Err(CallError)` means "not one of these tools" — everything else is an
/// `isError` result the calling model can read and act on.
pub async fn call(
    state: &SharedState,
    name: &str,
    args: Option<Map<String, Value>>,
    client_name: Option<&str>,
) -> Result<Value, CallError> {
    let a = args.unwrap_or_default();
    let out = match name {
        "docs__resolve" => resolve(state, &a).await,
        "docs__query" => {
            return Ok(query_tool(state, &a)
                .await
                .unwrap_or_else(|e| err_result(&e)))
        }
        "docs__request" => request(state, &a, client_name).await,
        other => return Err(CallError::ToolNotFound(other.to_string())),
    };
    Ok(match out {
        Ok(v) => ok_json(&v),
        Err(e) => err_result(&e),
    })
}

// ---------------------------------------------------------------------------
// docs__resolve
// ---------------------------------------------------------------------------

async fn resolve(state: &SharedState, args: &Map<String, Value>) -> Result<Value, String> {
    let library = arg_str(args, "library")?
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("library is required")?;
    let intent = arg_str(args, "query")?.unwrap_or_default();

    let all = qstore::list_corpora(&state.corpus)
        .await
        .map_err(|e| e.to_string())?;
    let needle = library.to_lowercase();
    let mut matches: Vec<_> = all
        .iter()
        .filter(|c| c.library.eq_ignore_ascii_case(library))
        .collect();
    if matches.is_empty() {
        matches = all
            .iter()
            .filter(|c| c.library.to_lowercase().contains(&needle))
            .collect();
    }
    matches.sort_by(|a, b| a.library.cmp(&b.library).then(b.version.cmp(&a.version)));

    if matches.is_empty() {
        let known: Vec<String> = {
            let mut libs: Vec<String> = all.iter().map(|c| c.library.clone()).collect();
            libs.sort();
            libs.dedup();
            libs
        };
        return Ok(json!({
            "matches": [],
            "message": format!(
                "no documentation corpus on this gateway matches '{library}'"
            ),
            "libraries_available": known,
            "next_step": "docs__request",
            "hint": format!(
                "call docs__request(library=\"{library}\"{}) to ask this gateway's owner to \
                 ingest it. That files a request; it does not start an ingest, so do not wait \
                 for a corpus to appear.",
                if intent.is_empty() {
                    String::new()
                } else {
                    format!(", reason=\"{intent}\"")
                }
            ),
        }));
    }

    let snap = state.snapshot();
    let mut out = Vec::new();
    // The `re_embed_required` badge covers two states that mean opposite things
    // to a caller, so the advice below has to know which of them it is talking
    // about — see [`query::CorpusStatus::embed_model_resolvable`].
    let (mut mid_re_embed, mut pin_lost) = (false, false);
    for c in matches {
        let status = query::corpus_status(&snap, c);
        if status.embed_status == "re_embed_required" {
            *(if status.embed_model_resolvable {
                &mut mid_re_embed
            } else {
                &mut pin_lost
            }) = true;
        }
        let sources: Vec<String> = qstore::list_sources(&state.corpus, c.id)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|s| s.root)
            .collect();
        out.push(json!({
            "corpus_id": c.corpus_id(),
            "library": c.library,
            "version": c.version,
            "status": c.status,
            "chunk_count": c.chunk_count,
            "crawl_date": c.crawl_date,
            "source_kind": c.source_kind,
            "sources": sources,
            "ingest_model": c.ingest_model,
            "ingest_prompt_version": c.ingest_prompt_version,
            "embed_model": c.embed_identity().to_string(),
            "eval_score": c.eval_score,
            "eval_k": c.eval_k,
            "eval_best": c.eval_best,
            "eval_at": c.eval_at,
            "embed_status": status.embed_status,
            "eval_status": status.eval_status,
            "flags": status.flags,
            "warnings": status.warnings,
        }));
    }

    let mut hint = String::from(
        "pass one of these corpus_id values to docs__query. `eval_score` is this gateway's own \
         measured hit rate for the corpus — higher is better, and a corpus flagged \
         `eval_regression` still answers, it has just scored below its own best.",
    );
    if mid_re_embed {
        hint.push_str(
            " A `re_embed_required` corpus whose embedding model is still on this gateway is \
             mid re-embed: some chunks have no vector yet, so the vector half of the search \
             sees only part of it and BM25 carries the rest. It answers.",
        );
    }
    if pin_lost {
        hint.push_str(
            " A `re_embed_required` corpus whose embedding model this gateway can no longer \
             resolve — its `warnings` name the model — cannot be searched at all: docs__query \
             against it fails until its owner re-embeds it. Use another version if there is \
             one, or answer without it and say the documentation was unavailable.",
        );
    }
    Ok(json!({
        "matches": out,
        "next_step": "docs__query",
        "hint": hint,
    }))
}

// ---------------------------------------------------------------------------
// docs__query
// ---------------------------------------------------------------------------

async fn query_tool(state: &SharedState, args: &Map<String, Value>) -> Result<Value, String> {
    let corpus_id = arg_str(args, "corpus_id")?
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("corpus_id is required (get one from docs__resolve)")?;
    let q = arg_str(args, "query")?
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("query is required")?;

    let corpus = qstore::get_corpus_by_id(&state.corpus, corpus_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!("no corpus '{corpus_id}' on this gateway — call docs__resolve to list them")
        })?;

    let snap = state.snapshot();
    let mut params = query::default_params(&snap);
    if let Some(p) = args.get("params").filter(|v| !v.is_null()) {
        params = query::apply_params(&params, p)?;
    }
    // The explicit argument wins over both the settings default and `params`:
    // it is the one a caller is most likely to have set deliberately.
    if let Some(b) = arg_usize(args, "budget_tokens")? {
        params.budget_tokens = (b > 0).then_some(b);
    }

    let retriever = query::open_retriever(state, &corpus)
        .await
        .map_err(|e| e.to_string())?;
    let result = retriever
        .search(q, &params)
        .await
        .map_err(|e| e.to_string())?;
    let ids: Vec<String> = result.hits.iter().map(|h| h.chunk.id.clone()).collect();
    let urls = qstore::document_urls(&state.corpus, &ids)
        .await
        .map_err(|e| e.to_string())?;
    let status = query::corpus_status(&snap, &corpus);

    Ok(ok_text(quickdoc_core::markdown::render(
        &corpus,
        q,
        &result,
        &urls,
        &status.warnings,
    )))
}

// ---------------------------------------------------------------------------
// docs__request
// ---------------------------------------------------------------------------

async fn request(
    state: &SharedState,
    args: &Map<String, Value>,
    client_name: Option<&str>,
) -> Result<Value, String> {
    let library = arg_str(args, "library")?
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("library is required")?;
    let version = arg_str(args, "version")?.unwrap_or_default().trim();
    let reason = arg_str(args, "reason")?
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // A corpus that already exists *and can answer* is the answer to the
    // request, and filing one would put something in the owner's queue that is
    // already satisfied. A corpus that failed, never finished, or has no chunks
    // is not that: it is the opposite — the owner needs to know somebody wanted
    // it, and the agent needs to know why asking again did not help. Those are
    // filed like any other request, with the corpus named in the answer.
    let existing: Vec<_> = qstore::list_corpora(&state.corpus)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|c| {
            c.library.eq_ignore_ascii_case(library)
                && (version.is_empty() || c.version.eq_ignore_ascii_case(version))
        })
        .collect();
    let newest = existing.iter().max_by(|a, b| a.version.cmp(&b.version));
    if let Some(c) = newest.filter(|c| c.status == "ready" && c.chunk_count > 0) {
        return Ok(json!({
            "filed": false,
            "corpus_id": c.corpus_id(),
            "message": format!(
                "this gateway already has {} — nothing was filed",
                c.corpus_id()
            ),
            "next_step": "docs__query",
        }));
    }
    let unusable = newest.map(|c| {
        json!({
            "corpus_id": c.corpus_id(),
            "status": c.status,
            "chunk_count": c.chunk_count,
        })
    });

    let r = qstore::file_doc_request(&state.corpus, library, version, reason, client_name)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(c) = &unusable {
        let corpus_id = c["corpus_id"].as_str().unwrap_or_default();
        let status = c["status"].as_str().unwrap_or_default();
        return Ok(json!({
            "filed": true,
            "library": r.library,
            "version": r.version,
            "status": r.status,
            "times_requested": r.count,
            "first_requested_at": r.first_requested_at,
            "requested_by": r.client_name,
            "existing_corpus": c,
            "message": format!(
                "this gateway has a corpus {corpus_id}, but it is '{status}' with {} chunk(s) \
                 and cannot answer, so the request was filed anyway ({} so far). Ingestion is \
                 the owner's decision, so no corpus will appear from this call — answer from \
                 what you have and say the documentation was not available.",
                c["chunk_count"].as_i64().unwrap_or(0),
                times(r.count),
            ),
        }));
    }
    Ok(json!({
        "filed": true,
        "library": r.library,
        "version": r.version,
        "status": r.status,
        "times_requested": r.count,
        "first_requested_at": r.first_requested_at,
        "requested_by": r.client_name,
        "message": format!(
            "filed a request for {}{} in the owner's queue ({} so far). Ingestion is the \
             owner's decision, so no corpus will appear from this call — answer from what you \
             have and say the documentation was not available.",
            r.library,
            if r.version.is_empty() {
                String::new()
            } else {
                format!("@{}", r.version)
            },
            times(r.count),
        ),
    }))
}

/// "1 request" / "n requests" — the count the owner's queue will show.
fn times(count: i64) -> String {
    if count == 1 {
        "1 request".to_string()
    } else {
        format!("{count} requests")
    }
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
    fn the_namespaces_do_not_overlap() {
        assert!(owns("docs__query"));
        assert!(!owns("lmgw__status"));
        assert!(!super::super::selfadmin::owns("docs__query"));
    }
}
