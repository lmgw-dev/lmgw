//! Knowledge bases in Chat threads (chat-complete design §9.3): which bases a
//! turn uses, the auto-mode retrieval that runs before the model is called,
//! the `<context>` block it becomes in the request, and the checks on the
//! thread's knowledge settings.
//!
//! **Which bases.** A turn uses the thread's `kb_ids` and the `kb_refs` of
//! the user message it answers (picked with `#` for that message alone).
//!
//! **Auto mode** searches them ([`crate::knowledge::retrieve::retrieve`],
//! which never fails) with the user message's text plus the previous user
//! message's, stores the result on the user message (`context`), emits it as
//! the SSE event `retrieval` before the model is called, and sends it as a
//! `<context source="knowledge">` block ahead of the message's text. Every
//! later turn replays that block unchanged from the stored context — the
//! prompt a model saw once is the prompt it sees again, so llama.cpp's
//! prompt cache stays warm and the `[n]` citations keep pointing at the same
//! excerpts.
//!
//! **When a turn searches again** (the reuse rule): a user message that
//! already carries a retrieval which found excerpts, over the same set of
//! bases, keeps it — that is Regenerate, on the reply or on the message
//! itself: the same question, answered again, cites the same sources. An
//! **edited** user message loses its stored retrieval with the edit (the
//! text changed) and is searched afresh; so is a message whose stored
//! retrieval found nothing (a GPU hold, a base still ingesting — trying
//! again is the point), or whose bases changed since. A continue never
//! searches.
//!
//! **Tool mode** attaches the `kb__*` tools instead, restricted to the same
//! bases ([`KbTools`], wired in `agentchat`); nothing is searched up front.

use std::collections::HashSet;

use serde_json::json;

use super::chat_caller::Caller;
use super::chat_live::Ticket;
use super::chat_repo::ChatRepo;
use super::chat_turn::TurnMode;
use crate::ir::ContentPart;
use crate::knowledge::retrieve::{self, Options};
use crate::state::{AppState, SharedState};
use crate::store::{
    ChatContext, ChatMessageRow, ChatThread, ContextExcerpt, KbMode, ThreadDefaults,
};

/// The line that closes every context block, before `</context>`.
pub(super) const CITE_INSTRUCTION: &str =
    "Use these excerpts where they are relevant and cite them as [n].";

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// `ids` deduplicated (first occurrence kept), with every id that is not in
/// `already` naming a base that exists — else the error names the ones that
/// do not. An id the thread (or message, or folder) already carries is not
/// checked again: a base deleted after it was picked must not block saving
/// anything else; the retrieval reports it as gone instead.
pub(super) async fn check_kb_ids(
    state: &AppState,
    ids: &[i64],
    already: &[i64],
) -> Result<Vec<i64>, String> {
    let ids = dedup(ids);
    if ids.iter().all(|id| already.contains(id)) {
        return Ok(ids);
    }
    let known: HashSet<i64> = crate::knowledge::store::list_kbs(&state.knowledge.pool)
        .await
        .map_err(|e| format!("the knowledge bases could not be read: {e}"))?
        .iter()
        .map(|k| k.id)
        .collect();
    let unknown: Vec<String> = ids
        .iter()
        .filter(|id| !already.contains(id) && !known.contains(id))
        .map(i64::to_string)
        .collect();
    if unknown.is_empty() {
        Ok(ids)
    } else {
        Err(format!(
            "no knowledge base with id {} — see the Knowledge page for the ones there are",
            unknown.join(", ")
        ))
    }
}

pub(super) fn parse_mode(s: &str) -> Result<KbMode, String> {
    KbMode::parse(s.trim()).ok_or_else(|| format!("kb_mode must be 'auto' or 'tool' (got '{s}')"))
}

/// A thread's own retrieval budget: above zero, like the setting it
/// overrides; `None` is the setting.
pub(super) fn check_budget(v: Option<i64>) -> Result<(), String> {
    match v {
        Some(n) if n <= 0 => Err(format!(
            "kb_budget_tokens must be a whole number above 0, or null for the \
             chat_kb_budget_tokens setting (got {n})"
        )),
        _ => Ok(()),
    }
}

