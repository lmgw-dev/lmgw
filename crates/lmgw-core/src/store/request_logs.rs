//! Request logs

use sqlx::{Row, SqlitePool};

use crate::ir::Timings;
use crate::pricing::Cost;
use crate::telemetry::{outcome_of, RequestClass};

use super::*;

#[derive(Debug, Clone, serde::Serialize)]
pub struct RequestLogRow {
    pub id: i64,
    pub ts: String,
    pub client_key: Option<String>,
    pub ingress_proto: String,
    pub requested_alias: String,
    pub upstream_name: Option<String>,
    pub upstream_model: Option<String>,
    /// MCP tool name for `ingress_proto = 'mcp'` rows (§10); NULL otherwise.
    pub mcp_tool: Option<String>,
    pub egress_proto: Option<String>,
    pub status: i64,
    pub ttfb_ms: Option<i64>,
    pub total_ms: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub streamed: bool,
    pub error_kind: Option<String>,
    pub error_msg: Option<String>,
    // --- usage analytics (§2.1) ---
    pub key_id: Option<i64>,
    pub class: Option<String>,
    pub cost_micro: Option<i64>,
    pub cost_in_micro: Option<i64>,
    pub cost_out_micro: Option<i64>,
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    pub price_source: Option<String>,
    pub cached_in_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub prefill_ms: Option<f64>,
    pub decode_ms: Option<f64>,
    pub decode_tok_s: Option<f64>,
    pub prompt_n: Option<i64>,
    pub cache_n: Option<i64>,
    pub draft_n: Option<i64>,
    pub draft_accepted: Option<i64>,
    /// `max_tokens` lmgw lowered a client-set value to, on a ladder rung or a
    /// guarded unified-KV pool (ladder design §3.2, unified-KV design §3.3
    /// step 1). `NULL` = not clamped — migration 0041.
    pub max_tokens_clamped: Option<i64>,
    /// Why a fallback answered instead of the requested local model: `hold`,
    /// `external_vram`, `background` or `unavailable` (candidate-aliases
    /// design §4.7, §12 entry 70).
    /// `NULL` = no fallback — migration 0042.
    pub fallback_reason: Option<String>,
    /// The rung a ladder row answered from, 1-based like every other rung
    /// surface (ladder design §6, §12 entry 11). `NULL` for a row without a
    /// ladder — migration 0043.
    pub rung: Option<i64>,
}

fn log_from_row(row: &sqlx::sqlite::SqliteRow) -> RequestLogRow {
    RequestLogRow {
        id: row.get("id"),
        ts: row.get("ts"),
        client_key: row.get("client_key"),
        ingress_proto: row.get("ingress_proto"),
        requested_alias: row.get("requested_alias"),
        upstream_name: row.get("upstream_name"),
        upstream_model: row.get("upstream_model"),
        mcp_tool: row.get("mcp_tool"),
        egress_proto: row.get("egress_proto"),
        status: row.get("status"),
        ttfb_ms: row.get("ttfb_ms"),
        total_ms: row.get("total_ms"),
        prompt_tokens: row.get("prompt_tokens"),
        completion_tokens: row.get("completion_tokens"),
        streamed: row.get::<i64, _>("streamed") != 0,
        error_kind: row.get("error_kind"),
        error_msg: row.get("error_msg"),
        key_id: row.get("key_id"),
        class: row.get("class"),
        cost_micro: row.get("cost_micro"),
        cost_in_micro: row.get("cost_in_micro"),
        cost_out_micro: row.get("cost_out_micro"),
        price_in: row.get("price_in"),
        price_out: row.get("price_out"),
        price_source: row.get("price_source"),
        cached_in_tokens: row.get("cached_in_tokens"),
        cache_write_tokens: row.get("cache_write_tokens"),
        reasoning_tokens: row.get("reasoning_tokens"),
        prefill_ms: row.get("prefill_ms"),
        decode_ms: row.get("decode_ms"),
        decode_tok_s: row.get("decode_tok_s"),
        prompt_n: row.get("prompt_n"),
        cache_n: row.get("cache_n"),
        draft_n: row.get("draft_n"),
        draft_accepted: row.get("draft_accepted"),
        max_tokens_clamped: row.get("max_tokens_clamped"),
        fallback_reason: row.get("fallback_reason"),
        rung: row.get("rung"),
    }
}

