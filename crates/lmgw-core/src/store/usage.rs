//! Usage queries (usage-analytics §5). Everything the Usage page asks comes
//! through here, so a chart cannot invent its own arithmetic.

use sqlx::{Row, SqlitePool};

use super::*;

/// Calendar granularity of a series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Hour,
    Day,
    Week,
    Month,
}

impl Bucket {
    pub fn parse(s: &str) -> Self {
        match s {
            "hour" => Self::Hour,
            "week" => Self::Week,
            "month" => Self::Month,
            _ => Self::Day,
        }
    }

    /// SQL that turns a stored UTC hour key into a bucket label **in the
    /// viewer's local time**. Buckets are stored in UTC (§3.4) and a "day" that
    /// silently means UTC-day to the query and local-day to the reader is wrong
    /// every day for anyone not on UTC.
    fn sql(self, tz_offset_min: i64) -> String {
        let shift = format!("'{tz_offset_min:+} minutes'");
        let fmt = match self {
            Self::Hour => "'%Y-%m-%dT%H'",
            Self::Day => "'%Y-%m-%d'",
            // %W is the Monday-based week *of the year* — not the ISO week, so
            // 2026-01-01 labels as 2026-W00 and 2025-12-29 (ISO 2026-W01) as
            // 2025-W52. It is a stable, correctly ordered label, which is all a
            // bucket key has to be; it is not an ISO week number and must not
            // be presented as one.
            Self::Week => "'%Y-W%W'",
            Self::Month => "'%Y-%m'",
        };
        format!("strftime({fmt}, bucket_utc || ':00:00', {shift})")
    }
}

/// What a series is split by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    None,
    Alias,
    Key,
    Upstream,
    Class,
}

impl GroupBy {
    pub fn parse(s: &str) -> Self {
        match s {
            "alias" => Self::Alias,
            "key" => Self::Key,
            "upstream" => Self::Upstream,
            "class" => Self::Class,
            _ => Self::None,
        }
    }

