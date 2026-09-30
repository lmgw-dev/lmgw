//! Answering a question: budget, retrieve, build the prompt, and hand back the
//! citations the UI links to.
//!
//! The prompt is three parts: a short system prompt ([`SYSTEM_PROMPT`]); the
//! conversation so far, verbatim; and one final user message carrying the
//! numbered excerpts and the question. The excerpts are chosen best first and
//! shown in reading order ([`reading_order`]). Earlier turns do **not** carry their
//! excerpts again — they were evidence for earlier answers, and resending them
//! would spend the budget twice.
//!
//! Sizes are measured with [`crate::chunk::estimator`], the one the chunks are
//! cut with, and everything that limited this answer is in its
//! [`AnswerMeta`]: the [`Budget`], how many excerpts the search offered and
//! how many the budget let through, [`MAX_EXCERPTS`], the retrieval depths
//! ([`retrieval_note`]: quickdoc's `k_fts`, `k_vec`, `rrf_k`, `k_rerank`), and
//! any stand-in number.

use futures::stream::BoxStream;
use quickdoc_core::embed::TokenCounter;
use quickdoc_core::retrieve::{Retriever, SearchParams, SearchResult};
use serde::{Deserialize, Serialize};

use crate::budget::{self, Budget, BudgetError, Reserve, MESSAGE_FRAMING_TOKENS};
use crate::gateway::{rerank_fallback_alias, ChatChunk, GatewayError, ModelInfo, WireMessage};

/// The system prompt, whole. Short on purpose: every token here is one the
/// excerpts cannot use. It says how the excerpts are ordered ([`reading_order`])
/// so the model reads one file's excerpts as a sequence, says what an excerpt
/// headed `read by <model>` is ([`crate::vision`]: a vision model's reading of
/// a page image, which can misread what the file's own text has right), and
/// asks for the question's language, which the English prompt would otherwise
/// win over when the excerpts do not answer.
pub const SYSTEM_PROMPT: &str = "You answer questions about the files in the user's folder, \
using only the numbered excerpts given with each question. Excerpts from the same file come \
together, in the order they appear in that file. Cite every claim with its excerpt number in \
square brackets, like [2], and the first time you cite an excerpt name its file path and its \
heading or page. An excerpt headed \"read by <model>\" is that model's reading of the page \
image, not the file's own text: its names and numbers can be misread, and where the file's own \
text is also given, rely on that. If the excerpts do not contain the answer, say plainly that the \
indexed files do not answer it; do not guess and do not use outside knowledge. Answer in the \
language the question is asked in.";

/// The most excerpts one answer is offered, before the budget trims further —
/// quickdoc's `SearchParams.limit`. Twelve is enough for an answer that draws
/// on several files; beyond that, a small local model's attention spreads thin
/// and the answer gets worse, not better. Carried in every [`AnswerMeta`].
pub const MAX_EXCERPTS: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One earlier turn, as the UI keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Earlier turns, oldest first, without their excerpts.
    #[serde(default)]
    pub history: Vec<ChatMessage>,
    pub question: String,
}

/// One excerpt the answer may cite as `[n]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    /// The `[n]` the model was shown.
    pub n: usize,
    /// Relative to the folder.
    pub path: String,
    /// `# A > ## B`, `page N`, or empty.
    pub heading_path: String,
    /// The page, for a PDF excerpt.
    pub page: Option<u32>,
    /// Byte offsets into the file's text as it was indexed (pdftotext's
    /// output for a PDF).
    pub span_start: i64,
    pub span_end: i64,
    /// 1-based, inclusive lines of the excerpt in that same text — what the
    /// MCP `read` tool's `start_line` / `end_line` take. They are the lines
    /// **of the file as it was indexed**, stored with the chunk by the sync
    /// ([`crate::index::IndexDir::fill_stored_lines`]): no file is read for
    /// them, so a file edited since the last sync is cited where the excerpt
    /// was then, and the next sync brings both up to date. `None` only when
    /// the index holds no lines for the chunk. Filled in by the caller, not
    /// by [`prepare`].
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub end_line: Option<usize>,
    pub chunk_id: String,
    /// The final ranking score (the reranker's when it ran, RRF's otherwise).
    pub score: f32,
    /// The excerpt as the model saw it.
    pub text: String,
}