pub struct NewRequestLog {
    pub client_key: Option<String>,
    pub ingress_proto: String,
    pub requested_alias: String,
    pub upstream_id: Option<i64>,
    pub upstream_name: Option<String>,
    pub upstream_model: Option<String>,
    /// MCP tool name for `ingress_proto = 'mcp'` rows (§10); NULL otherwise.
    pub mcp_tool: Option<String>,
    pub egress_proto: Option<String>,
    pub status: i64,
    pub ttfb_ms: Option<i64>,
    pub total_ms: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub streamed: bool,
    pub error_kind: Option<String>,
    pub error_msg: Option<String>,
    /// `api_keys.id` of whoever made the call — the identity, next to
    /// `client_key`'s name, so a renamed or deleted key does not orphan its
    /// history. `None` when auth is off and no internal identity applies.
    pub key_id: Option<i64>,
    pub class: RequestClass,
    /// Input-token detail beyond the plain total (cache subsets, reasoning).
    pub cached_in_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    /// What it cost and under which numbers. [`Cost::unknown`] for anything
    /// that could not be priced — which writes NULLs, never zeros.
    pub cost: Cost,
    /// llama.cpp's own measurements, for a local request.
    pub timings: Option<Timings>,
    /// `max_tokens` lmgw lowered a client-set value to (ladder design §3.2,
    /// unified-KV design §3.3 step 1). `None` = not clamped; only the gate's
    /// per-send lease ([`crate::gate::TurnLease::max_tokens_clamped`]) ever
    /// sets it.
    pub max_tokens_clamped: Option<u32>,
    /// Why a fallback answered instead of the requested local model
    /// ([`crate::gate::FallbackReason::as_str`]); `None` = no fallback. The
    /// request's [`crate::gate::GateHeaders::fallback_reason`], the same fact
    /// its `x-lmgw-fallback-reason` header states.
    pub fallback_reason: Option<String>,
    /// The rung a ladder row answered from, 1-based (ladder design §6) — or,
    /// for a refusal, the rung it was judged on. `None` for a row without a
    /// ladder, and for a request its fallback answered.
    pub rung: Option<i64>,
}

impl Default for NewRequestLog {
    fn default() -> Self {
        Self {
            client_key: None,
            ingress_proto: String::new(),
            requested_alias: String::new(),
            upstream_id: None,
            upstream_name: None,
            upstream_model: None,
            mcp_tool: None,
            egress_proto: None,
            status: 0,
            ttfb_ms: None,
            total_ms: None,
            prompt_tokens: None,
            completion_tokens: None,
            streamed: false,
            error_kind: None,
            error_msg: None,
            key_id: None,
            class: RequestClass::Chat,
            cached_in_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            cost: Cost::unknown(),
            timings: None,
            max_tokens_clamped: None,
            fallback_reason: None,
            rung: None,
        }
    }
}

/// Latency histogram bucket for `ms`: **4 buckets per octave**, so a percentile
/// read back out is accurate to about ±9%. A doubling ladder would be ±100%,
/// which is not a latency measurement, it is a rumour.
///
/// Index 0 holds everything under 1 ms; the top is clamped so a pathological
/// hour cannot widen the table.
pub fn latency_bucket_idx(ms: i64) -> i64 {
    if ms <= 1 {
        return 0;
    }
    ((ms as f64).log2() * 4.0).round().clamp(0.0, 127.0) as i64
}

/// Milliseconds at the geometric centre of a bucket — what a percentile query
/// reports. The centre rather than an edge: an edge is systematically wrong in
/// one direction, and with 4 buckets per octave the centre is within ±9%.
///
/// The index is produced by *rounding* `4·log2(ms)`, so bucket `i` covers
/// `[2^((i-0.5)/4), 2^((i+0.5)/4))` and its centre is exactly `2^(i/4)` — no
/// half-step offset, which would bias every reading upward by 9%.
pub fn latency_bucket_ms(idx: i64) -> f64 {
    if idx <= 0 {
        return 1.0;
    }
    2f64.powf(idx as f64 / 4.0)
}

