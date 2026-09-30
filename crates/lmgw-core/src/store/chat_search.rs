//! Chat search (chat-complete design §4): the FTS5 table `chat_fts`
//! (migration 0050, kept in sync by triggers) queried by [`search_chat`].

use sqlx::{Row, SqlitePool};

use super::*;

/// Threads per result page. `total_threads` and `next_offset` in the answer
/// make the paging visible.
pub const SEARCH_PAGE_THREADS: i64 = 50;
/// Best hits shown per thread; `match_count` says how many there are.
pub const SEARCH_HITS_PER_THREAD: i64 = 3;
/// Snippet match markers (private-use characters): the UI HTML-escapes the
/// snippet, then turns these into `<mark>`.
pub const SNIPPET_OPEN: char = '\u{E000}';
pub const SNIPPET_CLOSE: char = '\u{E001}';
/// Fewest characters a query needs (trimmed, counted as chars).
pub const SEARCH_MIN_CHARS: usize = 2;

/// Which archive state a search covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchArchived {
    Active,
    Archived,
    All,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    /// `t` title, `m` message, `a` attachment name.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub snippet: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchThread {
    pub thread_id: i64,
    pub title: String,
    pub folder_id: Option<i64>,
    pub archived: bool,
    pub updated_at: String,
    pub match_count: i64,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchPage {
    pub total_threads: i64,
    pub threads: Vec<SearchThread>,
    pub next_offset: Option<i64>,
}

/// Turn free text into an FTS5 MATCH expression that cannot be a syntax
/// error: whitespace-separated words become quoted prefix terms (`"foo"*`),
/// a `"quoted phrase"` stays a phrase (no prefix), all ANDed. Inner quotes
/// are doubled; tokens with no letter or digit (nothing the tokenizer would
/// keep) are dropped. Control characters (a NUL, a tab, …) separate words
/// like whitespace does: FTS5 reads a NUL as the end of its string, so one
/// inside a quoted term was an unterminated string — a 500 (review R1
/// finding 5). `None` = nothing searchable is left.
pub fn fts_query(q: &str) -> Option<String> {
    let q: String = q
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut terms: Vec<String> = Vec::new();
    let mut rest = q.trim_start();
    while !rest.is_empty() {
        let (text, phrase);
        if let Some(after) = rest.strip_prefix('"') {
            let end = after.find('"').unwrap_or(after.len());
            text = &after[..end];
            phrase = true;
            rest = after.get(end + 1..).unwrap_or("");
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            text = &rest[..end];
            phrase = false;
            rest = &rest[end..];
        }
        rest = rest.trim_start();
        if !text.chars().any(char::is_alphanumeric) {
            continue;
        }
        let quoted = text.replace('"', "\"\"");
        terms.push(if phrase {
            format!("\"{quoted}\"")
        } else {
            format!("\"{quoted}\"*")
        });
    }
    (!terms.is_empty()).then(|| terms.join(" AND "))
}

/// The FTS half of both queries: every matching index row with its rank
/// (materialized: `bm25()` only works in the query that owns the MATCH).
macro_rules! hits {
    () => {
        "SELECT rowid AS rid, thread_id, kind, ref_id, bm25(chat_fts) AS rank \
         FROM chat_fts WHERE chat_fts MATCH ?1"
    };
}

/// Archive state (`?2`: `0`, `1` or `all`) and folder (`?3`, NULL = any).
macro_rules! filter {
    () => {
        "(?2 = 'all' OR (?2 = '1') = (t.archived_at IS NOT NULL)) \
         AND (?3 IS NULL OR t.folder_id = ?3)"
    };
}

