//! `/api/usage/*` — usage analytics, cost & policy (design
//! `docs/design/2026-09-18-usage-analytics-cost-policy-design.md`).
//!
//! One query surface backs every chart on the Usage page (§5), so a chart
//! cannot invent its own arithmetic: every handler here is a thin translation
//! of query params into a `store::UsageFilter` plus a call into the already
//! tested aggregation layer (`store::usage_series` and friends). Mounted like
//! the rest of `/api` — no auth middleware, no self-admin gate; trust comes
//! from `bind_addr`.
//!
//! Two conventions from `api.rs` carry over unchanged: the `_inner` halves
//! return a typed DTO (or `Err(String)`) and the thin `pub async fn` wrappers
//! translate that into a `Response` via [`ops_result`], so every failure on
//! this plane has the one `ApiError` shape. `export_csv` is the one exception
//! — its success body is bytes, not JSON.

use std::collections::{BTreeMap, HashSet};

use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use chrono::{Duration, NaiveDateTime, Utc};
use lmgw_api_types as dto;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Row, SqlitePool};

use crate::config::{self, Snapshot, UpstreamKind};
use crate::pricing::{self, TokenUsage};
use crate::state::SharedState;
use crate::store::{self, Bucket, GroupBy, UsageCell, UsageFilter};

use super::api::ops_result;

/// The sentinel series key/label for the server-side folded tail (design §5,
/// "never a ninth generated hue"). Never a colour slot, never counted toward
/// `limit`.
const OTHER_KEY: &str = "other";