/// The knowledge half of a thread-settings patch, checked and laid over `t`
/// (absent fields leave `t` alone; `kb_budget_tokens: null` returns to the
/// setting). Nothing is applied when anything is refused.
pub(super) async fn apply_settings(
    state: &AppState,
    t: &mut ChatThread,
    kb_ids: Option<Vec<i64>>,
    kb_mode: Option<String>,
    kb_budget_tokens: Option<Option<i64>>,
) -> Result<(), String> {
    let mode = kb_mode.as_deref().map(parse_mode).transpose()?;
    if let Some(b) = kb_budget_tokens {
        check_budget(b)?;
    }
    let ids = match kb_ids {
        Some(ids) => Some(check_kb_ids(state, &ids, &t.kb_ids).await?),
        None => None,
    };
    if let Some(ids) = ids {
        t.kb_ids = ids;
    }
    if let Some(m) = mode {
        t.kb_mode = m;
    }
    if let Some(b) = kb_budget_tokens {
        t.kb_budget_tokens = b;
    }
    Ok(())
}

/// Folder defaults' knowledge fields, normalised: no bases and auto mode are
/// the global behaviour, not defaults to record.
pub(super) fn check_defaults(d: &mut ThreadDefaults) -> Result<(), String> {
    check_budget(d.kb_budget_tokens)?;
    d.kb_ids = d
        .kb_ids
        .take()
        .map(|ids| dedup(&ids))
        .filter(|ids| !ids.is_empty());
    d.kb_mode = d.kb_mode.filter(|m| *m != KbMode::Auto);
    Ok(())
}

/// The bases a folder's defaults name must exist, except those it already
/// named (`already`).
pub(super) async fn check_default_kbs(
    state: &AppState,
    d: &ThreadDefaults,
    already: &[i64],
) -> Result<(), String> {
    match &d.kb_ids {
        Some(ids) => check_kb_ids(state, ids, already).await.map(|_| ()),
        None => Ok(()),
    }
}