    fn sql(self) -> &'static str {
        match self {
            Self::None => "''",
            Self::Alias => "alias",
            Self::Key => "CAST(key_id AS TEXT)",
            Self::Upstream => "CAST(upstream_id AS TEXT)",
            Self::Class => "class",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UsageFilter {
    /// Inclusive UTC hour key, `YYYY-MM-DDTHH`.
    pub from: String,
    /// Exclusive.
    pub to: String,
    pub alias: Option<String>,
    pub key_id: Option<i64>,
    pub class: Option<String>,
    pub upstream_id: Option<i64>,
    /// Minutes east of UTC, from the browser. Applied in the query, never after.
    pub tz_offset_min: i64,
}

/// One (bucket, series) cell of a usage series.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageCell {
    pub bucket: String,
    pub series: String,
    pub requests: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub tokens_cached: i64,
    pub tokens_cache_write: i64,
    pub tokens_reasoning: i64,
    pub cost_micro: i64,
    pub cost_unknown_requests: i64,
    pub cost_unknown_tokens: i64,
    pub errors: i64,
    pub refusals: i64,
    pub ttfb_sum: i64,
    pub ttfb_count: i64,
    pub total_sum: i64,
    pub total_count: i64,
    pub decode_tokens: i64,
    pub decode_ms: f64,
    pub prefill_ms: f64,
    pub prompt_n: i64,
    pub cache_n: i64,
    pub draft_n: i64,
    pub draft_accepted: i64,
}

fn cell_from_row(r: &sqlx::sqlite::SqliteRow) -> UsageCell {
    UsageCell {
        bucket: r.get("bucket"),
        series: r.get::<Option<String>, _>("series").unwrap_or_default(),
        requests: r.get("requests"),
        tokens_in: r.get("tokens_in"),
        tokens_out: r.get("tokens_out"),
        tokens_cached: r.get("tokens_cached"),
        tokens_cache_write: r.get("tokens_cache_write"),
        tokens_reasoning: r.get("tokens_reasoning"),
        cost_micro: r.get("cost_micro"),
        cost_unknown_requests: r.get("cost_unknown_requests"),
        cost_unknown_tokens: r.get("cost_unknown_tokens"),
        errors: r.get("errors"),
        refusals: r.get("refusals"),
        ttfb_sum: r.get("ttfb_sum"),
        ttfb_count: r.get("ttfb_count"),
        total_sum: r.get("total_sum"),
        total_count: r.get("total_count"),
        decode_tokens: r.get("decode_tokens"),
        decode_ms: r.get("decode_ms"),
        prefill_ms: r.get("prefill_ms"),
        prompt_n: r.get("prompt_n"),
        cache_n: r.get("cache_n"),
        draft_n: r.get("draft_n"),
        draft_accepted: r.get("draft_accepted"),
    }
}

fn push_filters(qb: &mut sqlx::QueryBuilder<sqlx::Sqlite>, f: &UsageFilter) {
    qb.push(" WHERE bucket_utc >= ").push_bind(f.from.clone());
    qb.push(" AND bucket_utc < ").push_bind(f.to.clone());
    if let Some(a) = &f.alias {
        qb.push(" AND alias = ").push_bind(a.clone());
    }
    if let Some(k) = f.key_id {
        qb.push(" AND key_id = ").push_bind(k);
    }
    if let Some(c) = &f.class {
        qb.push(" AND class = ").push_bind(c.clone());
    }
    if let Some(u) = f.upstream_id {
        qb.push(" AND upstream_id = ").push_bind(u);
    }
}

/// The aggregate every chart reads.
pub async fn usage_series(
    pool: &SqlitePool,
    f: &UsageFilter,
    bucket: Bucket,
    group_by: GroupBy,
) -> DbResult<Vec<UsageCell>> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT ");
    qb.push(bucket.sql(f.tz_offset_min))
        .push(" AS bucket, ")
        .push(group_by.sql())
        .push(
            " AS series,
             SUM(requests) AS requests,
             SUM(tokens_in) AS tokens_in, SUM(tokens_out) AS tokens_out,
             SUM(tokens_cached) AS tokens_cached,
             SUM(tokens_cache_write) AS tokens_cache_write,
             SUM(tokens_reasoning) AS tokens_reasoning,
             SUM(cost_micro) AS cost_micro,
             SUM(cost_unknown_requests) AS cost_unknown_requests,
             SUM(cost_unknown_tokens) AS cost_unknown_tokens,
             SUM(CASE WHEN outcome IN ('client_error','upstream_error') THEN requests ELSE 0 END)
                 AS errors,
             SUM(CASE WHEN outcome = 'refused' THEN requests ELSE 0 END) AS refusals,
             SUM(ttfb_sum) AS ttfb_sum, SUM(ttfb_count) AS ttfb_count,
             SUM(total_sum) AS total_sum, SUM(total_count) AS total_count,
             SUM(decode_tokens) AS decode_tokens, SUM(decode_ms) AS decode_ms,
             SUM(prefill_ms) AS prefill_ms,
             SUM(prompt_n) AS prompt_n, SUM(cache_n) AS cache_n,
             SUM(draft_n) AS draft_n, SUM(draft_accepted) AS draft_accepted
             FROM usage_hourly",
        );
    push_filters(&mut qb, f);
    qb.push(" GROUP BY bucket, series ORDER BY bucket ASC");
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows.iter().map(cell_from_row).collect())
}

/// Totals over the whole window — the tiles, and the budget check.
pub async fn usage_totals(pool: &SqlitePool, f: &UsageFilter) -> DbResult<UsageCell> {
    let rows = usage_series(pool, f, Bucket::Month, GroupBy::None).await?;
    let mut t = UsageCell::default();
    for r in rows {
        t.requests += r.requests;
        t.tokens_in += r.tokens_in;
        t.tokens_out += r.tokens_out;
        t.tokens_cached += r.tokens_cached;
        t.tokens_cache_write += r.tokens_cache_write;
        t.tokens_reasoning += r.tokens_reasoning;
        t.cost_micro += r.cost_micro;
        t.cost_unknown_requests += r.cost_unknown_requests;
        t.cost_unknown_tokens += r.cost_unknown_tokens;
        t.errors += r.errors;
        t.refusals += r.refusals;
        t.ttfb_sum += r.ttfb_sum;
        t.ttfb_count += r.ttfb_count;
        t.total_sum += r.total_sum;
        t.total_count += r.total_count;
        t.decode_tokens += r.decode_tokens;
        t.decode_ms += r.decode_ms;
        t.prefill_ms += r.prefill_ms;
        t.prompt_n += r.prompt_n;
        t.cache_n += r.cache_n;
        t.draft_n += r.draft_n;
        t.draft_accepted += r.draft_accepted;
    }
    Ok(t)
}

