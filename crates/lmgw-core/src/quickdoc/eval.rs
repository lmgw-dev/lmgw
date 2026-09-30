//! The `eval_run` job (quickdoc §9c, §10): a corpus's golden queries scored at
//! depth `k`, persisted, and turned into the regression badge.
//!
//! It is a *job* rather than a request handler because it is N retrievals, each
//! with an embedding call in it — a corpus with a few hundred golden queries is
//! minutes of work, and the owner should be able to watch it and stop it.
//!
//! The badge policy lives one layer down, in
//! [`quickdoc_core::store::record_eval_run`], so the tool surface and the UI
//! read the same bit rather than each deciding what "regression" means.

use quickdoc_core::eval;
use quickdoc_core::retrieve::SearchParams;
use quickdoc_core::store::{self as qstore, Corpus, NewEvalRun};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::jobs::{JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use crate::state::SharedState;

/// Request payload of an `eval_run` job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub corpus_id: i64,
    /// Depth hit@k is measured at. Absent takes the owner's default.
    #[serde(default)]
    pub k: Option<usize>,
    /// Stage parameters under test. Absent takes the owner's defaults — the
    /// same ones `docs__query` runs with, which is what makes the score a
    /// statement about what callers actually get.
    #[serde(default)]
    pub params: Option<SearchParams>,
}

/// `done`/`total` are golden queries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Detail {
    pub corpus: String,
    pub k: usize,
    pub hit_at_k: f32,
    pub mrr: f32,
    /// Queries whose every expected chunk is gone (§4) — they score zero for a
    /// reason that is not "retrieval got worse".
    pub orphaned_queries: usize,
    pub rerank_model: Option<String>,
    /// Why the rerank stage did not run, when it did not.
    pub rerank_skipped: Option<String>,
}

/// One live eval per corpus.
pub fn job_key(corpus_id: i64) -> String {
    format!("corpus:{corpus_id}")
}

pub async fn start(
    state: &SharedState,
    corpus: &Corpus,
    k: Option<usize>,
    params: Option<SearchParams>,
) -> Result<Spawn, String> {
    crate::jobs::spawn(
        state,
        JobKind::EvalRun,
        Some(job_key(corpus.id)),
        format!("eval {}", corpus.corpus_id()),
        json!({ "corpus_id": corpus.id, "k": k, "params": params }),
    )
    .await
}

pub struct EvalRunExecutor;

#[async_trait::async_trait]
impl JobExecutor for EvalRunExecutor {
    fn kind(&self) -> JobKind {
        JobKind::EvalRun
    }

    async fn run(&self, ctx: JobCtx, input: Value) -> Result<JobOutcome, String> {
        let input: Input =
            serde_json::from_value(input).map_err(|e| format!("eval_run input: {e}"))?;
        let corpus = qstore::get_corpus(&ctx.state.corpus, input.corpus_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("corpus {} no longer exists", input.corpus_id))?;
        run_eval(&ctx, &corpus, input).await
    }
}

async fn run_eval(ctx: &JobCtx, corpus: &Corpus, input: Input) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    let snap = state.snapshot();
    let k = input
        .k
        .filter(|k| *k > 0)
        .unwrap_or(snap.settings.docs_search.eval_k.max(1) as usize);
    let base = input
        .params
        .unwrap_or_else(|| super::query::default_params(&snap));
    let params = eval::params_at_k(&base, k);

    let golden = qstore::list_golden_queries(&state.corpus, corpus.id)
        .await
        .map_err(|e| e.to_string())?;
    if golden.is_empty() {
        // Scoring nothing would write a 0.0 that reads as "retrieval is
        // broken". An eval with no golden queries is a missing input.
        return Err(format!(
            "corpus {} has no golden queries — add some before evaluating it",
            corpus.corpus_id()
        ));
    }

    let retriever = super::query::open_retriever(state, corpus)
        .await
        .map_err(|e| e.to_string())?;
    let mut detail = Detail {
        corpus: corpus.corpus_id(),
        k,
        rerank_model: retriever.rerank_model(),
        rerank_skipped: retriever.rerank_unavailable().map(String::from),
        ..Default::default()
    };
    let total = golden.len() as u64;
    ctx.progress(progress(0, total, "scoring", &detail)).await;

    let mut per_query = Vec::with_capacity(golden.len());
    for (i, g) in golden.iter().enumerate() {
        // Cancel lands between queries: a partial eval is not a score, so
        // nothing is written and the badge keeps whatever it had.
        if ctx.canceled() {
            ctx.progress(progress(i as u64, total, "canceled", &detail))
                .await;
            return Ok(JobOutcome::Canceled);
        }
        let score = eval::score_query(&retriever, g, &params)
            .await
            .map_err(|e| format!("scoring {:?}: {e}", g.query))?;
        per_query.push(score);
        ctx.progress(progress(i as u64 + 1, total, "scoring", &detail))
            .await;
    }

    let report = eval::report(corpus.corpus_id(), k, per_query);
    detail.hit_at_k = report.hit_at_k;
    detail.mrr = report.mrr;
    detail.orphaned_queries = report.orphaned_queries;

    let run = qstore::record_eval_run(
        &state.corpus,
        &NewEvalRun {
            corpus_id: corpus.id,
            k: k as i64,
            queries: report.queries as i64,
            hit_at_k: report.hit_at_k as f64,
            mrr: report.mrr as f64,
            orphaned_queries: report.orphaned_queries as i64,
            params: serde_json::to_value(&params).unwrap_or(Value::Null),
            report: serde_json::to_value(&report).unwrap_or(Value::Null),
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    ctx.progress(progress(total, total, "done", &detail)).await;
    Ok(JobOutcome::Done(json!({
        "corpus_id": corpus.id,
        "eval_run_id": run.id,
        "k": k,
        "queries": report.queries,
        "hit_at_k": report.hit_at_k,
        "mrr": report.mrr,
        "orphaned_queries": report.orphaned_queries,
        "regression": run.regression,
    })))
}

fn progress(done: u64, total: u64, stage: &str, detail: &Detail) -> JobProgress {
    JobProgress {
        done,
        total: Some(total),
        stage: stage.to_string(),
        detail: serde_json::to_value(detail).unwrap_or(Value::Null),
    }
}
