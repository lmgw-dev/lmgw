//! Documentation corpora (quickdoc §10)
//!
//! The dashboard owns corpus creation because the owner decides what a corpus
//! is (`web::api_docs`), and that stays true — these functions are the *owner's*
//! hands on the admin plane, not the agent-facing `docs__*` toolset on `/mcp`.
//! What they add is bulk: importing forty libraries through a wizard is forty
//! manual passes, while an agent holding the request queue can do the same run
//! unattended. Every one of them calls the dashboard's handler.

use lmgw_api_types::self_admin::{
    CorpusJob, CorpusWithJob, DocsCorpora, DocsCorpusList, DocsCorpusOne, DocsCorpusSetAnswer,
    DocsIngestAnswer, DocsIngestStarted, DocsRequestQueue,
};
use lmgw_api_types::{Ack, WithNextStep};
use serde::Deserialize;

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

/// A job as the corpus listing shows it: how far it got, not what it was
/// asked.
fn job_view(j: jobs::JobView) -> CorpusJob {
    let jobs::JobView {
        id,
        kind,
        key: _,
        label: _,
        status,
        done,
        total,
        percent,
        stage,
        detail: _,
        error,
        created_at: _,
        started_at,
        finished_at,
    } = j;
    CorpusJob {
        id,
        kind,
        status,
        stage,
        done,
        total,
        percent,
        error,
        started_at,
        finished_at,
    }
}

/// Every corpus, or one named corpus, each with the job attached to it.
///
/// Documents and chunks are deliberately not here: they are the dashboard's
/// corpus browser (`GET /api/docs/corpora/{id}/documents`), and a corpus with
/// two thousand pages would otherwise answer a routine "how is the import
/// going" with two thousand rows.
pub async fn docs_corpora(
    state: &SharedState,
    corpus: Option<&str>,
) -> Result<DocsCorpora, String> {
    let sel = corpus.map(str::trim).filter(|s| !s.is_empty());
    if let Some(sel) = sel {
        let c = crate::web::api_docs::corpus_by_selector(state, sel).await?;
        let view = crate::web::api_docs::corpus_view(state, &c).await?;
        let job = corpus_job(state, c.id, false).await.map(job_view);
        return Ok(DocsCorpora::One(Box::new(DocsCorpusOne {
            corpus: CorpusWithJob { corpus: view, job },
        })));
    }

    let overview = crate::web::api_docs::list_corpora_inner(state).await?;
    let mut corpora = Vec::with_capacity(overview.corpora.len());
    for corpus in overview.corpora {
        let job = corpus_job(state, corpus.id, false).await.map(job_view);
        corpora.push(CorpusWithJob { corpus, job });
    }
    Ok(DocsCorpora::List(DocsCorpusList {
        corpora,
        pending_requests: overview.pending_requests,
        rerank_model: overview.rerank_model,
        schema_version: overview.schema_version,
        hint: "`status` is the corpus's own state (ready|ingesting|failed|re_embed_required); `job` is the ingest or \
               re-embed run behind it. `flags`/`warnings` say when a corpus is servable but degraded — \
               a re-embed required, an eval regression. `pending_requests` is the queue in \
               lmgw__docs_requests."
            .into(),
    }))
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

pub async fn docs_corpus_set(
    state: &SharedState,
    p: DocsCorpusPatch,
) -> Result<DocsCorpusSetAnswer, String> {
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
            let created = crate::web::api_docs::create_corpus_inner(state, body).await?;
            let next_step = if created.job_id.is_none() {
                "lmgw__docs_ingest action=start — the corpus exists but is empty until it runs"
            } else {
                "poll lmgw__docs_corpora corpus=<id> — `job` reports the ingest's stage and percent"
            };
            Ok(DocsCorpusSetAnswer::Created(Box::new(WithNextStep::new(
                created, next_step,
            ))))
        }
        "delete" => {
            let c = corpus_arg(state, p.corpus.as_deref()).await?;
            Ok(DocsCorpusSetAnswer::Deleted(
                crate::web::api_docs::delete_corpus_inner(state, c.id)
                    .await
                    .map_err(|refusal| refusal.message)?,
            ))
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
) -> Result<DocsIngestAnswer, String> {
    let c = corpus_arg(state, corpus).await?;
    match action.trim() {
        "start" => {
            let spawn = crate::quickdoc::ingest::start(state, &c).await?;
            Ok(DocsIngestAnswer::Started(ingest_started(
                spawn,
                format!("ingesting {}", c.corpus_id()),
            )))
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
            Ok(DocsIngestAnswer::Started(ingest_started(
                spawn,
                format!("re-embedding {}", c.corpus_id()),
            )))
        }
        "cancel" => {
            let j = corpus_job(state, c.id, true).await.ok_or_else(|| {
                format!(
                    "no ingest or re-embed job is running for {} — nothing to cancel",
                    c.corpus_id()
                )
            })?;
            Ok(DocsIngestAnswer::Canceled(job_cancel(state, j.id).await?))
        }
        other => Err(format!("unknown action '{other}' (start|re_embed|cancel)")),
    }
}

/// The job a start or re-embed queued, or the one already in flight.
fn ingest_started(spawn: jobs::Spawn, message: String) -> DocsIngestStarted {
    DocsIngestStarted {
        ok: true,
        job_id: spawn.id(),
        already_running: matches!(spawn, jobs::Spawn::AlreadyRunning(_)),
        message,
        next_step: "poll lmgw__docs_corpora corpus=<id>".into(),
    }
}

/// The `docs__request` queue (quickdoc §7): what agents asked for and missed.
pub async fn docs_requests(
    state: &SharedState,
    status: Option<&str>,
) -> Result<DocsRequestQueue, String> {
    // `list_requests_inner` validates the status, so both planes refuse alike.
    let status = status.map(str::trim).unwrap_or_default();
    Ok(DocsRequestQueue {
        response: crate::web::api_docs::list_requests_inner(state, status).await?,
        hint: "fulfil one by creating the corpus (lmgw__docs_corpus_set action=create): the \
               request closes itself when an ingest for its library@version completes. \
               lmgw__docs_request_set is for the ones you will not ingest."
            .into(),
    })
}

pub async fn docs_request_set(
    state: &SharedState,
    id: Option<i64>,
    status: &str,
) -> Result<Ack, String> {
    let id = id.ok_or("pass id (from lmgw__docs_requests)")?;
    crate::web::api_docs::set_request_status_inner(state, id, status.trim()).await
}