/// Ranked totals for one dimension — the "where it went" bars.
pub async fn usage_top(
    pool: &SqlitePool,
    f: &UsageFilter,
    dim: GroupBy,
) -> DbResult<Vec<UsageCell>> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT '' AS bucket, ");
    qb.push(dim.sql()).push(
        " AS series,
         SUM(requests) AS requests,
         SUM(tokens_in) AS tokens_in, SUM(tokens_out) AS tokens_out,
         SUM(tokens_cached) AS tokens_cached,
         SUM(tokens_cache_write) AS tokens_cache_write,
         SUM(tokens_reasoning) AS tokens_reasoning,
         SUM(cost_micro) AS cost_micro,
         SUM(cost_unknown_requests) AS cost_unknown_requests,
         SUM(cost_unknown_tokens) AS cost_unknown_tokens,
         SUM(CASE WHEN outcome IN ('client_error','upstream_error') THEN requests ELSE 0 END)
             AS errors,
         SUM(CASE WHEN outcome = 'refused' THEN requests ELSE 0 END) AS refusals,
         SUM(ttfb_sum) AS ttfb_sum, SUM(ttfb_count) AS ttfb_count,
         SUM(total_sum) AS total_sum, SUM(total_count) AS total_count,
         SUM(decode_tokens) AS decode_tokens, SUM(decode_ms) AS decode_ms,
         SUM(prefill_ms) AS prefill_ms,
         SUM(prompt_n) AS prompt_n, SUM(cache_n) AS cache_n,
         SUM(draft_n) AS draft_n, SUM(draft_accepted) AS draft_accepted
         FROM usage_hourly",
    );
    push_filters(&mut qb, f);
    qb.push(" GROUP BY series ORDER BY cost_micro DESC, tokens_in + tokens_out DESC");
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows.iter().map(cell_from_row).collect())
}

/// Weekday (0 = Sunday) × hour request counts, in local time.
pub async fn usage_heat(pool: &SqlitePool, f: &UsageFilter) -> DbResult<Vec<(i64, i64, i64)>> {
    let shift = format!("'{:+} minutes'", f.tz_offset_min);
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
        "SELECT CAST(strftime('%w', bucket_utc || ':00:00', {shift}) AS INTEGER) AS dow,
                CAST(strftime('%H', bucket_utc || ':00:00', {shift}) AS INTEGER) AS hour,
                SUM(requests) AS requests
         FROM usage_hourly"
    ));
    push_filters(&mut qb, f);
    qb.push(" GROUP BY dow, hour");
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<i64, _>("dow"),
                r.get::<i64, _>("hour"),
                r.get::<i64, _>("requests"),
            )
        })
        .collect())
}

