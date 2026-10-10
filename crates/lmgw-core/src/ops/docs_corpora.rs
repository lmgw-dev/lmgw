//! Documentation corpora (quickdoc §10)
//!
//! The dashboard owns corpus creation because the owner decides what a corpus
//! is (`web::api_docs`), and that stays true — these functions are the *owner's*
//! hands on the admin plane, not the agent-facing `docs__*` toolset on `/mcp`.
//! What they add is bulk: importing forty libraries through a wizard is forty
//! manual passes, while an agent holding the request queue can do the same run
//! unattended. Every one of them calls the dashboard's handler.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::jobs::{self, JobKind};
use crate::state::SharedState;

use super::*;

/// Which corpus a caller named, as both front ends accept it.
async fn corpus_arg(
    state: &SharedState,
    sel: Option<&str>,
) -> Result<quickdoc_core::store::Corpus, String> {
    let sel = sel
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("pass corpus (numeric id or `library@version`, from lmgw__docs_corpora)")?;
    crate::web::api_docs::corpus_by_selector(state, sel).await
}

/// The most recent ingest or re-embed job for a corpus, running or finished.
///
/// Finished ones are included on purpose: the call after a start asks "how did
/// it go", and a view that only knew about live jobs would answer "none" for
/// both "it succeeded" and "it failed in the first second".
async fn corpus_job(
    state: &SharedState,
    corpus_id: i64,
    active_only: bool,
) -> Option<jobs::JobView> {
    let key = crate::quickdoc::ingest::job_key(corpus_id);
    let mut found: Vec<jobs::JobView> = Vec::new();
    for kind in [JobKind::Ingest, JobKind::ReEmbed] {
        let rows = jobs::list(state, Some(kind), active_only, 0)
            .await
            .unwrap_or_default();
        found.extend(
            rows.into_iter()
                .filter(|j| j.key.as_deref() == Some(key.as_str())),
        );
    }
    // Rows come back newest-first per kind; the highest id wins across both.
    found.into_iter().max_by_key(|j| j.id)
}

fn job_view(j: &jobs::JobView) -> Value {
    json!({
        "id": j.id,
        "kind": j.kind,
        "status": j.status,
        "stage": j.stage,
        "done": j.done,
        "total": j.total,
        "percent": j.percent,
        "error": j.error,
        "started_at": j.started_at,
        "finished_at": j.finished_at,
    })
}

/// Every corpus, or one named corpus, each with the job attached to it.
///
/// Documents and chunks are deliberately not here: they are the dashboard's
/// corpus browser (`GET /api/docs/corpora/{id}/documents`), and a corpus with
/// two thousand pages would otherwise answer a routine "how is the import
/// going" with two thousand rows.
pub async fn docs_corpora(state: &SharedState, corpus: Option<&str>) -> Result<Value, String> {
    let sel = corpus.map(str::trim).filter(|s| !s.is_empty());
    if let Some(sel) = sel {
        let c = crate::web::api_docs::corpus_by_selector(state, sel).await?;
        let mut view = crate::web::api_docs::corpus_view(state, &c).await?;
        if let Some(j) = corpus_job(state, c.id, false).await {
            view["job"] = job_view(&j);
        }
        return Ok(json!({ "corpus": view }));
    }

    let mut out = crate::web::api_docs::list_corpora_inner(state).await?;
    if let Some(rows) = out["corpora"].as_array_mut() {
        for v in rows.iter_mut() {
            let Some(id) = v["id"].as_i64() else { continue };
            if let Some(j) = corpus_job(state, id, false).await {
                v["job"] = job_view(&j);
            }
        }
    }
    out["hint"] = json!(
        "`status` is the corpus's own state (ready|ingesting|failed); `job` is the ingest or \
         re-embed run behind it. `flags`/`warnings` say when a corpus is servable but degraded — \
         a re-embed required, an eval regression. `pending_requests` is the queue in \
         lmgw__docs_requests."
    );
    Ok(out)
}

/// Sparse patch for a corpus. Lists arrive as newline-delimited text, the same
/// way an MCP server's `args` do: one flat scalar per argument (§20).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DocsCorpusPatch {
    pub action: String,
    pub corpus: Option<String>,
    pub library: Option<String>,
    pub version: Option<String>,
    pub embed_model: Option<String>,
    pub ingest_model: Option<String>,
    pub source_root: Option<String>,
    pub source_kind: Option<String>,
    pub fence: Option<String>,
    pub start: Option<bool>,
}