/// Everything about how an answer was put together, for the UI to print.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnswerMeta {
    pub chat_model: String,
    pub budget: Budget,
    pub citations: Vec<Citation>,
    /// [`MAX_EXCERPTS`].
    pub max_excerpts: usize,
    /// Excerpts the search ranked (at most `max_excerpts`).
    pub excerpts_found: usize,
    /// Of those, the ones the retrieval budget had no room for — quickdoc's
    /// own budget pass plus the re-measure of the rendered prompt. The rest
    /// are `citations`.
    pub excerpts_dropped_for_budget: usize,
    pub rerank_model: Option<String>,
    /// Why reranking did not run, when it was asked for.
    pub rerank_skipped: Option<String>,
    /// Stand-ins and anything else worth a line.
    pub notes: Vec<String>,
    /// `x-lmgw-fallback` — the alias that actually answered, when a held local
    /// model fell back.
    pub served_by_fallback: Option<String>,
}

/// A prompt ready to send.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub messages: Vec<WireMessage>,
    pub meta: AnswerMeta,
}

/// An answer as it starts streaming.
pub struct Answer {
    pub meta: AnswerMeta,
    pub chunks: BoxStream<'static, Result<ChatChunk, GatewayError>>,
}

#[derive(Debug, thiserror::Error)]
pub enum AskError {
    #[error("there is no index yet; run a sync first")]
    NoIndex,
    /// The index was loaded while the embedding model was held, and the
    /// first probe after the hold found another model behind the alias.
    #[error(
        "the embedding alias {alias} now names another model than the one the index was \
         built with ({detail}); the index was unloaded and the next sync rebuilds it"
    )]
    IndexModelChanged { alias: String, detail: String },
    #[error("the question is empty")]
    EmptyQuestion,
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("retrieval failed: {0}")]
    Retrieval(String),
    #[error(transparent)]
    Gateway(#[from] GatewayError),
}

fn tokens(tc: &dyn TokenCounter, text: &str) -> u64 {
    tc.count(text) as u64
}

/// The final user message: excerpts, then the question. With no excerpts
/// (nothing matched, or nothing fitted) it says "none", and the system prompt
/// tells the model what to answer then. The empty form is also the longer
/// wrapper, which is why the history budget measures that one.
pub fn user_message(excerpts: &str, question: &str) -> String {
    if excerpts.is_empty() {
        format!("Excerpts: none.\n\nQuestion: {question}")
    } else {
        format!("Excerpts:\n\n{excerpts}\n\nQuestion: {question}")
    }
}

/// One excerpt as the model sees it.
pub fn render_excerpt(n: usize, path: &str, heading_path: &str, text: &str) -> String {
    if heading_path.is_empty() {
        format!("[{n}] {path}\n{text}")
    } else {
        format!("[{n}] {path} — {heading_path}\n{text}")
    }
}

/// The order the chosen excerpts are shown in: grouped by file, the files in
/// the order of their best excerpt, and each file's excerpts by their place in
/// it. `picked` is `(path, span_start)` best first; the result indexes into it.
///
/// Rank order scatters a file: a table's header row and the rows under it are
/// often separate excerpts, and a model shown the rows first and the header
/// later reads the wrong column. The ranking has done its work once it chose
/// the excerpts; the model reads them in the file's order.
pub fn reading_order(picked: &[(&str, i64)]) -> Vec<usize> {
    let mut first: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, (path, _)) in picked.iter().enumerate() {
        first.entry(path).or_insert(i);
    }
    let mut order: Vec<usize> = (0..picked.len()).collect();
    order.sort_by_key(|&i| (first[picked[i].0], picked[i].1));
    order
}

/// The page of a PDF excerpt, from its heading: `page N` for the PDF's own
/// text, `page N, read by <alias>` for a vision model's reading of it
/// ([`crate::vision::heading`]).
pub fn page_of(heading_path: &str) -> Option<u32> {
    let rest = heading_path.strip_prefix("page ")?;
    let (number, after) = rest.split_at(
        rest.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len()),
    );
    if !(after.is_empty() || after.starts_with(", read by ")) {
        return None;
    }
    number.parse().ok()
}