/// Percentiles of one latency metric, read off the sparse histogram.
///
/// Returns milliseconds at the geometric centre of the bucket each percentile
/// falls in (±9%, see [`latency_bucket_ms`]). `None` when the window holds no
/// successful request — an empty window has no p95, and reporting 0 would draw
/// a dashboard that says the gateway got infinitely fast.
pub async fn usage_percentiles(
    pool: &SqlitePool,
    f: &UsageFilter,
    metric: &str,
    ps: &[f64],
) -> DbResult<Vec<Option<f64>>> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT bucket_idx, SUM(count) AS n FROM usage_latency_hourly
         WHERE bucket_utc >= ",
    );
    qb.push_bind(f.from.clone());
    qb.push(" AND bucket_utc < ").push_bind(f.to.clone());
    qb.push(" AND metric = ").push_bind(metric.to_string());
    if let Some(a) = &f.alias {
        qb.push(" AND alias = ").push_bind(a.clone());
    }
    if let Some(k) = f.key_id {
        qb.push(" AND key_id = ").push_bind(k);
    }
    if let Some(c) = &f.class {
        qb.push(" AND class = ").push_bind(c.clone());
    }
    if let Some(u) = f.upstream_id {
        qb.push(" AND upstream_id = ").push_bind(u);
    }
    qb.push(" GROUP BY bucket_idx ORDER BY bucket_idx ASC");
    let rows = qb.build().fetch_all(pool).await?;

    let hist: Vec<(i64, i64)> = rows
        .iter()
        .map(|r| (r.get::<i64, _>("bucket_idx"), r.get::<i64, _>("n")))
        .collect();
    Ok(percentiles_of(&hist, ps))
}