/// One root per line. Blank lines are skipped rather than stored as a source
/// that fetches nothing.
fn lines_of(raw: &Option<String>) -> Vec<String> {
    raw.as_deref()
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

pub async fn docs_corpus_set(state: &SharedState, p: DocsCorpusPatch) -> Result<Value, String> {
    match p.action.trim() {
        "create" => {
            let roots = lines_of(&p.source_root);
            require_all(&[
                ("library", p.library.is_some()),
                ("version", p.version.is_some()),
                ("embed_model", p.embed_model.is_some()),
                ("ingest_model", p.ingest_model.is_some()),
                ("source_root", !roots.is_empty()),
            ])?;
            // The kind is a starting hint — every fetched document is sniffed
            // and read as what it actually is — but a kind the store's CHECK
            // constraint would reject has to fail here, with the four spellings
            // named, rather than as an opaque insert error.
            let kind = p
                .source_kind
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .unwrap_or("llms_txt")
                .to_string();
            if quickdoc_core::ingest::SourceKind::parse(&kind).is_none() {
                return Err(format!(
                    "unknown source_kind '{kind}' (llms_txt|markdown|rustdoc_json|html)"
                ));
            }
            let fence = lines_of(&p.fence);
            let body = crate::web::api_docs::CreateCorpus {
                library: p.library.unwrap_or_default(),
                version: p.version.unwrap_or_default(),
                embed_model: p.embed_model.unwrap_or_default(),
                ingest_model: p.ingest_model.unwrap_or_default(),
                sources: roots
                    .into_iter()
                    .map(|root| crate::web::api_docs::NewSource {
                        root,
                        kind: kind.clone(),
                        fence: fence.clone(),
                    })
                    .collect(),
                // Creating a corpus nobody asked to fill is the unusual case:
                // the wizard's own default is to start, and an agent importing
                // in bulk wants one call per library, not two.
                start: p.start.unwrap_or(true),
            };
            let mut out = crate::web::api_docs::create_corpus_inner(state, body).await?;
            out["next_step"] = json!(if out["job_id"].is_null() {
                "lmgw__docs_ingest action=start — the corpus exists but is empty until it runs"
            } else {
                "poll lmgw__docs_corpora corpus=<id> — `job` reports the ingest's stage and percent"
            });
            Ok(out)
        }
        "delete" => {
            let c = corpus_arg(state, p.corpus.as_deref()).await?;
            super::backends::to_json(crate::web::api_docs::delete_corpus_inner(state, c.id).await)
        }
        other => Err(format!("unknown action '{other}' (create|delete)")),
    }
}

/// Start, re-embed or cancel a corpus's job. Each returns as soon as the job is
/// queued: the work outlives the call, which is what [`docs_corpora`] reports.
pub async fn docs_ingest(
    state: &SharedState,
    corpus: Option<&str>,
    action: &str,
    embed_model: Option<&str>,
) -> Result<Value, String> {
    let c = corpus_arg(state, corpus).await?;
    match action.trim() {
        "start" => {
            let spawn = crate::quickdoc::ingest::start(state, &c).await?;
            Ok(json!({
                "ok": true,
                "job_id": spawn.id(),
                "already_running": matches!(spawn, jobs::Spawn::AlreadyRunning(_)),
                "message": format!("ingesting {}", c.corpus_id()),
                "next_step": "poll lmgw__docs_corpora corpus=<id>",
            }))
        }
        "re_embed" => {
            let spawn = crate::quickdoc::reembed::start(
                state,
                &c,
                embed_model
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(String::from),
            )
            .await?;
            Ok(json!({
                "ok": true,
                "job_id": spawn.id(),
                "already_running": matches!(spawn, jobs::Spawn::AlreadyRunning(_)),
                "message": format!("re-embedding {}", c.corpus_id()),
                "next_step": "poll lmgw__docs_corpora corpus=<id>",
            }))
        }
        "cancel" => {
            let j = corpus_job(state, c.id, true).await.ok_or_else(|| {
                format!(
                    "no ingest or re-embed job is running for {} — nothing to cancel",
                    c.corpus_id()
                )
            })?;
            job_cancel(state, j.id).await
        }
        other => Err(format!("unknown action '{other}' (start|re_embed|cancel)")),
    }
}

/// The `docs__request` queue (quickdoc §7): what agents asked for and missed.
pub async fn docs_requests(state: &SharedState, status: Option<&str>) -> Result<Value, String> {
    let status = status.map(str::trim).unwrap_or_default();
    if !matches!(status, "" | "pending" | "fulfilled" | "dismissed") {
        return Err(format!(
            "unknown status '{status}' (pending|fulfilled|dismissed, or omit for all)"
        ));
    }
    let mut out = crate::web::api_docs::list_requests_inner(state, status).await?;
    out["hint"] = json!(
        "fulfil one by creating the corpus (lmgw__docs_corpus_set action=create): the request \
         closes itself when an ingest for its library@version completes. lmgw__docs_request_set \
         is for the ones you will not ingest."
    );
    Ok(out)
}

pub async fn docs_request_set(
    state: &SharedState,
    id: Option<i64>,
    status: &str,
) -> Result<Value, String> {
    let id = id.ok_or("pass id (from lmgw__docs_requests)")?;
    super::backends::to_json(
        crate::web::api_docs::set_request_status_inner(state, id, status.trim()).await,
    )
}