/// Search threads by title, message text and sent attachment names, best
/// bm25 rank first. `folder` narrows to one folder. Threads of every kind
/// the Chat list shows are searched (the list applies no kind filter).
///
/// One statement answers the page (review R1 finding 6 — one query per
/// thread repeated the full MATCH fifty times, since `thread_id` is
/// UNINDEXED and filters only after it):
///
/// - `h` is every matching row with its rank, materialized — `bm25()` only
///   works in the query that owns the MATCH;
/// - `t` groups it into the page's threads, with each one's `match_count`
///   and the total number of matching threads (a window over the groups,
///   taken before the page's `LIMIT`);
/// - `top` keeps each page thread's best rows, and only those are looked up
///   again by rowid for their `snippet()` — a seek per shown hit, not a
///   snippet of every match. A message hit carries its role and an
///   attachment hit the message it was sent with, read in the same
///   statement.
///
/// The total rides on the page's rows, so a page past the end (no rows)
/// counts separately.
pub async fn search_chat(
    pool: &SqlitePool,
    query: &str,
    archived: SearchArchived,
    folder: Option<i64>,
    offset: i64,
) -> DbResult<SearchPage> {
    let empty = SearchPage {
        total_threads: 0,
        threads: vec![],
        next_offset: None,
    };
    let Some(expr) = fts_query(query) else {
        return Ok(empty);
    };
    let arch = match archived {
        SearchArchived::Active => "0",
        SearchArchived::Archived => "1",
        SearchArchived::All => "all",
    };
    let offset = offset.max(0);
    let rows = sqlx::query(concat!(
        "WITH h AS MATERIALIZED (",
        hits!(),
        "), pg AS MATERIALIZED ( \
             SELECT t.id, t.title, t.folder_id, t.archived_at IS NOT NULL AS archived, \
                    t.updated_at, COUNT(*) AS n, MIN(h.rank) AS best, \
                    COUNT(*) OVER () AS total \
             FROM h JOIN chat_threads t ON t.id = h.thread_id WHERE ",
        filter!(),
        "    GROUP BY t.id ORDER BY best, t.updated_at DESC, t.id DESC LIMIT ?4 OFFSET ?5 \
         ), top AS MATERIALIZED ( \
             SELECT rid, thread_id, kind, ref_id, rank FROM ( \
                 SELECT h.*, ROW_NUMBER() OVER ( \
                     PARTITION BY h.thread_id ORDER BY h.rank, h.rid) AS rn \
                 FROM h WHERE h.thread_id IN (SELECT id FROM pg) \
             ) WHERE rn <= ?8 \
         ) \
         SELECT pg.id, pg.title, pg.folder_id, pg.archived, pg.updated_at, pg.n, pg.total, \
                top.kind, top.ref_id, \
                snippet(chat_fts, 0, ?6, ?7, '…', 16) AS snip, \
                CASE top.kind WHEN 'm' THEN \
                    (SELECT role FROM chat_messages WHERE id = top.ref_id) END AS role, \
                CASE top.kind WHEN 'a' THEN \
                    (SELECT message_id FROM chat_attachments WHERE id = top.ref_id) END AS att_msg \
         FROM pg JOIN top ON top.thread_id = pg.id \
              CROSS JOIN chat_fts ON chat_fts.rowid = top.rid \
         WHERE chat_fts MATCH ?1 \
         ORDER BY pg.best, pg.updated_at DESC, pg.id DESC, top.rank, top.rid"
    ))
    .bind(&expr)
    .bind(arch)
    .bind(folder)
    .bind(SEARCH_PAGE_THREADS)
    .bind(offset)
    .bind(SNIPPET_OPEN.to_string())
    .bind(SNIPPET_CLOSE.to_string())
    .bind(SEARCH_HITS_PER_THREAD)
    .fetch_all(pool)
    .await?;

    let mut threads: Vec<SearchThread> = Vec::new();
    let mut total = None;
    for r in &rows {
        let thread_id: i64 = r.get("id");
        total.get_or_insert(r.get::<i64, _>("total"));
        if threads.last().is_none_or(|t| t.thread_id != thread_id) {
            threads.push(SearchThread {
                thread_id,
                title: r.get("title"),
                folder_id: r.get("folder_id"),
                archived: r.get::<i64, _>("archived") != 0,
                updated_at: r.get("updated_at"),
                match_count: r.get("n"),
                hits: Vec::new(),
            });
        }
        let kind: String = r.get("kind");
        let ref_id: i64 = r.get("ref_id");
        let (message_id, role) = match kind.as_str() {
            "m" => (Some(ref_id), r.get::<Option<String>, _>("role")),
            // An attachment hit opens the message it was sent with.
            "a" => (r.get::<Option<i64>, _>("att_msg"), None),
            _ => (None, None),
        };
        let hit = SearchHit {
            kind,
            message_id,
            role,
            snippet: r.get("snip"),
        };
        threads.last_mut().expect("just pushed").hits.push(hit);
    }
    let total = match total {
        Some(n) => n,
        // No page: past the end, or nothing matched.
        None => {
            sqlx::query_scalar(concat!(
                "WITH h AS MATERIALIZED (",
                hits!(),
                ") SELECT COUNT(DISTINCT t.id) FROM h JOIN chat_threads t ON t.id = h.thread_id \
                   WHERE ",
                filter!()
            ))
            .bind(&expr)
            .bind(arch)
            .bind(folder)
            .fetch_one(pool)
            .await?
        }
    };
    let end = offset + threads.len() as i64;
    Ok(SearchPage {
        total_threads: total,
        next_offset: (end < total).then_some(end),
        threads,
    })
}

#[cfg(test)]
mod tests {
    use super::fts_query;

    #[test]
    fn words_become_prefix_terms_and_quotes_a_phrase() {
        assert_eq!(
            fts_query("deplo  prod").unwrap(),
            "\"deplo\"* AND \"prod\"*"
        );
        assert_eq!(
            fts_query("\"exact phrase\" x").unwrap(),
            "\"exact phrase\" AND \"x\"*"
        );
    }

    #[test]
    fn hostile_input_is_quoted_or_dropped() {
        assert_eq!(fts_query("NEAR(").unwrap(), "\"NEAR(\"*");
        assert_eq!(fts_query("a\"b").unwrap(), "\"a\"\"b\"*");
        assert_eq!(fts_query("\"open phrase").unwrap(), "\"open phrase\"");
        assert!(fts_query("- * : \"\"").is_none());
    }

    #[test]
    fn control_characters_separate_words() {
        assert_eq!(fts_query("ab\0c").unwrap(), "\"ab\"* AND \"c\"*");
        assert_eq!(fts_query("\"a\u{1}b\"").unwrap(), "\"a b\"");
        assert_eq!(fts_query("x\ty\u{7f}").unwrap(), "\"x\"* AND \"y\"*");
        assert!(fts_query("\0\u{1b}").is_none());
    }
}