pub async fn insert_request_log(pool: &SqlitePool, l: &NewRequestLog) -> DbResult<i64> {
    // The row and its rollup are one transaction (usage-analytics §3.2). Not a
    // periodic batch over recent rows: a batch's window and the retention
    // pruner's window are two clocks that eventually disagree, and the failure
    // mode is silent under-counting of exactly the busiest hour.
    let mut tx = pool.begin().await?;

    // **One clock read for the row and its bucket.** Both used to call
    // `datetime('now')` / `strftime(... 'now')` in separate statements, and
    // SQLite re-reads the clock per statement — so a request committing at
    // HH:59:59.99 could be logged in hour H and rolled into hour H+1, which a
    // later `rebuild_usage` (which reads `ts`) would then silently move back.
    let now = chrono::Utc::now();
    let ts = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let bucket = now.format("%Y-%m-%dT%H").to_string();

    let res = sqlx::query(
        "INSERT INTO request_logs (ts, client_key, ingress_proto, requested_alias, upstream_id,
            upstream_name, upstream_model, mcp_tool, egress_proto, status, ttfb_ms, total_ms,
            prompt_tokens, completion_tokens, streamed, error_kind, error_msg, max_tokens_clamped,
            key_id, class, cost_micro, cost_in_micro, cost_out_micro,
            price_in, price_out, price_cache_read, price_cache_write, price_source,
            cached_in_tokens, cache_write_tokens, reasoning_tokens,
            prefill_ms, decode_ms, decode_tok_s, prompt_n, cache_n, draft_n, draft_accepted,
            predicted_n, fallback_reason, rung)
         VALUES (?39,?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,
                 ?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,?33,?34,?35,?36,?37,
                 ?38,?40,?41)",
    )
    .bind(&l.client_key)
    .bind(&l.ingress_proto)
    .bind(&l.requested_alias)
    .bind(l.upstream_id)
    .bind(&l.upstream_name)
    .bind(&l.upstream_model)
    .bind(&l.mcp_tool)
    .bind(&l.egress_proto)
    .bind(l.status)
    .bind(l.ttfb_ms)
    .bind(l.total_ms)
    .bind(l.prompt_tokens)
    .bind(l.completion_tokens)
    .bind(l.streamed as i64)
    .bind(&l.error_kind)
    .bind(&l.error_msg)
    .bind(l.max_tokens_clamped.map(|v| v as i64))
    .bind(l.key_id)
    .bind(l.class.as_str())
    .bind(l.cost.total_micro)
    .bind(l.cost.in_micro)
    .bind(l.cost.out_micro)
    .bind(l.cost.used.price_in)
    .bind(l.cost.used.price_out)
    .bind(l.cost.used.price_cache_read)
    .bind(l.cost.used.price_cache_write)
    .bind(l.cost.source.as_str())
    .bind(l.cached_in_tokens)
    .bind(l.cache_write_tokens)
    .bind(l.reasoning_tokens)
    .bind(l.timings.map(|t| t.prompt_ms))
    .bind(l.timings.map(|t| t.predicted_ms))
    .bind(l.timings.map(|t| t.predicted_per_second))
    .bind(l.timings.map(|t| t.prompt_n as i64))
    .bind(l.timings.and_then(|t| t.cache_n).map(|v| v as i64))
    .bind(l.timings.and_then(|t| t.draft_n).map(|v| v as i64))
    .bind(l.timings.and_then(|t| t.draft_n_accepted).map(|v| v as i64))
    // The count itself, not just the rate it was achieved at: reconstructing it
    // as rate x duration truncates a short turn to zero tokens.
    .bind(l.timings.map(|t| t.predicted_n as i64))
    .bind(&ts)
    .bind(&l.fallback_reason)
    .bind(l.rung)
    .execute(&mut *tx)
    .await?;
    let id = res.last_insert_rowid();

    roll_up(&mut tx, l, &bucket).await?;
    tx.commit().await?;
    Ok(id)
}