/// Rebuild `usage_hourly` + `usage_latency_hourly` from whatever raw rows still
/// exist, discarding what is there.
///
/// Two jobs. On upgrade it is the **backfill**: an install with a month of
/// request logs and no rollups would otherwise open the Usage page on an empty
/// chart and read it as "the gateway did nothing in September". Afterwards it
/// is the repair — the rollup and the rows it came from are two
/// representations of one truth, and this is what proves they still agree.
///
/// It is not a substitute for the same-transaction upsert on the write path:
/// rows the retention pruner has already eaten cannot be rebuilt from anything,
/// which is exactly why the rollup is written as the row is.
pub async fn rebuild_usage(pool: &SqlitePool) -> DbResult<u64> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM usage_hourly")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM usage_latency_hourly")
        .execute(&mut *tx)
        .await?;

    // The refusal list comes from the one place that defines it, rather than a
    // second copy in SQL that would drift the first time a kind is added.
    let refusals = crate::telemetry::REFUSAL_KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    let outcome = format!(
        "CASE WHEN error_kind IN ({refusals}) THEN 'refused'
              WHEN status BETWEEN 200 AND 299 THEN 'ok'
              WHEN status BETWEEN 400 AND 499 THEN 'client_error'
              ELSE 'upstream_error' END"
    );

    let sql = format!(
        "INSERT INTO usage_hourly (bucket_utc, key_id, alias, upstream_id, class, outcome,
            requests, tokens_in, tokens_out, tokens_cached, tokens_cache_write, tokens_reasoning,
            cost_micro, cost_unknown_requests, cost_unknown_tokens,
            ttfb_sum, ttfb_count, total_sum, total_count, total_min, total_max,
            decode_tokens, decode_ms, prefill_ms, prompt_n, cache_n, draft_n, draft_accepted)
         SELECT strftime('%Y-%m-%dT%H', ts),
                COALESCE(key_id, 0),
                requested_alias,
                COALESCE(upstream_id, 0),
                COALESCE(class, 'chat'),
                {outcome},
                COUNT(*),
                COALESCE(SUM(prompt_tokens), 0),
                COALESCE(SUM(completion_tokens), 0),
                COALESCE(SUM(cached_in_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0),
                COALESCE(SUM(reasoning_tokens), 0),
                COALESCE(SUM(cost_micro), 0),
                -- 'unknown' is not 'zero', and a tool row is not an unpriced
                -- model call — both rules restated here so a rebuild produces
                -- exactly what the write path would have.
                SUM(CASE WHEN cost_micro IS NULL AND COALESCE(class,'chat') <> 'tool'
                              AND (({outcome}) = 'ok'
                                   OR COALESCE(prompt_tokens,0) + COALESCE(completion_tokens,0) > 0)
                         THEN 1 ELSE 0 END),
                SUM(CASE WHEN cost_micro IS NULL AND COALESCE(class,'chat') <> 'tool'
                              AND (({outcome}) = 'ok'
                                   OR COALESCE(prompt_tokens,0) + COALESCE(completion_tokens,0) > 0)
                         THEN COALESCE(prompt_tokens,0) + COALESCE(completion_tokens,0)
                         ELSE 0 END),
                COALESCE(SUM(ttfb_ms), 0),
                SUM(CASE WHEN ttfb_ms IS NOT NULL THEN 1 ELSE 0 END),
                COALESCE(SUM(total_ms), 0),
                SUM(CASE WHEN total_ms IS NOT NULL THEN 1 ELSE 0 END),
                MIN(total_ms), MAX(total_ms),
                COALESCE(SUM(predicted_n), 0),
                COALESCE(SUM(decode_ms), 0),
                COALESCE(SUM(prefill_ms), 0),
                COALESCE(SUM(prompt_n), 0),
                COALESCE(SUM(cache_n), 0),
                COALESCE(SUM(draft_n), 0),
                COALESCE(SUM(draft_accepted), 0)
         FROM request_logs
         GROUP BY 1, 2, 3, 4, 5, 6"
    );
    // Through a QueryBuilder because the statement is assembled (the refusal
    // list above), and sqlx rightly refuses a formatted string to `query`.
    let res = sqlx::QueryBuilder::<sqlx::Sqlite>::new(sql)
        .build()
        .execute(&mut *tx)
        .await?;
    let rolled = res.rows_affected();

    // The latency histogram is folded in Rust: its bucket index is
    // `round(4·log2(ms))`, and SQLite's log2() is a build-time option that is
    // not worth depending on for a one-off rebuild.
    let rows = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
        "SELECT strftime('%Y-%m-%dT%H', ts) AS bucket, COALESCE(key_id,0) AS key_id,
                requested_alias, COALESCE(upstream_id,0) AS upstream_id,
                COALESCE(class,'chat') AS class, ttfb_ms, total_ms
         FROM request_logs
         WHERE ({outcome}) = 'ok'"
    ))
    .build()
    .fetch_all(&mut *tx)
    .await?;

    #[allow(clippy::type_complexity)]
    let mut hist: std::collections::HashMap<
        (String, i64, String, i64, String, &str, i64),
        i64,
    > = std::collections::HashMap::new();
    for r in &rows {
        let bucket: String = r.get("bucket");
        let key_id: i64 = r.get("key_id");
        let alias: String = r.get("requested_alias");
        let upstream_id: i64 = r.get("upstream_id");
        let class: String = r.get("class");
        for (metric, ms) in [
            ("ttfb", r.get::<Option<i64>, _>("ttfb_ms")),
            ("total", r.get::<Option<i64>, _>("total_ms")),
        ] {
            let Some(ms) = ms else { continue };
            *hist
                .entry((
                    bucket.clone(),
                    key_id,
                    alias.clone(),
                    upstream_id,
                    class.clone(),
                    metric,
                    latency_bucket_idx(ms),
                ))
                .or_insert(0) += 1;
        }
    }
    for ((bucket, key_id, alias, upstream_id, class, metric, idx), n) in hist {
        sqlx::query(
            "INSERT INTO usage_latency_hourly
                (bucket_utc, key_id, alias, upstream_id, class, metric, bucket_idx, count)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(bucket_utc, key_id, alias, upstream_id, class, metric, bucket_idx)
             DO UPDATE SET count = count + excluded.count",
        )
        .bind(bucket)
        .bind(key_id)
        .bind(alias)
        .bind(upstream_id)
        .bind(class)
        .bind(metric)
        .bind(idx)
        .bind(n)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(rolled)
}

/// Backfill on first start after the upgrade: rebuild only when there are raw
/// rows and no rollups at all. Any other state is a running install, and
/// rebuilding one of those would throw away history the pruner has already
/// taken the raw rows for.
pub async fn backfill_usage_if_empty(pool: &SqlitePool) -> DbResult<u64> {
    let rollups: i64 = sqlx::query("SELECT COUNT(*) AS n FROM usage_hourly")
        .fetch_one(pool)
        .await?
        .get("n");
    if rollups > 0 {
        return Ok(0);
    }
    let raw: i64 = sqlx::query("SELECT COUNT(*) AS n FROM request_logs")
        .fetch_one(pool)
        .await?
        .get("n");
    if raw == 0 {
        return Ok(0);
    }
    let n = rebuild_usage(pool).await?;
    tracing::info!("backfilled {n} usage rollup rows from {raw} request logs");
    Ok(n)
}

