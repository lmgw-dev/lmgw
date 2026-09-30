//! The synthetic golden-query contract (§10, §11).
//!
//! ```text
//!  code   sample chunks out of the corpus
//!  model  emit_queries: questions the chunk it was shown answers
//!  code   check them, attach the chunk id itself, file them as candidates
//!  owner  accept → a real golden query · reject → discarded
//! ```
//!
//! The same split of duties ingestion runs under (§8): the model writes only
//! the one thing a model is good at here — the question a reader would have
//! asked — and **never names a chunk id**. Code attaches the id of the chunk it
//! showed the model, so a candidate's expectation cannot be hallucinated; the
//! worst a bad generation can produce is a bad *question*, which is exactly what
//! the curation queue is for.
//!
//! Nothing here writes a `golden_query`. A candidate is a proposal and stays one
//! until the owner accepts it (§11): a generated query that scored itself would
//! be a benchmark marking its own homework.
//!
//! Unlike [`ingest::prompt`](crate::ingest::prompt), the prompt below is **not**
//! versioned. A corpus is a function of its ingest prompt because the prompt
//! decides what the stored text is; candidates pass through a human before they
//! decide anything, so the prompt is an implementation detail rather than part
//! of a corpus's identity.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Name of the tool a generation run is driven through. As in extraction, the
/// tool's parameter schema *is* the constrained-output schema, and going through
/// a tool keeps a refusal conversational — a rejected query comes back as a tool
/// result the model can correct.
pub const EMIT_TOOL: &str = "emit_queries";

/// One query the model proposes about the chunk it was shown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedQuery {
    pub query: String,
    /// Why that chunk answers it, in the model's words. Stored beside the
    /// candidate as the context the owner curates with.
    #[serde(default)]
    pub rationale: String,
}

/// The `emit_queries` payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Proposal {
    #[serde(default)]
    pub queries: Vec<ProposedQuery>,
}

/// JSON schema of [`Proposal`], handed to the model as the tool's parameters.
///
/// `want` is how many queries the run asked for per chunk. It is written into
/// the description rather than enforced as `maxItems`: the number is the
/// owner's request, and a model that returns one more is not a protocol error —
/// every query it returns still goes through the same checks and the same queue.
pub fn emit_schema(want: usize) -> Value {
    json!({
        "type": "object",
        "properties": {
            "queries": {
                "type": "array",
                "description": format!(
                    "{want} question(s) that the section you were shown answers."
                ),
                "items": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description":
                                "The question, in the words a developer would search with. \
                                 Not a sentence copied out of the section."
                        },
                        "rationale": {
                            "type": "string",
                            "description":
                                "One sentence: what in the section answers the question."
                        }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["queries"],
        "additionalProperties": false
    })
}

/// A proposal that passed the checks.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedQuery {
    pub query: String,
    pub rationale: String,
}

/// One that did not, with the reason the model is told.
#[derive(Debug, Clone, PartialEq)]
pub struct Rejection {
    /// Position in the emitted `queries` array, so the model knows which one.
    pub index: usize,
    pub reason: String,
}

/// Outcome of one `emit_queries` call.
#[derive(Debug, Clone, Default)]
pub struct Verdict {
    pub accepted: Vec<AcceptedQuery>,
    pub rejected: Vec<Rejection>,
    /// How many of the rejections were queries this corpus already asks. Counted
    /// apart because a duplicate is the run doing its job (the corpus already
    /// covers that ground), not the model failing the contract.
    pub duplicates: usize,
}