/// `Result<T, String>` -> `Result<Value, String>`, so every handler here can
/// share `api::ops_result`'s one `ApiError` shape without hand-building JSON
/// (the DTOs are the contract; this just carries them across that boundary).
fn ok_value<T: Serialize>(v: T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SeriesQuery {
    from: Option<String>,
    to: Option<String>,
    bucket: Option<String>,
    group_by: Option<String>,
    class: Option<String>,
    alias: Option<String>,
    key_id: Option<i64>,
    upstream_id: Option<i64>,
    tz: Option<i64>,
    /// Series past this rank (by cost, then tokens) fold into `Other`.
    /// Defaults to 6, the number of chart colour slots.
    limit: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TopQuery {
    dim: Option<String>,
    from: Option<String>,
    to: Option<String>,
    class: Option<String>,
    alias: Option<String>,
    key_id: Option<i64>,
    upstream_id: Option<i64>,
    tz: Option<i64>,
    /// Truncate the ranked rows to the first N. Absent = every row for `dim`.
    limit: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HeatQuery {
    from: Option<String>,
    to: Option<String>,
    class: Option<String>,
    alias: Option<String>,
    key_id: Option<i64>,
    upstream_id: Option<i64>,
    tz: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ErrorsQuery {
    from: Option<String>,
    to: Option<String>,
    bucket: Option<String>,
    class: Option<String>,
    alias: Option<String>,
    key_id: Option<i64>,
    tz: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct LocalQuery {
    from: Option<String>,
    to: Option<String>,
    class: Option<String>,
    alias: Option<String>,
    key_id: Option<i64>,
    tz: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExportQuery {
    from: Option<String>,
    to: Option<String>,
    class: Option<String>,
    alias: Option<String>,
    key_id: Option<i64>,
    upstream_id: Option<i64>,
    tz: Option<i64>,
}

/// The filter fields every query above shares, before `from`/`to` defaulting
/// and validation.
struct RangeInput {
    from: Option<String>,
    to: Option<String>,
    alias: Option<String>,
    key_id: Option<i64>,
    class: Option<String>,
    upstream_id: Option<i64>,
    tz: Option<i64>,
}

/// `from`/`to` default to the last 30 days, as UTC hour keys. `to` is one hour
/// past `now` so the current, still-filling hour bucket is included — the
/// filter's own upper bound is exclusive (design §3.4).
fn default_window() -> (String, String) {
    let now = Utc::now();
    let to = (now + Duration::hours(1)).format("%Y-%m-%dT%H").to_string();
    let from = (now - Duration::days(30)).format("%Y-%m-%dT%H").to_string();
    (from, to)
}

/// Parse a `YYYY-MM-DDTHH` UTC hour key. `NaiveDateTime` rather than a bare
/// regex check, so a nonsense calendar date (`2026-13-40T99`) is rejected too.
fn parse_hour_key(v: &str) -> Result<NaiveDateTime, String> {
    NaiveDateTime::parse_from_str(&format!("{v}:00:00"), "%Y-%m-%dT%H:%M:%S")
        .map_err(|_| format!("'{v}' is not a valid UTC hour key (YYYY-MM-DDTHH)"))
}

/// The longest window a single query may span, in hours — about three years.
///
/// `bucket_labels` walks the window an hour at a time through a recursive CTE,
/// so `?from=0001-01-01T00` is not a slow query, it is 17 billion rows holding
/// a pool connection until the process dies. The bound is a real maximum and it
/// says so when it refuses, rather than silently returning a truncated range.
const MAX_WINDOW_HOURS: i64 = 24 * 365 * 3;

fn parse_filter(r: RangeInput) -> Result<UsageFilter, String> {
    let (def_from, def_to) = default_window();
    let from_in = r.from.filter(|s| !s.is_empty()).unwrap_or(def_from);
    let to_in = r.to.filter(|s| !s.is_empty()).unwrap_or(def_to);
    let from_dt = parse_hour_key(&from_in)?;
    let to_dt = parse_hour_key(&to_in)?;

    // **Canonicalise.** chrono accepts `2026-9-1T00` and `26-09-18T05`; SQLite
    // does not, and these strings are used raw in both `bucket_utc >= ?` string
    // comparisons and `datetime(? || ':00:00')`. A non-canonical key therefore
    // matched nothing (byte-wise `'2026-09-15T10' < '2026-9-1T00'`) and made
    // strftime return NULL, which the row mappers then unwrapped into a panic.
    // Re-formatting what chrono parsed removes the whole class.
    let from = from_dt.format("%Y-%m-%dT%H").to_string();
    let to = to_dt.format("%Y-%m-%dT%H").to_string();

    if from >= to {
        return Err(format!("'from' ({from}) must be before 'to' ({to})"));
    }
    let hours = (to_dt - from_dt).num_hours();
    if hours > MAX_WINDOW_HOURS {
        return Err(format!(
            "window is {hours} hours; the maximum is {MAX_WINDOW_HOURS} \
             (about three years) — narrow 'from'/'to'"
        ));
    }
    Ok(UsageFilter {
        from,
        to,
        alias: r.alias.filter(|s| !s.is_empty()),
        key_id: r.key_id,
        class: r.class.filter(|s| !s.is_empty()),
        upstream_id: r.upstream_id,
        // A UTC offset outside ±24h is not a time zone. Unclamped, SQLite's
        // `'+9223372036854775807 minutes'` modifier returns NULL and every
        // bucket mapper panics on the missing column.
        tz_offset_min: r.tz.unwrap_or(0).clamp(-1440, 1440),
    })
}

// ---------------------------------------------------------------------------
// Series identity — labels, colour slots, the local flag (design §6.3)
// ---------------------------------------------------------------------------

fn is_local_kind(kind: UpstreamKind) -> bool {
    matches!(
        kind,
        UpstreamKind::LlamaServer | UpstreamKind::AudioCpp | UpstreamKind::SdCpp
    )
}

/// Name and local-ness of one `upstream_id` as logged on a usage row —
/// including the four synthetic per-class ids (chat/aux local traffic logs
/// [`config::ROUTER_UPSTREAM_ID`] / [`config::AUX_UPSTREAM_ID`], audio logs
/// [`config::AUDIO_UPSTREAM_ID`], image logs [`config::IMAGE_UPSTREAM_ID`])
/// which never appear in `snap.upstreams` because they are never persisted
/// (config/snapshot.rs, `Snapshot::synthetic_upstream`).
fn upstream_label_and_kind(snap: &Snapshot, id: i64) -> (String, bool) {
    match id {
        0 => ("(none)".to_string(), false),
        config::ROUTER_UPSTREAM_ID => (config::ROUTER_UPSTREAM_NAME.to_string(), true),
        config::AUX_UPSTREAM_ID => (config::AUX_UPSTREAM_NAME.to_string(), true),
        config::AUDIO_UPSTREAM_ID => (config::AUDIO_UPSTREAM_NAME.to_string(), true),
        config::IMAGE_UPSTREAM_ID => (config::IMAGE_UPSTREAM_NAME.to_string(), true),
        id => snap
            .upstreams
            .get(&id)
            .map(|u| (u.name.clone(), is_local_kind(u.kind)))
            .unwrap_or_else(|| (format!("upstream {id}"), false)),
    }
}

fn is_local_upstream_id(snap: &Snapshot, id: i64) -> bool {
    upstream_label_and_kind(snap, id).1
}

fn key_label(snap: &Snapshot, id: i64) -> String {
    if id == 0 {
        return "(no key)".to_string();
    }
    snap.api_keys
        .iter()
        .find(|k| k.id == id)
        .map(|k| k.name.clone())
        .unwrap_or_else(|| format!("key {id}"))
}

/// Chart colour slot 0..5 for an alias, from its position in the gateway's
/// own sorted list of served model names (`Snapshot::exposed_models`, sorted
/// by name) — **not** from its rank in any one response, so a filter that
/// drops a series never repaints the survivors (design §6.3).
fn alias_slot(snap: &Snapshot, name: &str) -> Option<u8> {
    snap.exposed_models()
        .iter()
        .position(|m| m.name.eq_ignore_ascii_case(name))
        .map(|i| (i % 6) as u8)
}

/// `(label, colour slot, is-local)` for one series key, per `group_by`
/// (design §6.3 / `SeriesMeta` doc comments in `lmgw-api-types`).
///
/// Every dimension gets a slot, each from **its own** stable sorted list — the
/// alias list for aliases, the key list for keys, and so on. The rule that
/// matters is not "the six colours mean models", it is that a colour follows
/// the entity rather than its rank, so filtering a series out never repaints
/// the survivors. Leaving a whole dimension at `None` would paint a
/// keys-grouped chart entirely in the neutral "Other" grey, which is not a
/// chart.
fn series_identity(snap: &Snapshot, group_by: GroupBy, series: &str) -> (String, Option<u8>, bool) {
    match group_by {
        GroupBy::None => (String::new(), None, false),
        GroupBy::Alias => {
            let slot = alias_slot(snap, series);
            // A candidate alias's candidates are all local models, though its
            // fallback may answer some of its traffic (candidate-aliases §4.1).
            let local = snap.candidate_alias(series).is_some()
                || snap
                    .resolve(series)
                    .is_ok_and(|r| is_local_kind(r.upstream.kind));
            (series.to_string(), slot, local)
        }
        GroupBy::Key => {
            let id: i64 = series.parse().unwrap_or(0);
            let mut names: Vec<&str> = snap.api_keys.iter().map(|k| k.name.as_str()).collect();
            names.sort_unstable();
            let label = key_label(snap, id);
            let slot = names
                .iter()
                .position(|n| *n == label)
                .map(|i| (i % 6) as u8);
            (label, slot, false)
        }
        GroupBy::Upstream => {
            let id: i64 = series.parse().unwrap_or(0);
            let (label, local) = upstream_label_and_kind(snap, id);
            let mut names: Vec<&str> = snap.upstreams.values().map(|u| u.name.as_str()).collect();
            names.sort_unstable();
            let slot = names
                .iter()
                .position(|n| *n == label)
                .map(|i| (i % 6) as u8);
            (label, slot, local)
        }
        // Fixed, because the request classes are a closed set: chat is always
        // slot 0 wherever it is drawn.
        GroupBy::Class => {
            let slot = ["chat", "aux", "audio", "image", "tool"]
                .iter()
                .position(|c| *c == series)
                .map(|i| i as u8);
            (series.to_string(), slot, false)
        }
    }
}

fn series_meta(snap: &Snapshot, group_by: GroupBy, key: &str) -> dto::SeriesMeta {
    let (label, slot, local) = series_identity(snap, group_by, key);
    dto::SeriesMeta {
        key: key.to_string(),
        label,
        slot,
        local,
        folded: None,
    }
}

// ---------------------------------------------------------------------------
// store::UsageCell -> dto::UsageCell
// ---------------------------------------------------------------------------

fn cell_to_dto(c: &UsageCell) -> dto::UsageCell {
    dto::UsageCell {
        bucket: c.bucket.clone(),
        series: c.series.clone(),
        requests: c.requests,
        tokens_in: c.tokens_in,
        tokens_out: c.tokens_out,
        tokens_cached: c.tokens_cached,
        tokens_cache_write: c.tokens_cache_write,
        tokens_reasoning: c.tokens_reasoning,
        cost_micro: c.cost_micro,
        cost_unknown_requests: c.cost_unknown_requests,
        cost_unknown_tokens: c.cost_unknown_tokens,
        errors: c.errors,
        refusals: c.refusals,
        ttfb_sum: c.ttfb_sum,
        ttfb_count: c.ttfb_count,
        total_sum: c.total_sum,
        total_count: c.total_count,
        decode_tokens: c.decode_tokens,
        decode_ms: c.decode_ms,
        prefill_ms: c.prefill_ms,
        prompt_n: c.prompt_n,
        cache_n: c.cache_n,
        draft_n: c.draft_n,
        draft_accepted: c.draft_accepted,
        audio_in_ms: c.audio_in_ms,
        chars_in: c.chars_in,
        images_out: c.images_out,
        cost_unknown_audio_in_ms: c.cost_unknown_audio_in_ms,
        cost_unknown_chars_in: c.cost_unknown_chars_in,
        cost_unknown_images_out: c.cost_unknown_images_out,
    }
}

fn merge_into(acc: &mut UsageCell, c: &UsageCell) {
    acc.requests += c.requests;
    acc.tokens_in += c.tokens_in;
    acc.tokens_out += c.tokens_out;
    acc.tokens_cached += c.tokens_cached;
    acc.tokens_cache_write += c.tokens_cache_write;
    acc.tokens_reasoning += c.tokens_reasoning;
    acc.cost_micro += c.cost_micro;
    acc.cost_unknown_requests += c.cost_unknown_requests;
    acc.cost_unknown_tokens += c.cost_unknown_tokens;
    acc.errors += c.errors;
    acc.refusals += c.refusals;
    acc.ttfb_sum += c.ttfb_sum;
    acc.ttfb_count += c.ttfb_count;
    acc.total_sum += c.total_sum;
    acc.total_count += c.total_count;
    acc.decode_tokens += c.decode_tokens;
    acc.decode_ms += c.decode_ms;
    acc.prefill_ms += c.prefill_ms;
    acc.prompt_n += c.prompt_n;
    acc.cache_n += c.cache_n;
    acc.draft_n += c.draft_n;
    acc.draft_accepted += c.draft_accepted;
    acc.audio_in_ms += c.audio_in_ms;
    acc.chars_in += c.chars_in;
    acc.images_out += c.images_out;
    acc.cost_unknown_audio_in_ms += c.cost_unknown_audio_in_ms;
    acc.cost_unknown_chars_in += c.cost_unknown_chars_in;
    acc.cost_unknown_images_out += c.cost_unknown_images_out;
}

/// Fold every series past the first `limit` of `ranked` (already ordered
/// best-first by cost then tokens, per `store::usage_top`) into one `Other`
/// cell per bucket. Pure and DB-free, so the fold itself needs no seeded
/// database to test (design §5: "never a ninth generated hue").
///
/// Returns the (possibly folded) cells and whether any folding happened —
/// callers use the latter to decide whether an `Other` `SeriesMeta` exists at
/// all, since a window with `<= limit` series folds nothing.
/// The folded tail's series: the neutral "Other", carrying how many
/// entities went into it — the fold is a cap on colours, and a cap is said.
fn other_meta(ranked: usize, limit: usize) -> dto::SeriesMeta {
    dto::SeriesMeta {
        key: OTHER_KEY.to_string(),
        label: "Other".to_string(),
        slot: None,
        local: false,
        folded: Some(ranked.saturating_sub(limit).min(u32::MAX as usize) as u32),
    }
}

fn fold_tail_to_other(
    cells: Vec<UsageCell>,
    ranked: &[String],
    limit: usize,
) -> (Vec<UsageCell>, bool) {
    if ranked.len() <= limit {
        return (cells, false);
    }
    let kept: HashSet<&str> = ranked.iter().take(limit).map(String::as_str).collect();
    let mut kept_cells = Vec::with_capacity(cells.len());
    let mut other: BTreeMap<String, UsageCell> = BTreeMap::new();
    for c in cells {
        if kept.contains(c.series.as_str()) {
            kept_cells.push(c);
        } else {
            let bucket = c.bucket.clone();
            let entry = other.entry(bucket.clone()).or_insert_with(|| UsageCell {
                bucket,
                series: OTHER_KEY.to_string(),
                ..Default::default()
            });
            merge_into(entry, &c);
        }
    }
    kept_cells.extend(other.into_values());
    (kept_cells, true)
}

// ---------------------------------------------------------------------------
// Bucket labels — every bucket in the window, including empty ones
// ---------------------------------------------------------------------------

/// Every bucket label in `[from, to)`, in chronological order, generated from
/// the range itself rather than from whatever `usage_hourly` happens to hold
/// — a chart that silently drops a quiet day draws a lie about its own x-axis
/// (design §5 / `UsageSeriesResponse::buckets` doc comment).
///
/// Uses the same `strftime(fmt, ..., shift)` shape `store::Bucket::sql` uses
/// for the data query itself (that method is private to `store::usage`, so
/// this mirrors its two format tables rather than reusing it), fed by a recursive
/// CTE that walks the window one UTC hour at a time — run through SQLite
/// itself so the bucket labels here are guaranteed to agree with the ones the
/// data query produces, rather than a second, hand-rolled local-time
/// formatter that could drift from it.
async fn bucket_labels(
    pool: &SqlitePool,
    from: &str,
    to: &str,
    bucket: Bucket,
    tz_offset_min: i64,
) -> Result<Vec<String>, String> {
    if from >= to {
        return Ok(Vec::new());
    }
    let fmt = match bucket {
        Bucket::Hour => "%Y-%m-%dT%H",
        Bucket::Day => "%Y-%m-%d",
        Bucket::Week => "%Y-W%W",
        Bucket::Month => "%Y-%m",
    };
    let shift = format!("{tz_offset_min:+} minutes");
    let rows = sqlx::query(
        "WITH RECURSIVE hrs(h) AS (
            SELECT datetime(?1 || ':00:00')
            UNION ALL
            SELECT datetime(h, '+1 hour') FROM hrs
            WHERE datetime(h, '+1 hour') < datetime(?2 || ':00:00')
        )
        SELECT strftime(?3, h, ?4) AS bucket, MIN(h) AS first_h
        FROM hrs GROUP BY bucket ORDER BY first_h",
    )
    .bind(from)
    .bind(to)
    .bind(fmt)
    .bind(shift)
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| r.get::<String, _>("bucket"))
        .collect())
}

// ---------------------------------------------------------------------------
// Previous window (tiles' delta, design §5)
// ---------------------------------------------------------------------------

/// Totals over the window of the same length immediately before `f.from`, or
/// `None` when that window is entirely before the first rollup row exists —
/// there is nothing to compare a fresh install's first day against.
async fn previous_window_totals(
    pool: &SqlitePool,
    f: &UsageFilter,
) -> Result<Option<UsageCell>, String> {
    let from_dt = parse_hour_key(&f.from)?;
    let to_dt = parse_hour_key(&f.to)?;
    let span = to_dt - from_dt;
    let prev_to = f.from.clone();
    let prev_from = (from_dt - span).format("%Y-%m-%dT%H").to_string();

    let row = sqlx::query("SELECT MIN(bucket_utc) AS earliest FROM usage_hourly")
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;
    let earliest: Option<String> = row.get("earliest");
    let Some(earliest) = earliest else {
        return Ok(None);
    };
    if prev_to <= earliest {
        return Ok(None);
    }

    let prev_filter = UsageFilter {
        from: prev_from,
        to: prev_to,
        ..f.clone()
    };
    store::usage_totals(pool, &prev_filter)
        .await
        .map(Some)
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// GET /api/usage/series
// ---------------------------------------------------------------------------

async fn series_inner(
    st: &SharedState,
    q: SeriesQuery,
) -> Result<dto::UsageSeriesResponse, String> {
    let filter = parse_filter(RangeInput {
        from: q.from,
        to: q.to,
        alias: q.alias,
        key_id: q.key_id,
        class: q.class,
        upstream_id: q.upstream_id,
        tz: q.tz,
    })?;
    let bucket = Bucket::parse(q.bucket.as_deref().unwrap_or("day"));
    let group_by = GroupBy::parse(q.group_by.as_deref().unwrap_or("none"));
    // Six is the number of chart colour slots (design §6.3) — a caller that
    // wants every series unfolded passes an explicit, larger `limit`.
    let limit = q.limit.unwrap_or(6).max(0) as usize;
    let snap = st.snapshot();

    let cells = store::usage_series(&st.db, &filter, bucket, group_by)
        .await
        .map_err(|e| e.to_string())?;
    let ranked = store::usage_top(&st.db, &filter, group_by)
        .await
        .map_err(|e| e.to_string())?;
    let ranked_keys: Vec<String> = ranked.into_iter().map(|c| c.series).collect();

    let buckets = bucket_labels(
        &st.db,
        &filter.from,
        &filter.to,
        bucket,
        filter.tz_offset_min,
    )
    .await?;
    let (folded_cells, folded) = fold_tail_to_other(cells, &ranked_keys, limit);

    let mut series: Vec<dto::SeriesMeta> = ranked_keys
        .iter()
        .take(limit)
        .map(|key| series_meta(&snap, group_by, key))
        .collect();
    if folded {
        series.push(other_meta(ranked_keys.len(), limit));
    }

    let totals = store::usage_totals(&st.db, &filter)
        .await
        .map_err(|e| e.to_string())?;
    let previous = previous_window_totals(&st.db, &filter).await?;
    let units_since = store::units_since(&st.db)
        .await
        .map_err(|e| e.to_string())?;

    let p_total = store::usage_percentiles(&st.db, &filter, "total", &[0.5, 0.95])
        .await
        .map_err(|e| e.to_string())?;
    let p_ttfb = store::usage_percentiles(&st.db, &filter, "ttfb", &[0.5, 0.95])
        .await
        .map_err(|e| e.to_string())?;

    // Per-bucket percentiles, aligned with `buckets` so the chart can plot a
    // real p50/p95 line. Buckets with no successful request keep `None` rather
    // than a zero, which would draw a dip that never happened.
    let by_bucket_total =
        store::usage_percentiles_by_bucket(&st.db, &filter, bucket, "total", &[0.5, 0.95])
            .await
            .map_err(|e| e.to_string())?;
    let by_bucket_ttfb =
        store::usage_percentiles_by_bucket(&st.db, &filter, bucket, "ttfb", &[0.5, 0.95])
            .await
            .map_err(|e| e.to_string())?;
    let total_at: std::collections::HashMap<&str, &Vec<Option<f64>>> = by_bucket_total
        .iter()
        .map(|(b, v)| (b.as_str(), v))
        .collect();
    let ttfb_at: std::collections::HashMap<&str, &Vec<Option<f64>>> = by_bucket_ttfb
        .iter()
        .map(|(b, v)| (b.as_str(), v))
        .collect();
    let latency: Vec<dto::LatencyPoint> = buckets
        .iter()
        .map(|b| {
            let t = total_at.get(b.as_str());
            let f = ttfb_at.get(b.as_str());
            dto::LatencyPoint {
                bucket: b.clone(),
                p50_total_ms: t.and_then(|v| v.first().copied().flatten()),
                p95_total_ms: t.and_then(|v| v.get(1).copied().flatten()),
                p50_ttfb_ms: f.and_then(|v| v.first().copied().flatten()),
                p95_ttfb_ms: f.and_then(|v| v.get(1).copied().flatten()),
            }
        })
        .collect();

    Ok(dto::UsageSeriesResponse {
        buckets,
        series,
        cells: folded_cells.iter().map(cell_to_dto).collect(),
        totals: cell_to_dto(&totals),
        previous: previous.as_ref().map(cell_to_dto),
        currency: snap.settings.currency.clone(),
        p50_total_ms: p_total.first().copied().flatten(),
        p95_total_ms: p_total.get(1).copied().flatten(),
        p50_ttfb_ms: p_ttfb.first().copied().flatten(),
        p95_ttfb_ms: p_ttfb.get(1).copied().flatten(),
        latency,
        units_since,
    })
}

pub async fn series(State(st): State<SharedState>, Query(q): Query<SeriesQuery>) -> Response {
    ops_result(series_inner(&st, q).await.and_then(ok_value))
}

// ---------------------------------------------------------------------------
// GET /api/usage/top
// ---------------------------------------------------------------------------

async fn top_inner(st: &SharedState, q: TopQuery) -> Result<dto::UsageTopResponse, String> {
    let filter = parse_filter(RangeInput {
        from: q.from,
        to: q.to,
        alias: q.alias,
        key_id: q.key_id,
        class: q.class,
        upstream_id: q.upstream_id,
        tz: q.tz,
    })?;
    let dim = GroupBy::parse(q.dim.as_deref().unwrap_or("alias"));
    let snap = st.snapshot();

    let mut rows = store::usage_top(&st.db, &filter, dim)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(limit) = q.limit {
        rows.truncate(limit.max(0) as usize);
    }
    let series = rows
        .iter()
        .map(|r| series_meta(&snap, dim, &r.series))
        .collect();

    Ok(dto::UsageTopResponse {
        rows: rows.iter().map(cell_to_dto).collect(),
        series,
        currency: snap.settings.currency.clone(),
    })
}

pub async fn top(State(st): State<SharedState>, Query(q): Query<TopQuery>) -> Response {
    ops_result(top_inner(&st, q).await.and_then(ok_value))
}

// ---------------------------------------------------------------------------
// GET /api/usage/heat
// ---------------------------------------------------------------------------

async fn heat_inner(st: &SharedState, q: HeatQuery) -> Result<dto::UsageHeatResponse, String> {
    let filter = parse_filter(RangeInput {
        from: q.from,
        to: q.to,
        alias: q.alias,
        key_id: q.key_id,
        class: q.class,
        upstream_id: q.upstream_id,
        tz: q.tz,
    })?;
    let raw = store::usage_heat(&st.db, &filter)
        .await
        .map_err(|e| e.to_string())?;
    let max = raw.iter().map(|(_, _, n)| *n).max().unwrap_or(0);
    let cells = raw
        .into_iter()
        .map(|(dow, hour, requests)| dto::HeatCell {
            dow,
            hour,
            requests,
        })
        .collect();
    Ok(dto::UsageHeatResponse { cells, max })
}

pub async fn heat(State(st): State<SharedState>, Query(q): Query<HeatQuery>) -> Response {
    ops_result(heat_inner(&st, q).await.and_then(ok_value))
}

// ---------------------------------------------------------------------------
// GET /api/usage/errors — refusals and errors by kind
// ---------------------------------------------------------------------------

async fn errors_inner(
    st: &SharedState,
    q: ErrorsQuery,
) -> Result<dto::UsageErrorsResponse, String> {
    let filter = parse_filter(RangeInput {
        from: q.from,
        to: q.to,
        class: q.class,
        alias: q.alias,
        key_id: q.key_id,
        upstream_id: None,
        tz: q.tz,
    })?;
    let snap = st.snapshot();
    let bucket = q
        .bucket
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(Bucket::parse)
        .unwrap_or(Bucket::Day);
    let buckets = bucket_labels(
        &st.db,
        &filter.from,
        &filter.to,
        bucket,
        filter.tz_offset_min,
    )
    .await?;
    let rows = store::usage_error_kinds(&st.db, &filter, bucket)
        .await
        .map_err(|e| e.to_string())?;

    // kind -> bucket -> count, then flattened onto the full bucket list so a
    // quiet day is a zero rather than a gap.
    let index: std::collections::HashMap<&str, usize> = buckets
        .iter()
        .enumerate()
        .map(|(i, b)| (b.as_str(), i))
        .collect();
    let mut by_kind: std::collections::HashMap<String, Vec<i64>> = std::collections::HashMap::new();
    for (bucket, kind, n) in &rows {
        let slot = by_kind
            .entry(kind.clone())
            .or_insert_with(|| vec![0; buckets.len()]);
        if let Some(i) = index.get(bucket.as_str()) {
            slot[*i] += n;
        }
    }

    let mut kinds: Vec<dto::ErrorKindSeries> = by_kind
        .into_iter()
        .map(|(kind, counts)| dto::ErrorKindSeries {
            refusal: crate::telemetry::REFUSAL_KINDS.contains(&kind.as_str()),
            total: counts.iter().sum(),
            kind,
            counts,
        })
        .collect();
    kinds.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.kind.cmp(&b.kind)));

    Ok(dto::UsageErrorsResponse {
        buckets,
        kinds,
        retention_days: snap.settings.retention_days,
    })
}

pub async fn errors(State(st): State<SharedState>, Query(q): Query<ErrorsQuery>) -> Response {
    ops_result(errors_inner(&st, q).await.and_then(ok_value))
}

// ---------------------------------------------------------------------------
// GET /api/usage/local — the local/cloud split and the counterfactual (§2.5)
// ---------------------------------------------------------------------------

async fn local_inner(st: &SharedState, q: LocalQuery) -> Result<dto::UsageLocalResponse, String> {
    let filter = parse_filter(RangeInput {
        from: q.from,
        to: q.to,
        alias: q.alias,
        key_id: q.key_id,
        class: q.class,
        upstream_id: None,
        tz: q.tz,
    })?;
    let snap = st.snapshot();

    let by_upstream = store::usage_top(&st.db, &filter, GroupBy::Upstream)
        .await
        .map_err(|e| e.to_string())?;
    let mut local_tokens = 0i64;
    let mut cloud_tokens = 0i64;
    let mut local_in = 0i64;
    let mut local_out = 0i64;
    let mut local_cached = 0i64;
    let mut local_reasoning = 0i64;
    for row in &by_upstream {
        let id: i64 = row.series.parse().unwrap_or(0);
        let total = row.tokens_in + row.tokens_out;
        if is_local_upstream_id(&snap, id) {
            local_tokens += total;
            local_in += row.tokens_in;
            local_out += row.tokens_out;
            local_cached += row.tokens_cached;
            local_reasoning += row.tokens_reasoning;
        } else {
            cloud_tokens += total;
        }
    }

    let totals = store::usage_totals(&st.db, &filter)
        .await
        .map_err(|e| e.to_string())?;
    let decode_tok_s =
        (totals.decode_ms > 0.0).then(|| totals.decode_tokens as f64 / (totals.decode_ms / 1000.0));
    let kv_reuse = (totals.prompt_n > 0).then(|| totals.cache_n as f64 / totals.prompt_n as f64);
    let draft_acceptance =
        (totals.draft_n > 0).then(|| totals.draft_accepted as f64 / totals.draft_n as f64);

    let reference_alias = snap.settings.local_reference_alias.trim().to_string();
    let (reference_alias_out, counterfactual_micro) = if reference_alias.is_empty() {
        (None, None)
    } else {
        let (up_id, up_model) = snap
            .aliases
            .get(&reference_alias.to_lowercase())
            .map(|a| (Some(a.upstream_id), Some(a.upstream_model_id.clone())))
            .unwrap_or((None, None));
        let prices = snap.prices_for(&reference_alias, up_id, up_model.as_deref());
        let counterfactual = prices.as_ref().and_then(|p| {
            pricing::price_tokens(
                &TokenUsage {
                    prompt: Some(local_in as u64),
                    completion: Some(local_out as u64),
                    cached_in: Some(local_cached as u64),
                    cache_write: None,
                    reasoning: Some(local_reasoning as u64),
                },
                Some(p),
            )
            .total_micro
        });
        (Some(reference_alias), counterfactual)
    };

    Ok(dto::UsageLocalResponse {
        local_tokens,
        cloud_tokens,
        counterfactual_micro,
        reference_alias: reference_alias_out,
        currency: snap.settings.currency.clone(),
        decode_tok_s,
        kv_reuse,
        draft_acceptance,
    })
}

pub async fn local(State(st): State<SharedState>, Query(q): Query<LocalQuery>) -> Response {
    ops_result(local_inner(&st, q).await.and_then(ok_value))
}

// ---------------------------------------------------------------------------
// GET /api/usage/keys
// ---------------------------------------------------------------------------

async fn key_last_used(pool: &SqlitePool, key_id: i64) -> Result<Option<String>, String> {
    let row = sqlx::query("SELECT MAX(bucket_utc) AS last FROM usage_hourly WHERE key_id = ?1")
        .bind(key_id)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(row.get::<Option<String>, _>("last"))
}

/// A sentinel far enough in the future that `bucket_utc < to` never excludes
/// real data — the symmetric counterpart to `BudgetPeriod::Total`'s
/// `"0000-01-01T00"` sentinel `from`.
const FAR_FUTURE_HOUR_KEY: &str = "9999-12-31T23";

async fn keys_inner(st: &SharedState) -> Result<dto::KeysResponse, String> {
    let snap = st.snapshot();
    let now = Utc::now();

    let mut keys = Vec::with_capacity(snap.api_keys.len());
    for k in &snap.api_keys {
        let period_start = k.policy.budget_period.start_hour_key(now);
        let spent_micro = store::key_spend_micro(&st.db, k.id, &period_start)
            .await
            .map_err(|e| e.to_string())?;
        let period_filter = UsageFilter {
            from: period_start,
            to: FAR_FUTURE_HOUR_KEY.to_string(),
            alias: None,
            key_id: Some(k.id),
            class: None,
            upstream_id: None,
            tz_offset_min: 0,
        };
        let period_totals = store::usage_totals(&st.db, &period_filter)
            .await
            .map_err(|e| e.to_string())?;
        let last_used = key_last_used(&st.db, k.id).await?;
        // A device's own connection facts (client-apps design §1.6); every
        // other kind has none.
        let device = k.kind == crate::config::ApiKeyKind::Device;
        let last_seen_at = if device {
            store::key_last_seen(&st.db, k.id)
                .await
                .map_err(|e| e.to_string())?
        } else {
            None
        };

        keys.push(dto::KeyRow {
            id: k.id,
            name: k.name.clone(),
            enabled: k.enabled,
            kind: k.kind.as_str().to_string(),
            scope_mode: k.policy.scope_mode.as_str().to_string(),
            scope_patterns: k.policy.scope_patterns.clone(),
            tool_scope_mode: k.policy.tool_scope_mode.as_str().to_string(),
            tool_scope_patterns: k.policy.tool_scope_patterns.clone(),
            budget_micro: k.policy.budget_micro,
            budget_period: k.policy.budget_period.as_str().to_string(),
            rpm_limit: k.policy.rpm_limit,
            tpm_limit: k.policy.tpm_limit,
            concurrency_limit: k.policy.concurrency_limit,
            expires_at: k.policy.expires_at.clone(),
            note: k.note.clone(),
            spent_micro,
            spent_unknown_requests: period_totals.cost_unknown_requests,
            spent_unknown_tokens: period_totals.cost_unknown_tokens,
            requests: period_totals.requests,
            last_used,
            last_seen_at,
            hosts_label: k.hosts_label.clone(),
            self_admin: k.self_admin.into(),
            online: st
                .devices
                .links
                .online(k.id)
                .into_iter()
                .map(str::to_string)
                .collect(),
            open_links: st
                .devices
                .links
                .counts(k.id)
                .into_iter()
                .map(|(kind, count)| lmgw_api_types::OpenLinks {
                    kind: kind.to_string(),
                    count,
                })
                .collect(),
        });
    }

    let global_period_start = snap.settings.global_budget_period.start_hour_key(now);
    let global_spent_micro = store::total_spend_micro(&st.db, &global_period_start)
        .await
        .map_err(|e| e.to_string())?;
    let global_totals = store::usage_totals(
        &st.db,
        &UsageFilter {
            from: global_period_start,
            to: FAR_FUTURE_HOUR_KEY.to_string(),
            ..Default::default()
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(dto::KeysResponse {
        keys,
        currency: snap.settings.currency.clone(),
        global_budget_micro: snap.settings.global_budget_micro,
        global_spent_micro,
        global_budget_period: snap.settings.global_budget_period.as_str().to_string(),
        global_spent_unknown_requests: global_totals.cost_unknown_requests,
        auth_enabled: snap.settings.auth_enabled,
    })
}

pub async fn keys(State(st): State<SharedState>) -> Response {
    ops_result(keys_inner(&st).await.and_then(ok_value))
}

// ---------------------------------------------------------------------------
// GET /api/usage/prices
// ---------------------------------------------------------------------------

async fn prices_inner(st: &SharedState) -> Result<dto::PricesResponse, String> {
    let snap = st.snapshot();
    let rows = store::list_prices(&st.db)
        .await
        .map_err(|e| e.to_string())?;
    let prices = rows
        .iter()
        .map(|p| dto::PriceRowView {
            id: p.id,
            scope_kind: p.scope_kind.as_str().to_string(),
            scope_key: p.scope_key.clone(),
            unit: p.unit.as_str().to_string(),
            price_in: p.price_in,
            price_out: p.price_out,
            price_cache_read: p.price_cache_read,
            price_cache_write: p.price_cache_write,
            source: p.source.as_str().to_string(),
            note: p.note.clone(),
            updated_at: p.updated_at.clone(),
            price: p.price,
        })
        .collect();

    // What the spend total cannot cover, and how much it has already failed to
    // cover: the configured aliases with no price *and* the passthrough models
    // the rollup has seen (`ops::unpriced_models`). A local model is free
    // rather than unpriced and never appears (design §2.4/§9).
    let unpriced_models = crate::ops::unpriced_models(st)
        .await?
        .into_iter()
        .map(|(name, requests)| dto::UnpricedModel { name, requests })
        .collect();

    Ok(dto::PricesResponse {
        prices,
        currency: snap.settings.currency.clone(),
        unpriced_models,
    })
}

pub async fn prices(State(st): State<SharedState>) -> Response {
    ops_result(prices_inner(&st).await.and_then(ok_value))
}

// ---------------------------------------------------------------------------
// GET /api/usage/export.csv — the raw rollup rows, streamed back verbatim.
// This gateway never uploads the owner's data anywhere; the response body
// *is* the export, and nothing is written to disk to produce it.
// ---------------------------------------------------------------------------

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// The billable units' six columns (billable-units design §8.5) are the
/// last: a spreadsheet that reads the older columns by position keeps
/// working. A quantity is what was measured, and 0 in an hour before
/// `units_since`, when nothing was recorded.
const CSV_HEADER: &str =
    "bucket_utc,key_id,key_name,alias,upstream_id,upstream_name,class,outcome,\
requests,tokens_in,tokens_out,tokens_cached,tokens_cache_write,tokens_reasoning,\
cost_micro,cost_unknown_requests,cost_unknown_tokens,\
ttfb_sum,ttfb_count,total_sum,total_count,total_min,total_max,\
decode_tokens,decode_ms,prefill_ms,prompt_n,cache_n,draft_n,draft_accepted,\
audio_in_ms,chars_in,images_out,\
cost_unknown_audio_in_ms,cost_unknown_chars_in,cost_unknown_images_out\n";

async fn export_csv_inner(st: &SharedState, q: ExportQuery) -> Result<(String, String), String> {
    let filter = parse_filter(RangeInput {
        from: q.from,
        to: q.to,
        alias: q.alias,
        key_id: q.key_id,
        class: q.class,
        upstream_id: q.upstream_id,
        tz: q.tz,
    })?;
    let snap = st.snapshot();

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT bucket_utc, key_id, alias, upstream_id, class, outcome,
                requests, tokens_in, tokens_out, tokens_cached, tokens_cache_write,
                tokens_reasoning,
                cost_micro, cost_unknown_requests, cost_unknown_tokens,
                ttfb_sum, ttfb_count, total_sum, total_count, total_min, total_max,
                decode_tokens, decode_ms, prefill_ms, prompt_n, cache_n, draft_n, draft_accepted,
                audio_in_ms, chars_in, images_out,
                cost_unknown_audio_in_ms, cost_unknown_chars_in, cost_unknown_images_out
         FROM usage_hourly WHERE bucket_utc >= ",
    );
    qb.push_bind(filter.from.clone());
    qb.push(" AND bucket_utc < ").push_bind(filter.to.clone());
    if let Some(a) = &filter.alias {
        qb.push(" AND alias = ").push_bind(a.clone());
    }
    if let Some(k) = filter.key_id {
        qb.push(" AND key_id = ").push_bind(k);
    }
    if let Some(c) = &filter.class {
        qb.push(" AND class = ").push_bind(c.clone());
    }
    if let Some(u) = filter.upstream_id {
        qb.push(" AND upstream_id = ").push_bind(u);
    }
    qb.push(" ORDER BY bucket_utc, alias, upstream_id, class, outcome");
    let rows = qb
        .build()
        .fetch_all(&st.db)
        .await
        .map_err(|e| e.to_string())?;

    let mut out = String::with_capacity(CSV_HEADER.len() + rows.len() * 160);
    out.push_str(CSV_HEADER);
    for r in &rows {
        let key_id: i64 = r.get("key_id");
        let upstream_id: i64 = r.get("upstream_id");
        let (upstream_name, _) = upstream_label_and_kind(&snap, upstream_id);
        let fields = [
            r.get::<String, _>("bucket_utc"),
            key_id.to_string(),
            key_label(&snap, key_id),
            r.get::<String, _>("alias"),
            upstream_id.to_string(),
            upstream_name,
            r.get::<String, _>("class"),
            r.get::<String, _>("outcome"),
            r.get::<i64, _>("requests").to_string(),
            r.get::<i64, _>("tokens_in").to_string(),
            r.get::<i64, _>("tokens_out").to_string(),
            r.get::<i64, _>("tokens_cached").to_string(),
            r.get::<i64, _>("tokens_cache_write").to_string(),
            r.get::<i64, _>("tokens_reasoning").to_string(),
            r.get::<i64, _>("cost_micro").to_string(),
            r.get::<i64, _>("cost_unknown_requests").to_string(),
            r.get::<i64, _>("cost_unknown_tokens").to_string(),
            r.get::<i64, _>("ttfb_sum").to_string(),
            r.get::<i64, _>("ttfb_count").to_string(),
            r.get::<i64, _>("total_sum").to_string(),
            r.get::<i64, _>("total_count").to_string(),
            r.get::<Option<i64>, _>("total_min")
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.get::<Option<i64>, _>("total_max")
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.get::<i64, _>("decode_tokens").to_string(),
            r.get::<f64, _>("decode_ms").to_string(),
            r.get::<f64, _>("prefill_ms").to_string(),
            r.get::<i64, _>("prompt_n").to_string(),
            r.get::<i64, _>("cache_n").to_string(),
            r.get::<i64, _>("draft_n").to_string(),
            r.get::<i64, _>("draft_accepted").to_string(),
            r.get::<i64, _>("audio_in_ms").to_string(),
            r.get::<i64, _>("chars_in").to_string(),
            r.get::<i64, _>("images_out").to_string(),
            r.get::<i64, _>("cost_unknown_audio_in_ms").to_string(),
            r.get::<i64, _>("cost_unknown_chars_in").to_string(),
            r.get::<i64, _>("cost_unknown_images_out").to_string(),
        ];
        out.push_str(
            &fields
                .iter()
                .map(|f| csv_field(f))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }

    let filename = format!("lmgw-usage_{}_{}.csv", filter.from, filter.to);
    Ok((filename, out))
}

pub async fn export_csv(State(st): State<SharedState>, Query(q): Query<ExportQuery>) -> Response {
    match export_csv_inner(&st, q).await {
        Ok((filename, body)) => (
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{filename}\""),
                ),
            ],
            body,
        )
            .into_response(),
        Err(message) => ops_result(Err(message)),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        store::open_in_memory().await.unwrap()
    }

    // -- empty-bucket fill ---------------------------------------------------

    #[tokio::test]
    async fn bucket_labels_lists_every_hour_even_with_no_data() {
        let pool = pool().await;
        let labels = bucket_labels(&pool, "2026-01-01T00", "2026-01-01T04", Bucket::Hour, 0)
            .await
            .unwrap();
        assert_eq!(
            labels,
            vec![
                "2026-01-01T00",
                "2026-01-01T01",
                "2026-01-01T02",
                "2026-01-01T03",
            ]
        );
    }

    #[tokio::test]
    async fn bucket_labels_lists_every_day_including_quiet_ones() {
        let pool = pool().await;
        // A quiet-day gap is exactly what a chart must not silently drop.
        let labels = bucket_labels(&pool, "2026-01-01T00", "2026-01-04T00", Bucket::Day, 0)
            .await
            .unwrap();
        assert_eq!(labels, vec!["2026-01-01", "2026-01-02", "2026-01-03"]);
    }

    #[tokio::test]
    async fn bucket_labels_applies_the_timezone_shift() {
        let pool = pool().await;
        // 2026-01-01T23 UTC is 2026-01-02T02 at UTC+3.
        let labels = bucket_labels(&pool, "2026-01-01T23", "2026-01-02T01", Bucket::Hour, 180)
            .await
            .unwrap();
        assert_eq!(labels, vec!["2026-01-02T02", "2026-01-02T03"]);
    }

    #[tokio::test]
    async fn bucket_labels_empty_range_is_empty() {
        let pool = pool().await;
        let labels = bucket_labels(&pool, "2026-01-01T00", "2026-01-01T00", Bucket::Hour, 0)
            .await
            .unwrap();
        assert!(labels.is_empty());
    }

    // -- Other fold -----------------------------------------------------------

    fn cell(bucket: &str, series: &str, cost_micro: i64, tokens: i64) -> UsageCell {
        UsageCell {
            bucket: bucket.to_string(),
            series: series.to_string(),
            cost_micro,
            tokens_in: tokens,
            requests: 1,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn fold_leaves_a_window_with_no_tail_untouched() {
        let cells = vec![cell("d1", "a", 100, 10), cell("d1", "b", 50, 5)];
        let ranked = vec!["a".to_string(), "b".to_string()];
        let (out, folded) = fold_tail_to_other(cells.clone(), &ranked, 6);
        assert!(!folded, "nothing past the limit — no Other series at all");
        assert_eq!(out, cells);
    }

    #[tokio::test]
    async fn fold_sums_the_tail_into_one_other_cell_per_bucket_never_a_ninth_series() {
        // 4 series, limit 2: "a" and "b" survive, "c" and "d" fold together.
        let cells = vec![
            cell("d1", "a", 400, 40),
            cell("d1", "b", 300, 30),
            cell("d1", "c", 200, 20),
            cell("d1", "d", 100, 10),
            cell("d2", "c", 20, 2),
            cell("d2", "d", 10, 1),
        ];
        let ranked = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        let (out, folded) = fold_tail_to_other(cells, &ranked, 2);
        assert!(folded);

        let kept: Vec<&UsageCell> = out.iter().filter(|c| c.series != OTHER_KEY).collect();
        assert_eq!(
            kept.len(),
            2,
            "never a 9th (or here, 3rd+4th) generated series"
        );
        assert!(kept.iter().all(|c| c.series == "a" || c.series == "b"));

        let other_d1 = out
            .iter()
            .find(|c| c.series == OTHER_KEY && c.bucket == "d1")
            .expect("folded d1 bucket exists");
        assert_eq!(other_d1.cost_micro, 300); // 200 + 100
        assert_eq!(other_d1.tokens_in, 30); // 20 + 10
        assert_eq!(other_d1.requests, 2);

        let other_d2 = out
            .iter()
            .find(|c| c.series == OTHER_KEY && c.bucket == "d2")
            .expect("folded d2 bucket exists");
        assert_eq!(other_d2.cost_micro, 30); // 20 + 10
        assert_eq!(other_d2.tokens_in, 3); // 2 + 1
    }

    /// The legend says how big the tail is: "Other (49 aliases)", not a
    /// grey swatch that could be one alias or fifty.
    #[tokio::test]
    async fn the_other_series_counts_what_it_folded() {
        let other = other_meta(55, 6);
        assert_eq!(other.key, OTHER_KEY);
        assert_eq!(other.slot, None, "Other wears the neutral, never a slot");
        assert_eq!(other.folded, Some(49));
        // Every named series says nothing about folding.
        let st = crate::state::AppState::init_for_tests().await.unwrap();
        let named = series_meta(&st.snapshot(), GroupBy::Alias, "gemma4-12b");
        assert_eq!(named.folded, None);
    }

    #[tokio::test]
    async fn fold_at_exactly_the_limit_folds_nothing() {
        let cells = vec![cell("d1", "a", 1, 1), cell("d1", "b", 1, 1)];
        let ranked = vec!["a".to_string(), "b".to_string()];
        let (out, folded) = fold_tail_to_other(cells, &ranked, 2);
        assert!(!folded);
        assert!(out.iter().all(|c| c.series != OTHER_KEY));
    }

    // -- the cache-write tier -------------------------------------------------

    /// Migration 0030 gave the 1.25x tier a column because it had nowhere to go
    /// in the rollup. Everything downstream of the rollup has to carry it too,
    /// or it stays invisible for exactly the reason the migration names.
    #[tokio::test]
    async fn the_wire_cell_carries_the_cache_write_tier() {
        let mut c = cell("d1", "claude-opus-5", 900, 1_000);
        c.tokens_cached = 600;
        c.tokens_cache_write = 200;
        let dto = cell_to_dto(&c);
        assert_eq!(dto.tokens_cache_write, 200);
        // Still the *total*, cache included — fresh input is the remainder the
        // reader subtracts, never a fourth number that can drift.
        assert_eq!(dto.tokens_in, 1_000);
    }

    /// The fold is a sum, so anything it forgets reads as zero for the whole
    /// tail — and a zero is indistinguishable from "that series had none".
    #[tokio::test]
    async fn fold_carries_the_cache_tier_and_the_prefill_clock_into_other() {
        let mut c = cell("d1", "c", 200, 20);
        c.tokens_cache_write = 7;
        c.prefill_ms = 120.0;
        let mut d = cell("d1", "d", 100, 10);
        d.tokens_cache_write = 3;
        d.prefill_ms = 80.0;
        let ranked = vec!["a".to_string(), "c".to_string(), "d".to_string()];
        let (out, folded) = fold_tail_to_other(vec![c, d], &ranked, 1);
        assert!(folded);
        let other = out
            .iter()
            .find(|c| c.series == OTHER_KEY)
            .expect("the tail folded");
        assert_eq!(other.tokens_cache_write, 10);
        assert_eq!(
            other.prefill_ms, 200.0,
            "a folded series keeps its prefill clock; 0 ms reads as infinitely fast prefill"
        );
    }

    #[tokio::test]
    async fn fold_carries_the_measured_quantities_and_their_remainder_into_other() {
        let mut c = cell("d1", "c", 200, 0);
        c.audio_in_ms = 27_000;
        c.images_out = 2;
        c.cost_unknown_audio_in_ms = 27_000;
        let mut d = cell("d1", "d", 100, 0);
        d.chars_in = 1_234;
        d.cost_unknown_chars_in = 1_234;
        d.cost_unknown_images_out = 1;
        let ranked = vec!["a".to_string(), "c".to_string(), "d".to_string()];
        let (out, _) = fold_tail_to_other(vec![c, d], &ranked, 1);
        let other = out
            .iter()
            .find(|c| c.series == OTHER_KEY)
            .expect("the tail folded");
        // Billable-units §8.3: a column is carried end to end or not at all.
        assert_eq!(
            (other.audio_in_ms, other.chars_in, other.images_out),
            (27_000, 1_234, 2)
        );
        assert_eq!(
            (
                other.cost_unknown_audio_in_ms,
                other.cost_unknown_chars_in,
                other.cost_unknown_images_out
            ),
            (27_000, 1_234, 1)
        );
        let wire = cell_to_dto(other);
        assert_eq!(wire.audio_in_ms, 27_000);
        assert_eq!(wire.cost_unknown_images_out, 1);
    }

    #[tokio::test]
    async fn csv_export_names_and_fills_the_cache_write_column() {
        let st = crate::state::AppState::init_for_tests().await.unwrap();
        // One Anthropic-shaped turn: 1000 in, of which 600 read from cache and
        // 200 written into it — three tiers at three prices.
        store::insert_request_log(
            &st.db,
            &store::NewRequestLog {
                ingress_proto: "anthropic".into(),
                requested_alias: "claude-opus-5".into(),
                status: 200,
                prompt_tokens: Some(1_000),
                completion_tokens: Some(100),
                cached_in_tokens: Some(600),
                cache_write_tokens: Some(200),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let (_, csv) = export_csv_inner(
            &st,
            ExportQuery {
                from: None,
                to: None,
                class: None,
                alias: None,
                key_id: None,
                upstream_id: None,
                tz: None,
            },
        )
        .await
        .unwrap();

        let mut lines = csv.lines();
        let head: Vec<&str> = lines.next().expect("a header").split(',').collect();
        let row: Vec<&str> = lines.next().expect("the seeded hour").split(',').collect();
        let at = |name: &str| {
            let i = head
                .iter()
                .position(|h| *h == name)
                .unwrap_or_else(|| panic!("the export names {name}"));
            row[i]
        };
        assert_eq!(at("tokens_cache_write"), "200");
        assert_eq!(at("tokens_cached"), "600");
        assert_eq!(at("tokens_in"), "1000");
    }

    // -- identity / labels ------------------------------------------------

    #[tokio::test]
    async fn key_label_zero_is_no_key_and_unknown_id_says_so() {
        let st = crate::state::AppState::init_for_tests().await.unwrap();
        let snap = st.snapshot();
        assert_eq!(key_label(&snap, 0), "(no key)");
        assert_eq!(key_label(&snap, 12345), "key 12345");
    }

    #[tokio::test]
    async fn synthetic_local_upstreams_are_local_and_named() {
        let st = crate::state::AppState::init_for_tests().await.unwrap();
        let snap = st.snapshot();
        assert!(is_local_upstream_id(&snap, config::ROUTER_UPSTREAM_ID));
        assert!(is_local_upstream_id(&snap, config::AUX_UPSTREAM_ID));
        assert!(is_local_upstream_id(&snap, config::AUDIO_UPSTREAM_ID));
        assert!(is_local_upstream_id(&snap, config::IMAGE_UPSTREAM_ID));
        assert!(!is_local_upstream_id(&snap, 0));
        for (id, want) in [
            (config::AUDIO_UPSTREAM_ID, config::AUDIO_UPSTREAM_NAME),
            (config::IMAGE_UPSTREAM_ID, config::IMAGE_UPSTREAM_NAME),
        ] {
            let (name, local) = upstream_label_and_kind(&snap, id);
            assert_eq!(name, want);
            assert!(local, "upstream {id} is a local class");
        }
    }
}