/// Percentiles **per display bucket**, so a latency chart can plot a real p50
/// and p95 line instead of a mean.
///
/// A mean is what you get if you divide the sums the rollup also keeps, and it
/// is exactly the number §3.1 warns about: one 30-second outlier drags it four
/// times above the typical request. Each bucket's distribution is read from the
/// sparse histogram and the percentile taken within it — never averaged across
/// buckets, which is the other half of the same mistake.
pub async fn usage_percentiles_by_bucket(
    pool: &SqlitePool,
    f: &UsageFilter,
    bucket: Bucket,
    metric: &str,
    ps: &[f64],
) -> DbResult<Vec<(String, Vec<Option<f64>>)>> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT ");
    qb.push(bucket.sql(f.tz_offset_min)).push(
        " AS bucket, bucket_idx, SUM(count) AS n FROM usage_latency_hourly
         WHERE bucket_utc >= ",
    );
    qb.push_bind(f.from.clone());
    qb.push(" AND bucket_utc < ").push_bind(f.to.clone());
    qb.push(" AND metric = ").push_bind(metric.to_string());
    if let Some(a) = &f.alias {
        qb.push(" AND alias = ").push_bind(a.clone());
    }
    if let Some(k) = f.key_id {
        qb.push(" AND key_id = ").push_bind(k);
    }
    if let Some(c) = &f.class {
        qb.push(" AND class = ").push_bind(c.clone());
    }
    if let Some(u) = f.upstream_id {
        qb.push(" AND upstream_id = ").push_bind(u);
    }
    qb.push(" GROUP BY bucket, bucket_idx ORDER BY bucket ASC, bucket_idx ASC");
    let rows = qb.build().fetch_all(pool).await?;

    let mut per_bucket: Vec<(String, Vec<(i64, i64)>)> = Vec::new();
    for r in &rows {
        let b: String = r.get("bucket");
        let pair = (r.get::<i64, _>("bucket_idx"), r.get::<i64, _>("n"));
        match per_bucket.last_mut() {
            Some((name, hist)) if *name == b => hist.push(pair),
            _ => per_bucket.push((b, vec![pair])),
        }
    }
    Ok(per_bucket
        .into_iter()
        .map(|(b, hist)| (b, percentiles_of(&hist, ps)))
        .collect())
}

/// Pick percentiles out of one `(bucket_idx, count)` histogram.
fn percentiles_of(hist: &[(i64, i64)], ps: &[f64]) -> Vec<Option<f64>> {
    let total: i64 = hist.iter().map(|(_, n)| n).sum();
    if total == 0 {
        return ps.iter().map(|_| None).collect();
    }
    ps.iter()
        .map(|p| {
            let want = (total as f64 * p).ceil().max(1.0) as i64;
            let mut seen = 0;
            for (idx, n) in hist {
                seen += n;
                if seen >= want {
                    return Some(latency_bucket_ms(*idx));
                }
            }
            hist.last().map(|(idx, _)| latency_bucket_ms(*idx))
        })
        .collect()
}