/// Fold one logged request into its hourly bucket.
///
/// Every scalar is a `col = col + excluded.col` upsert done by SQLite, so there
/// is no read-modify-write and two concurrent requests in the same bucket
/// cannot lose each other's counts. `bucket` is derived from the *same* clock
/// read that stamped the row's `ts`, so a rebuild — which reads `ts` — lands
/// every row in the hour it was already counted in.
async fn roll_up(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    l: &NewRequestLog,
    bucket: &str,
) -> DbResult<()> {
    let outcome = outcome_of(l.status, l.error_kind.as_deref());
    let key_id = l.key_id.unwrap_or(0);
    let upstream_id = l.upstream_id.unwrap_or(0);
    let class = l.class.as_str();
    // "Unknown" is not "zero" (§2.3): an unpriced request contributes nothing
    // to cost_micro and instead lands in the remainder that every total is
    // obliged to state beside itself.
    // What the unpriced remainder means (§2.3) is "this total is incomplete" —
    // so it counts work that happened and could not be priced, and nothing
    // else. A tool row is not a model call at all; a refusal or an error that
    // spent no tokens cost nothing to be missing. Counting those made an agent
    // retry-looping into 500 `gpu_hold` refusals read as a 500-request hole in
    // the price sheet, which is the opposite of what the line is telling you.
    let spent_tokens = l.prompt_tokens.unwrap_or(0) + l.completion_tokens.unwrap_or(0);
    let unknown = l.cost.total_micro.is_none()
        && l.class != RequestClass::Tool
        && (outcome == "ok" || spent_tokens > 0);
    let tokens_in = l.prompt_tokens.unwrap_or(0);
    let tokens_out = l.completion_tokens.unwrap_or(0);

    sqlx::query(
        "INSERT INTO usage_hourly (bucket_utc, key_id, alias, upstream_id, class, outcome,
            requests, tokens_in, tokens_out, tokens_cached, tokens_cache_write, tokens_reasoning,
            cost_micro, cost_unknown_requests, cost_unknown_tokens,
            ttfb_sum, ttfb_count, total_sum, total_count, total_min, total_max,
            decode_tokens, decode_ms, prefill_ms, prompt_n, cache_n, draft_n, draft_accepted)
         VALUES (?26, ?1, ?2, ?3, ?4, ?5,
            1, ?6, ?7, ?8, ?25, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?17,
            ?18, ?19, ?20, ?21, ?22, ?23, ?24)
         ON CONFLICT(bucket_utc, key_id, alias, upstream_id, class, outcome) DO UPDATE SET
            requests              = requests + 1,
            tokens_in             = tokens_in + excluded.tokens_in,
            tokens_out            = tokens_out + excluded.tokens_out,
            tokens_cached         = tokens_cached + excluded.tokens_cached,
            tokens_cache_write    = tokens_cache_write + excluded.tokens_cache_write,
            tokens_reasoning      = tokens_reasoning + excluded.tokens_reasoning,
            cost_micro            = cost_micro + excluded.cost_micro,
            cost_unknown_requests = cost_unknown_requests + excluded.cost_unknown_requests,
            cost_unknown_tokens   = cost_unknown_tokens + excluded.cost_unknown_tokens,
            ttfb_sum              = ttfb_sum + excluded.ttfb_sum,
            ttfb_count            = ttfb_count + excluded.ttfb_count,
            total_sum             = total_sum + excluded.total_sum,
            total_count           = total_count + excluded.total_count,
            -- MIN(a, NULL) is NULL, so coalesce each side to the other first.
            total_min             = MIN(COALESCE(total_min, excluded.total_min),
                                        COALESCE(excluded.total_min, total_min)),
            total_max             = MAX(COALESCE(total_max, excluded.total_max),
                                        COALESCE(excluded.total_max, total_max)),
            decode_tokens         = decode_tokens + excluded.decode_tokens,
            decode_ms             = decode_ms + excluded.decode_ms,
            prefill_ms            = prefill_ms + excluded.prefill_ms,
            prompt_n              = prompt_n + excluded.prompt_n,
            cache_n               = cache_n + excluded.cache_n,
            draft_n               = draft_n + excluded.draft_n,
            draft_accepted        = draft_accepted + excluded.draft_accepted",
    )
    .bind(key_id)
    .bind(&l.requested_alias)
    .bind(upstream_id)
    .bind(class)
    .bind(outcome)
    .bind(tokens_in)
    .bind(tokens_out)
    .bind(l.cached_in_tokens.unwrap_or(0))
    .bind(l.reasoning_tokens.unwrap_or(0))
    .bind(l.cost.total_micro.unwrap_or(0))
    .bind(i64::from(unknown))
    .bind(if unknown { tokens_in + tokens_out } else { 0 })
    .bind(l.ttfb_ms.unwrap_or(0))
    .bind(i64::from(l.ttfb_ms.is_some()))
    .bind(l.total_ms.unwrap_or(0))
    .bind(i64::from(l.total_ms.is_some()))
    .bind(l.total_ms)
    .bind(l.timings.map(|t| t.predicted_n as i64).unwrap_or(0))
    .bind(l.timings.map(|t| t.predicted_ms).unwrap_or(0.0))
    .bind(l.timings.map(|t| t.prompt_ms).unwrap_or(0.0))
    .bind(l.timings.map(|t| t.prompt_n as i64).unwrap_or(0))
    .bind(l.timings.and_then(|t| t.cache_n).unwrap_or(0) as i64)
    .bind(l.timings.and_then(|t| t.draft_n).unwrap_or(0) as i64)
    .bind(l.timings.and_then(|t| t.draft_n_accepted).unwrap_or(0) as i64)
    .bind(l.cache_write_tokens.unwrap_or(0))
    .bind(bucket)
    .execute(&mut **tx)
    .await?;

    // Latency distribution: successful requests only. A refusal's
    // time-to-first-byte is not a latency signal, and letting refusals in makes
    // a p50 improve precisely when the gateway starts failing fast.
    if outcome == "ok" {
        for (metric, ms) in [("ttfb", l.ttfb_ms), ("total", l.total_ms)] {
            let Some(ms) = ms else { continue };
            sqlx::query(
                "INSERT INTO usage_latency_hourly
                    (bucket_utc, key_id, alias, upstream_id, class, metric, bucket_idx, count)
                 VALUES (?7, ?1, ?2, ?3, ?4, ?5, ?6, 1)
                 ON CONFLICT(bucket_utc, key_id, alias, upstream_id, class, metric, bucket_idx)
                 DO UPDATE SET count = count + 1",
            )
            .bind(key_id)
            .bind(&l.requested_alias)
            .bind(upstream_id)
            .bind(class)
            .bind(metric)
            .bind(latency_bucket_idx(ms))
            .bind(bucket)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    pub alias: Option<String>,
    pub upstream_name: Option<String>,
    pub errors_only: bool,
    pub limit: i64,
    pub before_id: Option<i64>,
    /// The identity, not the name — a key filter has to survive a rename.
    pub key_id: Option<i64>,
    /// One refusal or error kind, which is how the Usage page's errors chart
    /// links a segment to the rows behind it.
    pub error_kind: Option<String>,
    pub class: Option<String>,
    /// A substring of the requested alias, case-insensitive — what a filter
    /// box types. `alias` stays the exact match a link from Usage carries:
    /// "gemma4-12b" must not also bring back "gemma4-12b-reason".
    pub alias_q: Option<String>,
    /// A substring of the upstream name, the same way.
    pub upstream_q: Option<String>,
}

/// `%text%` for a `LIKE … ESCAPE '\'`, with the pattern characters in
/// `text` taken literally: a model id is full of `_`, and an unescaped one
/// matches any character.
fn like_contains(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('%');
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

pub async fn query_logs(pool: &SqlitePool, f: &LogFilter) -> DbResult<Vec<RequestLogRow>> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT * FROM request_logs WHERE 1=1");
    if let Some(alias) = &f.alias {
        qb.push(" AND requested_alias = ").push_bind(alias.clone());
    }
    if let Some(up) = &f.upstream_name {
        qb.push(" AND upstream_name = ").push_bind(up.clone());
    }
    if f.errors_only {
        qb.push(" AND status >= 400");
    }
    if let Some(k) = f.key_id {
        qb.push(" AND key_id = ").push_bind(k);
    }
    if let Some(k) = &f.error_kind {
        qb.push(" AND error_kind = ").push_bind(k.clone());
    }
    if let Some(c) = &f.class {
        qb.push(" AND class = ").push_bind(c.clone());
    }
    if let Some(q) = &f.alias_q {
        qb.push(" AND requested_alias LIKE ")
            .push_bind(like_contains(q))
            .push(" ESCAPE '\\'");
    }
    if let Some(q) = &f.upstream_q {
        qb.push(" AND upstream_name LIKE ")
            .push_bind(like_contains(q))
            .push(" ESCAPE '\\'");
    }
    if let Some(before) = f.before_id {
        qb.push(" AND id < ").push_bind(before);
    }
    qb.push(" ORDER BY id DESC LIMIT ")
        .push_bind(if f.limit > 0 { f.limit } else { 100 });
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows.iter().map(log_from_row).collect())
}