/// The dedup key: a query is "already asked" regardless of case and spacing.
pub fn normalize(query: &str) -> String {
    query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Check one `emit_queries` call against the chunk it was written from.
///
/// `taken` holds the normalized form of every query this corpus already
/// asks — its golden queries and its earlier candidates — so a run never files
/// the same question twice. It is **not** mutated here; the caller adds what it
/// accepted, because only the caller knows whether the row was really written.
///
/// A rejection is never repaired: it comes back with a reason naming what was
/// wrong so the model can re-emit, exactly as a rejected span does in §8.
pub fn validate(payload: &str, taken: &HashSet<String>, p: &Proposal) -> Verdict {
    let mut v = Verdict::default();
    let haystack = payload.to_lowercase();
    for (i, q) in p.queries.iter().enumerate() {
        let query = q.query.trim();
        let reject = |reason: String| Rejection { index: i, reason };
        if query.is_empty() {
            v.rejected.push(reject(
                "the query is empty — write the question a reader would ask".into(),
            ));
            continue;
        }
        // A question that appears in the section word for word is found by BM25
        // alone whatever retrieval does, so it would measure nothing but its own
        // presence. This is the small-model failure mode here: copying a
        // sentence back instead of asking about it.
        if haystack.contains(&query.to_lowercase()) {
            v.rejected.push(reject(format!(
                "{query:?} appears in the section word for word — that is a copy of the \
                 text, not a question about it. Ask what a reader who had not seen this \
                 section would type."
            )));
            continue;
        }
        let key = normalize(query);
        if taken.contains(&key) || v.accepted.iter().any(|a| normalize(&a.query) == key) {
            v.duplicates += 1;
            v.rejected.push(reject(format!(
                "this corpus already asks {query:?} — ask about a different aspect of \
                 the section instead"
            )));
            continue;
        }
        v.accepted.push(AcceptedQuery {
            query: query.to_string(),
            rationale: q.rationale.trim().to_string(),
        });
    }
    v
}

/// What the model is told after an `emit_queries` call — the retry channel.
pub fn verdict_report(v: &Verdict) -> String {
    let mut s = format!(
        "accepted {} question(s), rejected {}.",
        v.accepted.len(),
        v.rejected.len()
    );
    if v.rejected.is_empty() {
        s.push_str(" Nothing left to correct — reply with one short sentence and stop.");
        return s;
    }
    for r in &v.rejected {
        s.push_str(&format!("\n- query {}: {}", r.index, r.reason));
    }
    s.push_str("\nCall emit_queries again with ONLY replacements for the rejected ones.");
    s
}

/// The per-chunk turn: which corpus this is, and the one section to write
/// questions about.
pub fn chunk_turn(corpus_id: &str, heading_path: &str, payload: &str, want: usize) -> String {
    let heading = if heading_path.trim().is_empty() {
        "(no heading)".to_string()
    } else {
        heading_path.trim().to_string()
    };
    format!(
        "Library: {corpus_id}\nSection: {heading}\n\n\
         Call {EMIT_TOOL} with {want} question(s) this section answers.\n\n\
         ---\n{payload}\n---"
    )
}

pub const SYSTEM: &str = "\
You write the test questions a documentation search is measured with. You are \
shown one section of a library's documentation at a time; for each, you write \
the question a developer would have typed into a search box to end up at that \
section.

Call `emit_queries` with your questions:

- Write what someone would ask BEFORE they had read the section. They do not \
know its wording, its heading, or the names it introduces unless those names \
are the thing they are looking for.
- Never copy a sentence out of the section. A question that appears in the text \
word for word is rejected: it would be found by any keyword search and \
therefore measures nothing.
- One question per idea, phrased naturally: 'how do I return JSON from a \
handler', not 'JSON handler IntoResponse'.
- If the section is navigation, a changelog entry or otherwise carries nothing \
a reader would search for, emit no queries at all rather than inventing one.
- `rationale`: one sentence naming what in the section answers it. It is shown \
to the person deciding whether to keep your question.

Every question is reviewed by the library's owner before it is used, so a \
question you are unsure about is better dropped than filed. When your questions \
have been accepted, reply with one short sentence and stop.";

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &str = "Use `Json(value)` as the return type to answer with JSON. \
                           The extractor sets the content type for you.";

    fn proposal(queries: &[&str]) -> Proposal {
        Proposal {
            queries: queries
                .iter()
                .map(|q| ProposedQuery {
                    query: (*q).to_string(),
                    rationale: "because".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_question_copied_out_of_the_section_is_refused_by_name() {
        let v = validate(
            PAYLOAD,
            &HashSet::new(),
            &proposal(&["The extractor sets the content type for you."]),
        );
        assert!(v.accepted.is_empty());
        assert_eq!(v.rejected.len(), 1);
        assert!(v.rejected[0].reason.contains("word for word"), "{v:?}");
        assert!(verdict_report(&v).contains("emit_queries again"));
    }

    #[test]
    fn duplicates_are_counted_apart_from_contract_failures() {
        let mut taken = HashSet::new();
        taken.insert(normalize("How do I  Return JSON?"));
        let v = validate(
            PAYLOAD,
            &taken,
            &proposal(&["how do I return json?", "", "how do I set a status code?"]),
        );
        assert_eq!(v.accepted.len(), 1);
        assert_eq!(v.accepted[0].query, "how do I set a status code?");
        assert_eq!(v.rejected.len(), 2);
        assert_eq!(v.duplicates, 1, "the empty one is not a duplicate");
    }

    /// Two identical questions in one call are the same duplicate as one that
    /// was filed an hour ago.
    #[test]
    fn a_call_cannot_propose_the_same_question_twice() {
        let v = validate(
            PAYLOAD,
            &HashSet::new(),
            &proposal(&["how do I return json", "How do I return JSON"]),
        );
        assert_eq!(v.accepted.len(), 1);
        assert_eq!(v.duplicates, 1);
    }

    #[test]
    fn the_turn_names_the_corpus_the_section_and_how_many_are_wanted() {
        let turn = chunk_turn("axum@0.8", "Responses > JSON", PAYLOAD, 2);
        assert!(turn.contains("axum@0.8"), "{turn}");
        assert!(turn.contains("Responses > JSON"), "{turn}");
        assert!(turn.contains("2 question(s)"), "{turn}");
        assert!(turn.contains(PAYLOAD), "{turn}");
        assert!(chunk_turn("axum@0.8", "  ", PAYLOAD, 1).contains("(no heading)"));
        assert!(emit_schema(3)["properties"]["queries"]["description"]
            .as_str()
            .unwrap()
            .contains('3'));
    }
}