/// Errors and refusals broken down by **kind**, from the raw rows.
///
/// The rollup keeps a four-way `outcome` and not the kind, on purpose: the
/// cardinality of its primary key is what keeps it small enough to never need
/// its own retention. But "which refusal am I actually hitting" is the question
/// an errors panel exists to answer, and it is a *recent operations* question —
/// so it is asked of the raw rows, which cover exactly the retention window,
/// and the panel says that is its range.
pub async fn usage_error_kinds(
    pool: &SqlitePool,
    f: &UsageFilter,
    bucket: Bucket,
) -> DbResult<Vec<(String, String, i64)>> {
    let shift = format!("'{:+} minutes'", f.tz_offset_min);
    // The same label the rollup charts use, so the errors card lines up with
    // its neighbours instead of drawing two day-wide columns beside 24 hourly
    // ones.
    let fmt = match bucket {
        Bucket::Hour => "'%Y-%m-%dT%H'",
        Bucket::Day => "'%Y-%m-%d'",
        Bucket::Week => "'%Y-W%W'",
        Bucket::Month => "'%Y-%m'",
    };
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
        "SELECT strftime({fmt}, ts, {shift}) AS bucket,
                COALESCE(error_kind, 'error') AS kind,
                COUNT(*) AS n
         FROM request_logs
         WHERE error_kind IS NOT NULL
           AND strftime('%Y-%m-%dT%H', ts) >= "
    ));
    qb.push_bind(f.from.clone());
    qb.push(" AND strftime('%Y-%m-%dT%H', ts) < ")
        .push_bind(f.to.clone());
    if let Some(a) = &f.alias {
        qb.push(" AND requested_alias = ").push_bind(a.clone());
    }
    if let Some(k) = f.key_id {
        qb.push(" AND key_id = ").push_bind(k);
    }
    if let Some(c) = &f.class {
        qb.push(" AND class = ").push_bind(c.clone());
    }
    qb.push(" GROUP BY bucket, kind ORDER BY bucket ASC");
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("bucket"),
                r.get::<String, _>("kind"),
                r.get::<i64, _>("n"),
            )
        })
        .collect())
}

/// Every model name that appears in the usage rollup, with the requests it has
/// on record, busiest first.
///
/// The rollup is the **only** place a passthrough model's name exists — an
/// `expose_all` upstream serves hundreds of models that were never configured
/// rows — so "which models have I actually used" cannot be answered from the
/// config alone. Names that no longer resolve (an MCP tool call, a model that
/// has since gone away) are the caller's to filter: this is the log, not a
/// catalog.
pub async fn used_model_names(pool: &SqlitePool) -> DbResult<Vec<(String, i64)>> {
    let rows = sqlx::query(
        "SELECT alias, COALESCE(SUM(requests), 0) AS requests
           FROM usage_hourly
          GROUP BY alias
          ORDER BY requests DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<String, _>("alias"), r.get::<i64, _>("requests")))
        .collect())
}

/// Spend so far in a key's current budget period, in micro-units.
///
/// Reads the rollup rather than the raw rows, so a budget still works after
/// retention has pruned the month's early requests.
pub async fn key_spend_micro(
    pool: &SqlitePool,
    key_id: i64,
    period_start_utc: &str,
) -> DbResult<i64> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(cost_micro), 0) AS spent FROM usage_hourly
         WHERE key_id = ?1 AND bucket_utc >= ?2",
    )
    .bind(key_id)
    .bind(period_start_utc)
    .fetch_one(pool)
    .await?;
    Ok(row.get("spent"))
}

/// Total spend across every key in a period — the owner's global budget.
pub async fn total_spend_micro(pool: &SqlitePool, period_start_utc: &str) -> DbResult<i64> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(cost_micro), 0) AS spent FROM usage_hourly WHERE bucket_utc >= ?1",
    )
    .bind(period_start_utc)
    .fetch_one(pool)
    .await?;
    Ok(row.get("spent"))
}

/// Prune the rollups themselves. Separate setting, separate call, default off —
/// a few hundred rows a day is a rounding error next to one day of raw logs,
/// and dropping them is dropping the only copy of that history.
pub async fn prune_usage(pool: &SqlitePool, months: i64) -> DbResult<u64> {
    if months <= 0 {
        return Ok(0);
    }
    let cutoff = format!("-{months} months");
    let mut removed = 0u64;
    // Two literal statements rather than a formatted table name: sqlx refuses a
    // dynamic SQL string, and rightly.
    for q in [
        "DELETE FROM usage_hourly WHERE bucket_utc < strftime('%Y-%m-%dT%H','now',?1)",
        "DELETE FROM usage_latency_hourly WHERE bucket_utc < strftime('%Y-%m-%dT%H','now',?1)",
    ] {
        let res = sqlx::query(q).bind(cutoff.clone()).execute(pool).await?;
        removed += res.rows_affected();
    }
    Ok(removed)
}