/// A search, and what the agent had to change about it.
pub struct Searched {
    pub result: SearchResult,
    /// Set when the rerank was answered through a hold fallback and dropped.
    pub rerank_dropped: Option<RerankDropped>,
}

/// What a question whose rerank reached a hold fallback says about it.
pub struct RerankDropped {
    /// Its `rerank_skipped`: why the answer was not reranked.
    pub reason: String,
    /// Its note ([`rerank_fallback_note`]): the fallback, and that the
    /// excerpts reached it.
    pub note: String,
}

impl Searched {
    /// Why reranking did not run, when it was asked for: the drop, or
    /// quickdoc's own reason.
    pub fn rerank_skipped(&self) -> Option<String> {
        match &self.rerank_dropped {
            Some(d) => Some(d.reason.clone()),
            None => self.result.trace.rerank_skipped.clone(),
        }
    }
}

/// `retriever.search`, except for a rerank answered through a hold fallback
/// ([`crate::gateway::RERANK_FALLBACK_PREFIX`]): its scores are not used — the
/// search runs again without the rerank stage, so the question keeps the
/// fused order — and [`Searched::rerank_dropped`] says so, and where the
/// excerpts went. Running it again embeds the query a second time, through
/// the embedding alias (the fallback answered only the rerank).
pub async fn search(
    retriever: &Retriever,
    query: &str,
    params: &SearchParams,
) -> Result<Searched, AskError> {
    match retriever.search(query, params).await {
        Ok(result) => Ok(Searched {
            result,
            rerank_dropped: None,
        }),
        Err(e) => {
            let Some(fallback) = rerank_fallback_alias(&e) else {
                return Err(AskError::Retrieval(e.to_string()));
            };
            let fused = SearchParams {
                rerank: false,
                ..params.clone()
            };
            let result = retriever
                .search(query, &fused)
                .await
                .map_err(|e| AskError::Retrieval(e.to_string()))?;
            let rerank = retriever.rerank_model().unwrap_or_default();
            Ok(Searched {
                result,
                rerank_dropped: Some(RerankDropped {
                    reason: format!(
                        "{rerank} is held and lmgw answered with its fallback '{fallback}', \
                         whose scores were not used"
                    ),
                    note: rerank_fallback_note(&rerank, &fallback),
                }),
            })
        }
    }
}

/// What a question whose rerank reached a hold fallback says.
pub fn rerank_fallback_note(rerank_model: &str, fallback: &str) -> String {
    format!(
        "the excerpts of this question reached the fallback '{fallback}': the rerank model \
         {rerank_model} is held and lmgw sent the rerank request to its fallback before the \
         agent could see it. Its scores were not used; the excerpts keep the fused BM25 + \
         vector order. Pick a rerank alias without a cloud fallback to keep this folder on this \
         machine"
    )
}

/// quickdoc's retrieval depths one search runs with, by their `SearchParams`
/// names: BM25 candidates, KNN candidates, the RRF damping constant, the
/// rerank window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Depths {
    pub k_fts: usize,
    pub k_vec: usize,
    pub rrf_k: f32,
    pub k_rerank: usize,
}

impl From<&SearchParams> for Depths {
    fn from(p: &SearchParams) -> Self {
        Self {
            k_fts: p.k_fts,
            k_vec: p.k_vec,
            rrf_k: p.rrf_k,
            k_rerank: p.k_rerank,
        }
    }
}

/// The retrieval depths one search ran with, named as quickdoc names them —
/// a line of every answer's notes.
pub fn retrieval_note(params: &SearchParams, reranked_by: Option<&str>) -> String {
    let rerank = match reranked_by {
        Some(m) => format!(
            "the top k_rerank {} of the fused list reranked by {m}, the rest in fused order",
            params.k_rerank
        ),
        None => format!("no rerank (k_rerank {} unused)", params.k_rerank),
    };
    format!(
        "retrieval: the k_fts {} best BM25 matches and the k_vec {} nearest vectors, fused by \
         reciprocal rank (rrf_k {}); {rerank}; up to limit {} excerpts offered (quickdoc \
         SearchParams)",
        params.k_fts, params.k_vec, params.rrf_k, params.limit
    )
}