fn dedup(ids: &[i64]) -> Vec<i64> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if !out.contains(id) {
            out.push(*id);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The turn
// ---------------------------------------------------------------------------

/// What knowledge does for one turn.
#[derive(Debug, Default)]
pub(super) struct TurnKb {
    /// Auto mode: the retrieval to run (or reuse) before the model is called.
    pub auto: Option<Auto>,
    /// Tool mode: the `kb__*` tools, restricted to these bases.
    pub tools: Option<KbTools>,
}

/// An auto-mode turn's retrieval.
#[derive(Debug)]
pub(super) enum Auto {
    /// Search now and store the result on user message `message_id`.
    Retrieve {
        message_id: i64,
        /// The message's own `kb_refs`, written back with the context.
        kb_refs: Vec<i64>,
        kb_ids: Vec<i64>,
        query: String,
        budget: usize,
    },
    /// The message's stored retrieval stands (see the module doc).
    Reuse {
        message_id: i64,
        context: ChatContext,
    },
}

/// Tool mode's knowledge bases: what `agentchat` restricts the `kb__*`
/// tools to, and the budget `kb__search` / `kb__read` default to.
#[derive(Debug, Clone, PartialEq)]
pub struct KbTools {
    pub ids: Vec<i64>,
    pub budget: usize,
}

/// Decide a turn's knowledge from the thread and its history as the turn
/// reads it. `default_budget` is the `chat_kb_budget_tokens` setting.
pub(super) fn plan(
    default_budget: u32,
    thread: &ChatThread,
    history: &[ChatMessageRow],
    mode: TurnMode,
) -> TurnKb {
    // The user message this turn answers: on a fresh turn the history ends
    // with it; a continue answers the one before the reply it continues.
    let Some(pos) = history.iter().rposition(|m| m.role == "user") else {
        return TurnKb::default();
    };
    let current = &history[pos];
    let ids = dedup(
        &thread
            .kb_ids
            .iter()
            .chain(&current.kb_refs)
            .copied()
            .collect::<Vec<_>>(),
    );
    if ids.is_empty() {
        return TurnKb::default();
    }
    let budget = thread
        .kb_budget_tokens
        .and_then(|b| usize::try_from(b).ok())
        .filter(|b| *b > 0)
        .unwrap_or(default_budget.max(1) as usize);
    match thread.kb_mode {
        KbMode::Tool => TurnKb {
            auto: None,
            tools: Some(KbTools { ids, budget }),
        },
        KbMode::Auto => {
            // Late MCP task results the turn's start wrote after the user
            // message (an edit's, a regenerate's; a send's enter before it)
            // leave it the message this turn answers (MCP Tasks design
            // §3.1).
            let fresh = matches!(mode, TurnMode::Fresh { .. })
                && history[pos + 1..]
                    .iter()
                    .all(ChatMessageRow::is_task_result);
            if !fresh {
                return TurnKb::default();
            }
            let auto = match current
                .context
                .as_ref()
                .filter(|c| !c.excerpts.is_empty() && same_set(&c.kb_ids, &ids))
            {
                Some(c) => Auto::Reuse {
                    message_id: current.id,
                    context: c.clone(),
                },
                None => {
                    let previous = history[..pos].iter().rev().find(|m| m.role == "user");
                    Auto::Retrieve {
                        message_id: current.id,
                        kb_refs: current.kb_refs.clone(),
                        kb_ids: ids,
                        query: query_of(current, previous),
                        budget,
                    }
                }
            };
            TurnKb {
                auto: Some(auto),
                tools: None,
            }
        }
    }
}

fn same_set(a: &[i64], b: &[i64]) -> bool {
    let a: HashSet<&i64> = a.iter().collect();
    let b: HashSet<&i64> = b.iter().collect();
    a == b
}

/// The search: the previous user message's text, then the current one's — so
/// a follow-up like "and in August?" still finds what the question before it
/// was about. The current message comes **last** because retrieval, when the
/// query is longer than a model takes per input, keeps the end of it
/// ([`crate::knowledge::retrieve_guard`]): the previous message goes first,
/// and only then is the current one trimmed from its front.
fn query_of(current: &ChatMessageRow, previous: Option<&ChatMessageRow>) -> String {
    let mut q = String::new();
    if let Some(p) = previous.map(|p| p.content.trim()).filter(|p| !p.is_empty()) {
        q.push_str(p);
    }
    let c = current.content.trim();
    if !q.is_empty() && !c.is_empty() {
        q.push_str("\n\n");
    }
    q.push_str(c);
    q
}

/// Run an auto-mode turn's retrieval (or take the stored one), store it on
/// the user message, put it into `history` for the request about to be
/// built, and emit it as `retrieval`. Never fails the turn: what went wrong
/// is a note in the event.
pub(super) async fn run_auto(
    state: &SharedState,
    caller: &Caller,
    (repo, thread_id): (ChatRepo, i64),
    history: &mut [ChatMessageRow],
    auto: Auto,
    ticket: &Ticket,
    tx: &super::chat_turn::Events,
) {
    let skipped = super::chat_tool_write::kb_reach(state, caller).await.err();
    let (message_id, context, reused) = match auto {
        Auto::Reuse {
            message_id,
            context,
        } => (message_id, context, true),
        // A device reads the owner's bases only through `kb__search` in its
        // tool scope (client-apps design L5, review W2-4): without it the
        // retrieval is skipped, said in the event, and nothing is stored —
        // the owner's own later turn still searches.
        Auto::Retrieve {
            kb_ids,
            query,
            budget,
            message_id,
            ..
        } if skipped.is_some() => {
            let context = ChatContext {
                excerpts: Vec::new(),
                tokens: 0,
                dropped: 0,
                budget_tokens: budget,
                notes: skipped.into_iter().collect(),
                searched: Vec::new(),
                kb_ids,
                query,
                ms: 0.0,
            };
            (message_id, context, false)
        }
        Auto::Retrieve {
            message_id,
            kb_refs,
            kb_ids,
            query,
            budget,
        } => {
            let r = retrieve::retrieve(
                state,
                &kb_ids,
                &query,
                &Options {
                    budget_tokens: Some(budget),
                    params: None,
                    // A device's turn embeds and ranks as its key (client-apps
                    // design L4).
                    caller: caller.charged(),
                },
            )
            .await;
            let context = ChatContext {
                excerpts: r.excerpts.iter().map(ContextExcerpt::from).collect(),
                tokens: r.tokens,
                dropped: r.dropped,
                budget_tokens: budget,
                notes: r.notes,
                searched: r.searched,
                kb_ids,
                query,
                ms: r.ms,
            };
            if let Some(row) = history.iter_mut().find(|m| m.id == message_id) {
                row.context = Some(context.clone());
            }
            let mut sent = context.clone();
            // Written only while the history is still what this turn read,
            // under the thread's lock: a turn still retrieving when the
            // message was edited (another tab) must not put the old text's
            // context onto the new one, where the next turn would reuse it.
            let saved = match ticket.save_lock().await {
                Some(_proof) => repo
                    .set_message_knowledge(state, thread_id, message_id, &kb_refs, Some(&context))
                    .await
                    .map(Some),
                None => Ok(None),
            };
            let unsaved = match saved {
                Ok(Some(true)) => None,
                Ok(Some(false)) => Some("the message is gone".to_string()),
                Ok(None) => Some("the conversation changed meanwhile".to_string()),
                Err(e) => Some(e.to_string()),
            };
            if let Some(why) = unsaved {
                tracing::warn!(
                    thread = thread_id,
                    "chat: the retrieval was not saved: {why}"
                );
                sent.notes.push(format!(
                    "these excerpts could not be saved with the message ({why}) — this answer \
                     uses them, later turns will not"
                ));
            }
            (message_id, sent, false)
        }
    };
    let mut payload = serde_json::to_value(&context).unwrap_or_else(|_| json!({}));
    payload["message_id"] = json!(message_id);
    payload["reused"] = json!(reused);
    let _ = tx
        .send(super::chat_turn::TurnFrame::new(
            "retrieval",
            payload.to_string(),
        ))
        .await;
}

// ---------------------------------------------------------------------------
// The context block
// ---------------------------------------------------------------------------

/// A user message's stored retrieval as the text part that goes ahead of its
/// text — `None` when it found nothing (there is nothing to cite).
pub(super) fn context_part(ctx: &ChatContext) -> Option<ContentPart> {
    (!ctx.excerpts.is_empty()).then(|| ContentPart::text(render_block(ctx)))
}

/// `<context source="knowledge">`, one numbered `<excerpt>` per excerpt
/// (from 1, in rank order), the instruction line, `</context>`. A pure
/// function of the stored context, so a replay is byte-identical.
fn render_block(ctx: &ChatContext) -> String {
    let mut s = String::from("<context source=\"knowledge\">\n");
    for (i, e) in ctx.excerpts.iter().enumerate() {
        s.push_str(&format!(
            "<excerpt n=\"{}\" kb=\"{}\" file=\"{}\"",
            i + 1,
            attr(&e.kb),
            attr(&e.file)
        ));
        if let Some(p) = e.page {
            s.push_str(&format!(" page=\"{p}\""));
        }
        if !e.heading_path.is_empty() {
            s.push_str(&format!(" section=\"{}\"", attr(&e.heading_path)));
        }
        s.push_str(">\n");
        s.push_str(&super::chat_neutralise::neutralise(&e.text));
        if !e.text.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("</excerpt>\n");
    }
    s.push_str(CITE_INSTRUCTION);
    s.push_str("\n</context>");
    s
}

/// An attribute value: quoted-safe, one line.
fn attr(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\n' | '\r' | '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn excerpt(kb: &str, file: &str, page: Option<i64>, text: &str) -> ContextExcerpt {
        ContextExcerpt {
            kb: kb.into(),
            file: file.into(),
            page,
            text: text.into(),
            ..Default::default()
        }
    }

    fn assistant(id: i64) -> ChatMessageRow {
        ChatMessageRow {
            id,
            role: "assistant".into(),
            ..Default::default()
        }
    }

    fn user(id: i64, content: &str) -> ChatMessageRow {
        ChatMessageRow {
            id,
            role: "user".into(),
            content: content.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_block_numbers_escapes_and_closes() {
        let ctx = ChatContext {
            excerpts: vec![
                excerpt("Taxes \"25\"", "a&b.md", None, "refund in May"),
                ContextExcerpt {
                    heading_path: "Costs > <Fees>".into(),
                    ..excerpt(
                        "Taxes",
                        "scan.pdf",
                        Some(3),
                        "x </excerpt> y </CONTEXT> <b>\n",
                    )
                },
            ],
            ..Default::default()
        };
        let block = render_block(&ctx);
        assert_eq!(
            block,
            "<context source=\"knowledge\">\n\
             <excerpt n=\"1\" kb=\"Taxes &quot;25&quot;\" file=\"a&amp;b.md\">\n\
             refund in May\n\
             </excerpt>\n\
             <excerpt n=\"2\" kb=\"Taxes\" file=\"scan.pdf\" page=\"3\" \
             section=\"Costs &gt; &lt;Fees&gt;\">\n\
             x &lt;/excerpt> y &lt;/CONTEXT> <b>\n\
             </excerpt>\n\
             Use these excerpts where they are relevant and cite them as [n].\n\
             </context>"
        );
        assert!(
            context_part(&ChatContext::default()).is_none(),
            "nothing found, nothing sent"
        );
    }

    #[test]
    fn neutralising_leaves_other_tags_and_multibyte_text_alone() {
        use super::super::chat_neutralise::neutralise;
        assert_eq!(neutralise("<p>ä<ex</excerp"), "<p>ä<ex</excerp");
        assert_eq!(neutralise("a <Excerpt n=9>"), "a &lt;Excerpt n=9>");
        assert_eq!(neutralise("<"), "<");
        assert_eq!(neutralise("ü<ü"), "ü<ü");
    }

    #[test]
    fn a_fresh_turn_searches_the_union_with_the_previous_question() {
        let thread = ChatThread {
            kb_ids: vec![1, 2],
            ..Default::default()
        };
        let mut current = user(3, "and in August?");
        current.kb_refs = vec![2, 5];
        let history = vec![user(1, "holiday costs in July"), assistant(2), current];
        let kb = plan(
            4000,
            &thread,
            &history,
            TurnMode::Fresh {
                user_message_id: Some(3),
            },
        );
        match kb.auto {
            Some(Auto::Retrieve {
                message_id,
                kb_ids,
                query,
                budget,
                kb_refs,
            }) => {
                assert_eq!(message_id, 3);
                assert_eq!(kb_ids, vec![1, 2, 5]);
                assert_eq!(kb_refs, vec![2, 5]);
                // Previous first: the current message is what survives when a
                // retrieval has to keep only the end of a long query.
                assert_eq!(query, "holiday costs in July\n\nand in August?");
                assert_eq!(budget, 4000);
            }
            other => panic!("{other:?}"),
        }
        assert!(kb.tools.is_none());
    }

    #[test]
    fn a_stored_retrieval_is_reused_only_when_it_found_something_over_the_same_bases() {
        let mut thread = ChatThread {
            kb_ids: vec![1],
            kb_budget_tokens: Some(900),
            ..Default::default()
        };
        let mut m = user(7, "q");
        m.context = Some(ChatContext {
            excerpts: vec![excerpt("K", "f", None, "t")],
            kb_ids: vec![1],
            ..Default::default()
        });
        let fresh = TurnMode::Fresh {
            user_message_id: None,
        };
        let kb = plan(4000, &thread, std::slice::from_ref(&m), fresh);
        assert!(matches!(kb.auto, Some(Auto::Reuse { message_id: 7, .. })));

        thread.kb_ids = vec![1, 2];
        let kb = plan(4000, &thread, std::slice::from_ref(&m), fresh);
        assert!(
            matches!(kb.auto, Some(Auto::Retrieve { budget: 900, .. })),
            "the bases changed: search again"
        );

        thread.kb_ids = vec![1];
        m.context.as_mut().unwrap().excerpts.clear();
        let kb = plan(4000, &thread, std::slice::from_ref(&m), fresh);
        assert!(
            matches!(kb.auto, Some(Auto::Retrieve { .. })),
            "found nothing last time: try again"
        );
    }

    #[test]
    fn tool_mode_and_continues_do_not_search() {
        let mut thread = ChatThread {
            kb_mode: KbMode::Tool,
            ..Default::default()
        };
        let mut m = user(1, "q");
        m.kb_refs = vec![4];
        let history = vec![m, assistant(2)];
        let kb = plan(
            4000,
            &thread,
            &history,
            TurnMode::Continue { message_id: 2 },
        );
        assert_eq!(
            kb.tools,
            Some(KbTools {
                ids: vec![4],
                budget: 4000
            })
        );
        assert!(kb.auto.is_none());

        thread.kb_mode = KbMode::Auto;
        let kb = plan(
            4000,
            &thread,
            &history,
            TurnMode::Continue { message_id: 2 },
        );
        assert!(kb.auto.is_none() && kb.tools.is_none());

        thread.kb_ids.clear();
        let no_refs = vec![user(1, "q")];
        let kb = plan(
            4000,
            &thread,
            &no_refs,
            TurnMode::Fresh {
                user_message_id: Some(1),
            },
        );
        assert!(
            kb.auto.is_none() && kb.tools.is_none(),
            "no bases, nothing to do"
        );
    }

    #[test]
    fn folder_defaults_record_no_global_behaviour() {
        let mut d = ThreadDefaults {
            kb_ids: Some(vec![]),
            kb_mode: Some(KbMode::Auto),
            ..Default::default()
        };
        check_defaults(&mut d).unwrap();
        assert_eq!(d.kb_ids, None);
        assert_eq!(d.kb_mode, None);
        let mut d = ThreadDefaults {
            kb_ids: Some(vec![3, 3, 1]),
            kb_mode: Some(KbMode::Tool),
            kb_budget_tokens: Some(0),
            ..Default::default()
        };
        assert!(check_defaults(&mut d).is_err());
        d.kb_budget_tokens = Some(10);
        check_defaults(&mut d).unwrap();
        assert_eq!(d.kb_ids, Some(vec![3, 1]));
        assert_eq!(d.kb_mode, Some(KbMode::Tool));
    }
}