pub async fn get_log(pool: &SqlitePool, id: i64) -> DbResult<Option<RequestLogRow>> {
    let row = sqlx::query("SELECT * FROM request_logs WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(|r| log_from_row(r)))
}

/// Prune by age and by max row count (§10).
///
/// **Raw rows only.** `usage_hourly` is the history and deliberately outlives
/// them (usage-analytics §3.3): the raw row is the detail you can still open,
/// the rollup is the month you can still see. Pruning here while the rollup
/// stays is exactly the intended asymmetry, not an oversight.
pub async fn prune_logs(pool: &SqlitePool, days: i64, max_rows: i64) -> DbResult<u64> {
    let mut removed = 0u64;
    if days > 0 {
        let res = sqlx::query("DELETE FROM request_logs WHERE ts < datetime('now', ?1)")
            .bind(format!("-{days} days"))
            .execute(pool)
            .await?;
        removed += res.rows_affected();
    }
    if max_rows > 0 {
        let res = sqlx::query(
            "DELETE FROM request_logs WHERE id <= (
                SELECT id FROM request_logs ORDER BY id DESC LIMIT 1 OFFSET ?1
            )",
        )
        .bind(max_rows)
        .execute(pool)
        .await?;
        removed += res.rows_affected();
    }
    Ok(removed)
}