/// Budget, retrieve and build the prompt for one question.
///
/// `params` is the search as the caller wants it; its `limit` defaults to
/// [`MAX_EXCERPTS`] through [`default_search_params`], and its
/// `budget_tokens` is overwritten with the derived retrieval budget.
/// `chunk_tokens` is one excerpt's size, which a reported `max_output_tokens`
/// must leave room for to be the answer reserve ([`budget::reserve_for`]).
pub async fn prepare(
    retriever: &Retriever,
    model: &ModelInfo,
    req: &ChatRequest,
    tc: &dyn TokenCounter,
    mut params: SearchParams,
    chunk_tokens: usize,
) -> Result<Prepared, AskError> {
    let question = req.question.trim();
    if question.is_empty() {
        return Err(AskError::EmptyQuestion);
    }
    let system_tokens = tokens(tc, SYSTEM_PROMPT) + MESSAGE_FRAMING_TOKENS;
    let history_tokens = req
        .history
        .iter()
        .map(|m| tokens(tc, &m.content) + MESSAGE_FRAMING_TOKENS)
        .sum::<u64>()
        + tokens(tc, &user_message("", question))
        + MESSAGE_FRAMING_TOKENS;
    let budget = budget::derive(
        model,
        system_tokens,
        history_tokens,
        chunk_tokens as u64,
        &tc.name(),
    )?;

    let mut notes = Vec::new();
    if !budget.context_reported {
        notes.push(format!(
            "{} reports no context_length; the budget assumes {} tokens \
             (FALLBACK_CONTEXT_TOKENS)",
            model.alias,
            budget::FALLBACK_CONTEXT_TOKENS
        ));
    }
    let instead = format!(
        "{} tokens are kept free for the answer instead (DEFAULT_ANSWER_RESERVE_TOKENS) — not a \
         cap on it",
        budget::DEFAULT_ANSWER_RESERVE_TOKENS
    );
    match budget::reserve_for(
        model.max_output_tokens,
        budget.context_tokens,
        budget.system_tokens,
        budget.excerpt_tokens,
    ) {
        Reserve::Reported(_) => {}
        Reserve::Unreported => notes.push(format!(
            "{} reports no max_output_tokens; {} tokens are kept free for the answer \
             (DEFAULT_ANSWER_RESERVE_TOKENS) — not a cap on it",
            model.alias,
            budget::DEFAULT_ANSWER_RESERVE_TOKENS
        )),
        Reserve::NoRoom(m) if m >= budget.context_tokens => notes.push(format!(
            "{} reports max_output_tokens {m}, which is its whole context ({} tokens); {instead}",
            model.alias, budget.context_tokens
        )),
        Reserve::NoRoom(m) => notes.push(format!(
            "{} reports max_output_tokens {m}, which leaves no room in its {}-token context for \
             the system prompt ({}) and one excerpt (chunk_tokens, {}); {instead}",
            model.alias, budget.context_tokens, budget.system_tokens, budget.excerpt_tokens
        )),
    }

    params.budget_tokens = Some(usize::try_from(budget.retrieval_tokens).unwrap_or(usize::MAX));
    let searched = search(retriever, question, &params).await?;
    let rerank_skipped = searched.rerank_skipped();
    let Searched {
        result,
        rerank_dropped,
    } = searched;
    notes.push(retrieval_note(
        &params,
        result.trace.rerank_model.as_deref(),
    ));
    if let Some(d) = rerank_dropped {
        notes.push(d.note);
    }
    let urls = quickdoc_core::store::document_urls(
        retriever.pool(),
        &result
            .hits
            .iter()
            .map(|h| h.chunk.id.clone())
            .collect::<Vec<_>>(),
    )
    .await
    .map_err(|e| AskError::Retrieval(e.to_string()))?;

    // quickdoc's budget counts heading + payload and lets a lone top hit
    // overrun; the prompt also carries each excerpt's `[n] path` line. So the
    // rendered block is measured again and trimmed from the tail until it
    // truly fits — possibly to nothing, which the notes then say.
    let offered = result.hits.len();
    let mut chosen: Vec<(&quickdoc_core::retrieve::Hit, String)> = Vec::new();
    let mut blocks: Vec<String> = Vec::new();
    for h in &result.hits {
        let path = urls
            .get(&h.chunk.id)
            .cloned()
            .unwrap_or_else(|| h.chunk.derived_title.clone());
        let block = render_excerpt(
            chosen.len() + 1,
            &path,
            &h.chunk.heading_path,
            &h.chunk.payload,
        );
        let mut candidate = blocks.clone();
        candidate.push(block);
        if tokens(tc, &candidate.join("\n\n")) > budget.retrieval_tokens {
            break;
        }
        blocks = candidate;
        chosen.push((h, path));
    }
    // Chosen best first, numbered and shown in reading order. The numbers are
    // still 1..=k, only on other excerpts, so the block has exactly as many
    // characters as the one just measured.
    let picked: Vec<(&str, i64)> = chosen
        .iter()
        .map(|(h, path)| (path.as_str(), h.chunk.span_start))
        .collect();
    let mut citations: Vec<Citation> = Vec::with_capacity(chosen.len());
    let mut blocks: Vec<String> = Vec::with_capacity(chosen.len());
    for k in reading_order(&picked) {
        let (h, path) = &chosen[k];
        let n = citations.len() + 1;
        blocks.push(render_excerpt(
            n,
            path,
            &h.chunk.heading_path,
            &h.chunk.payload,
        ));
        citations.push(Citation {
            n,
            path: path.clone(),
            heading_path: h.chunk.heading_path.clone(),
            page: page_of(&h.chunk.heading_path),
            span_start: h.chunk.span_start,
            span_end: h.chunk.span_end,
            start_line: None,
            end_line: None,
            chunk_id: h.chunk.id.clone(),
            score: h.score,
            text: h.chunk.payload.clone(),
        });
    }
    let dropped = offered - citations.len() + result.trace.budget_dropped.len();
    let found = offered + result.trace.budget_dropped.len();
    if found > 0 && citations.is_empty() {
        notes.push(format!(
            "the retrieval budget ({} tokens) had no room for even the best excerpt; the model \
             was asked without any",
            budget.retrieval_tokens
        ));
    }
    if dropped > 0 {
        notes.push(format!(
            "{dropped} of {found} excerpt(s) did not fit the retrieval budget of {} tokens",
            budget.retrieval_tokens
        ));
    }

    let mut messages = vec![WireMessage {
        role: "system".into(),
        content: SYSTEM_PROMPT.into(),
    }];
    messages.extend(req.history.iter().map(|m| WireMessage {
        role: m.role.as_str().into(),
        content: m.content.clone(),
    }));
    messages.push(WireMessage {
        role: "user".into(),
        content: user_message(&blocks.join("\n\n"), question),
    });

    Ok(Prepared {
        messages,
        meta: AnswerMeta {
            chat_model: model.alias.clone(),
            budget,
            citations,
            max_excerpts: params.limit,
            excerpts_found: found,
            excerpts_dropped_for_budget: dropped,
            rerank_model: result.trace.rerank_model.clone(),
            rerank_skipped,
            notes,
            served_by_fallback: None,
        },
    })
}

/// quickdoc's defaults with this agent's excerpt count, and reranking on
/// exactly when a reranker is attached.
pub fn default_search_params(rerank: bool) -> SearchParams {
    SearchParams {
        limit: MAX_EXCERPTS,
        rerank,
        ..SearchParams::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_of_reads_a_text_page_and_a_reading_of_one() {
        assert_eq!(page_of("page 7"), Some(7));
        assert_eq!(page_of("page 12, read by gemma4-12b"), Some(12));
        assert_eq!(
            page_of(&crate::vision::heading(3, "embed/odd, alias")),
            Some(3)
        );
        assert_eq!(page_of("# Setup > ## SELinux"), None);
        assert_eq!(page_of("page 3b"), None);
        assert_eq!(page_of("page "), None);
        assert_eq!(page_of(""), None);
    }

    #[test]
    fn the_system_prompt_says_what_a_reading_is() {
        assert!(
            SYSTEM_PROMPT.contains("\"read by <model>\""),
            "{SYSTEM_PROMPT}"
        );
        assert!(SYSTEM_PROMPT.contains("rely on that"), "{SYSTEM_PROMPT}");
    }
}
