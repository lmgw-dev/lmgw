//! Prices & usage (usage-analytics design §2.2, §2.3, §5; billable-units
//! design §8)

use serde_json::{json, Value};

use crate::catalog;
use crate::config::PriceScope;
use crate::pricing::micro_to_units;
use crate::state::SharedState;
use crate::store::{self, GroupBy, UsageFilter};
use crate::telemetry::RequestClass;

mod units;
pub use units::PriceRates;

/// Sync catalog-advertised prices into the `prices` table and report what
/// happened, per upstream: rows written, models that advertised no usable
/// price (written nowhere — §2.3), and any upstream whose catalog fetch
/// failed. See [`catalog::sync_prices`] for the mechanics.
pub async fn prices_sync(state: &SharedState) -> Result<Value, String> {
    let summary = catalog::sync_prices(state).await?;
    serde_json::to_value(&summary).map_err(|e| e.to_string())
}

/// Every model the gateway serves that resolves to **no price at all**, with
/// the requests each has already spent unpriced, busiest first.
///
/// Two sources, because neither alone is the answer. The configured aliases
/// catch a model that is about to be a problem; the **usage rollup** catches
/// the passthrough models an `expose_all` upstream serves. Those are never
/// configured rows, and for an owner who runs entirely on passthrough they are
/// the only models there are — so a worklist built from aliases alone came
/// back empty in exactly the case it exists for.
///
/// Resolution goes through [`Snapshot::sheet_for`](crate::config::Snapshot::sheet_for), the same path a real
/// request takes, over **every unit** (billable-units §8.2): a model is listed
/// when no unit resolves a usable row, so a scope priced per minute of audio
/// or per request alone is priced. A local model never appears (it resolves
/// `free_local` — free, not unpriced), and a logged name that no longer routes
/// anywhere — an MCP tool call, a model since deleted — is skipped rather than
/// offered as something to price. A scope whose rows its route never measures
/// (a token row on a binary-speech alias) is priced here and still writes
/// unpriced rows: that is a fact about its traffic, and it shows in the
/// remainder of `lmgw__usage` and the Usage page, not on this list.
pub async fn unpriced_models(state: &SharedState) -> Result<Vec<(String, i64)>, String> {
    let snap = state.snapshot();
    let used = store::used_model_names(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let configured = snap
        .enabled_aliases()
        .iter()
        .map(|a| (a.alias.clone(), 0i64))
        .collect::<Vec<_>>();

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<(String, i64)> = Vec::new();
    for (name, requests) in used.into_iter().chain(configured) {
        if !seen.insert(name.to_lowercase()) {
            continue;
        }
        let Ok(route) = snap.resolve(&name) else {
            continue;
        };
        if !snap
            .sheet_for(&name, Some(route.upstream.id), Some(&route.upstream_model))
            .is_priced()
        {
            out.push((name, requests));
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(out)
}

/// One row of `prices`, as JSON.
fn price_row_view(r: &crate::config::PriceRow) -> Value {
    json!({
        "id": r.id,
        "scope_kind": r.scope_kind.as_str(),
        "scope_key": r.scope_key,
        "unit": r.unit,
        "price_in": r.price_in,
        "price_out": r.price_out,
        "price_cache_read": r.price_cache_read,
        "price_cache_write": r.price_cache_write,
        "price": r.price,
        "source": r.source.as_str(),
        "note": r.note,
        "updated_at": r.updated_at,
    })
}

/// Every price sheet on file, plus the enabled aliases that resolve to no
/// price at all — the worklist for `price_set` (an upstream that publishes no
/// pricing, e.g. Gemini, is priced by hand or not at all, and this is how an
/// owner or agent finds out which aliases that currently means).
pub async fn prices(state: &SharedState) -> Result<Value, String> {
    let snap = state.snapshot();
    let rows = store::list_prices(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let sheets: Vec<Value> = rows.iter().map(price_row_view).collect();

    let unpriced: Vec<Value> = unpriced_models(state)
        .await?
        .into_iter()
        .map(|(name, requests)| json!({ "model": name, "unpriced_requests": requests }))
        .collect();

    Ok(json!({
        "currency": snap.settings.currency,
        "sheets": sheets,
        "unpriced_models": unpriced,
        "hint": format!(
            "Each sheet prices one scope in one unit, and a scope may hold a row per unit \
             (tokens plus a per-request fee, say); a request costs the sum of every unit \
             priced for it, and is unpriced when any of those units' quantities is \
             unknown. Units: {}. A manual row always wins over a catalog row for the same \
             scope and unit. lmgw__prices_sync refreshes the catalog rows; lmgw__price_set \
             writes a manual one (needed for upstreams like Gemini that publish no pricing \
             at all). unpriced_models covers configured aliases *and* the passthrough \
             models an expose_all upstream serves, counted from the usage rollup, and lists \
             a model only when no unit resolves a price for it, through the same order a \
             real request uses — so a local model never appears here: it is free_local, \
             not unpriced.",
            units::unit_table()
        ),
    }))
}

/// Manual price upsert (usage-analytics design §2.2) — always wins over a
/// catalog row for the same scope and unit, because `store::upsert_price`
/// keys the unique index on `(scope_kind, scope_key, source, unit)` and this
/// always writes `source = manual`.
///
/// `unit` is the row's billable unit (billable-units design §8.2), omitted
/// for tokens: a `per_mtok` row takes the four token rates, per 1M tokens,
/// any other unit takes `price`, per its scale. The two shapes are checked
/// here, once for the tool and the dashboard alike. An existing row's unit is
/// part of its key, so a call in another unit writes a second row beside it.
pub async fn price_set(
    state: &SharedState,
    scope_kind: &str,
    scope_key: Option<&str>,
    unit: Option<&str>,
    rates: PriceRates,
    note: Option<&str>,
) -> Result<Value, String> {
    let scope = match scope_kind.trim() {
        "alias" => PriceScope::Alias,
        "upstream_model" => PriceScope::UpstreamModel,
        other => {
            return Err(format!(
                "unknown scope_kind '{other}' (alias|upstream_model)"
            ))
        }
    };
    let scope_key = scope_key
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "scope_key is required — the alias name, or lmgw__prices' \
             '<upstream_id>:<upstream_model_id>' for scope_kind=upstream_model"
                .to_string()
        })?;
    let unit = units::parse_unit(unit)?;
    let (p, price) = units::manual_row(unit, rates)?;
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    store::upsert_price(&state.db, scope, scope_key, unit, &p, price, note)
        .await
        .map_err(|e| e.to_string())?;
    state.reload_snapshot().await.map_err(|e| e.to_string())?;
    Ok(json!({
        "ok": true,
        "scope_kind": scope.as_str(),
        "scope_key": scope_key,
        "unit": unit.as_str(),
        "source": "manual",
    }))
}

/// Delete one price row by id (from `lmgw__prices`).
pub async fn price_delete(state: &SharedState, id: i64) -> Result<Value, String> {
    store::delete_price(&state.db, id)
        .await
        .map_err(|e| e.to_string())?;
    state.reload_snapshot().await.map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true, "id": id }))
}

/// Format a cost total together with its unpriced remainder — the §2.3 UI
/// obligation applied to every total this module returns, not only the ones
/// a chart draws: "€12.84" alone would silently drop however much of the
/// window has no price on file. The remainder names its tokens and every
/// quantity it holds (billable-units §8.3): "3 unpriced requests (0 tokens,
/// 4.5 min audio)".
fn money_with_unpriced(c: &store::UsageCell, currency: &str) -> String {
    let base = format!("{:.2} {currency}", micro_to_units(c.cost_micro));
    let n = c.cost_unknown_requests;
    if n == 0 {
        return base;
    }
    format!(
        "{base} · {n} unpriced request{} ({})",
        if n == 1 { "" } else { "s" },
        units::remainder_holds(c)
    )
}

/// One [`store::UsageCell`] rendered for `lmgw__usage` — money formatted with
/// its unpriced remainder stated beside it, latency and decode speed averaged
/// from the sums rather than left as raw hist inputs a caller cannot read.
fn usage_cell_view(c: &store::UsageCell, currency: &str) -> Value {
    let avg_ttfb_ms = (c.ttfb_count > 0).then(|| c.ttfb_sum as f64 / c.ttfb_count as f64);
    let avg_total_ms = (c.total_count > 0).then(|| c.total_sum as f64 / c.total_count as f64);
    let decode_tok_s = (c.decode_ms > 0.0).then(|| c.decode_tokens as f64 / (c.decode_ms / 1000.0));
    json!({
        "series": if c.series.is_empty() { Value::Null } else { json!(c.series) },
        "requests": c.requests,
        "tokens_in": c.tokens_in,
        "tokens_out": c.tokens_out,
        "tokens_cached": c.tokens_cached,
        "tokens_cache_write": c.tokens_cache_write,
        "tokens_reasoning": c.tokens_reasoning,
        "audio_in_ms": c.audio_in_ms,
        "chars_in": c.chars_in,
        "images_out": c.images_out,
        "cost": money_with_unpriced(c, currency),
        "cost_micro": c.cost_micro,
        "unpriced_requests": c.cost_unknown_requests,
        "unpriced_tokens": c.cost_unknown_tokens,
        "unpriced_audio_in_ms": c.cost_unknown_audio_in_ms,
        "unpriced_chars_in": c.cost_unknown_chars_in,
        "unpriced_images_out": c.cost_unknown_images_out,
        "errors": c.errors,
        "refusals": c.refusals,
        "avg_ttfb_ms": avg_ttfb_ms,
        "avg_total_ms": avg_total_ms,
        "decode_tok_s": decode_tok_s,
    })
}

/// `usage_top`'s ranked rows, folded to `limit` with the tail summed into a
/// real `Other` row — the design's own rule for charts (§5 `limit`) applied
/// here too, so a breakdown with more series than `limit` never just goes
/// silently missing.
fn fold_other(mut rows: Vec<store::UsageCell>, limit: usize) -> Vec<store::UsageCell> {
    if rows.len() <= limit {
        return rows;
    }
    let tail = rows.split_off(limit);
    let mut other = store::UsageCell {
        series: "Other".to_string(),
        ..Default::default()
    };
    for r in &tail {
        other.requests += r.requests;
        other.tokens_in += r.tokens_in;
        other.tokens_out += r.tokens_out;
        other.tokens_cached += r.tokens_cached;
        other.tokens_cache_write += r.tokens_cache_write;
        other.tokens_reasoning += r.tokens_reasoning;
        other.cost_micro += r.cost_micro;
        other.cost_unknown_requests += r.cost_unknown_requests;
        other.cost_unknown_tokens += r.cost_unknown_tokens;
        other.errors += r.errors;
        other.refusals += r.refusals;
        other.ttfb_sum += r.ttfb_sum;
        other.ttfb_count += r.ttfb_count;
        other.total_sum += r.total_sum;
        other.total_count += r.total_count;
        other.decode_tokens += r.decode_tokens;
        other.decode_ms += r.decode_ms;
        other.prefill_ms += r.prefill_ms;
        other.prompt_n += r.prompt_n;
        other.cache_n += r.cache_n;
        other.draft_n += r.draft_n;
        other.draft_accepted += r.draft_accepted;
        other.audio_in_ms += r.audio_in_ms;
        other.chars_in += r.chars_in;
        other.images_out += r.images_out;
        other.cost_unknown_audio_in_ms += r.cost_unknown_audio_in_ms;
        other.cost_unknown_chars_in += r.cost_unknown_chars_in;
        other.cost_unknown_images_out += r.cost_unknown_images_out;
    }
    rows.push(other);
    rows
}

/// Parse `lmgw__usage`'s `from`/`to` into the `[from, to)` UTC hour-key range
/// `store::UsageFilter` wants, at day granularity.
///
/// `to`: an ISO date (`YYYY-MM-DD`); omitted means "through today" (UTC).
/// `from`: an ISO date, or an `<N>d` shorthand meaning N days before `to`
/// (`7d`, `30d`, …); omitted means `30d`. The range covers whole days,
/// `from` 00:00 through the end of `to`, both inclusive.
fn parse_usage_range(from: Option<&str>, to: Option<&str>) -> Result<(String, String), String> {
    use chrono::{Duration, NaiveDate, Utc};

    let today = Utc::now().date_naive();
    let to_date = match to.map(str::trim).filter(|s| !s.is_empty()) {
        None => today,
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| format!("invalid 'to' date '{s}' (expected YYYY-MM-DD)"))?,
    };
    let from_date = match from.map(str::trim).filter(|s| !s.is_empty()) {
        None => to_date - Duration::days(30),
        Some(s) => match s.strip_suffix('d').and_then(|n| n.parse::<i64>().ok()) {
            Some(n) => to_date - Duration::days(n),
            None => NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| {
                format!("invalid 'from' date '{s}' (expected YYYY-MM-DD, or e.g. '7d')")
            })?,
        },
    };
    if from_date > to_date {
        return Err(format!("'from' ({from_date}) is after 'to' ({to_date})"));
    }
    let from_key = format!("{}T00", from_date.format("%Y-%m-%d"));
    // Exclusive upper bound: the hour key of the day *after* `to_date`, so
    // `to`'s whole day is included.
    let to_key = format!("{}T00", (to_date + Duration::days(1)).format("%Y-%m-%d"));
    Ok((from_key, to_key))
}

/// What did I spend, on what — the query behind `lmgw__usage`, over the same
/// `usage_hourly` rollups the (future) dashboard reads, so a chart and this
/// tool can never disagree (§5).
#[allow(clippy::too_many_arguments)]
pub async fn usage(
    state: &SharedState,
    from: Option<&str>,
    to: Option<&str>,
    group_by: Option<&str>,
    class: Option<&str>,
    alias: Option<&str>,
    limit: Option<i64>,
) -> Result<Value, String> {
    let (from_key, to_key) = parse_usage_range(from, to)?;
    let group = match group_by
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("alias")
    {
        "alias" => GroupBy::Alias,
        "key" => GroupBy::Key,
        "class" => GroupBy::Class,
        "upstream" => GroupBy::Upstream,
        "none" => GroupBy::None,
        other => {
            return Err(format!(
                "unknown group_by '{other}' (alias|key|class|upstream|none)"
            ))
        }
    };
    const CLASSES: [RequestClass; 5] = [
        RequestClass::Chat,
        RequestClass::Aux,
        RequestClass::Audio,
        RequestClass::Image,
        RequestClass::Tool,
    ];
    let class = match class.map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(c) if CLASSES.iter().any(|k| k.as_str() == c) => Some(c.to_string()),
        Some(other) => {
            return Err(format!(
                "unknown class '{other}' (chat|aux|audio|image|tool)"
            ))
        }
    };

    let filter = UsageFilter {
        from: from_key.clone(),
        to: to_key.clone(),
        alias: alias
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
        key_id: None,
        class,
        upstream_id: None,
        // No browser here to read a timezone from; report in UTC and say so,
        // rather than silently mislabeling a day boundary (§3.4).
        tz_offset_min: 0,
    };

    let currency = state.snapshot().settings.currency.clone();
    let totals = store::usage_totals(&state.db, &filter)
        .await
        .map_err(|e| e.to_string())?;

    let breakdown = if group == GroupBy::None {
        Vec::new()
    } else {
        let limit = limit.unwrap_or(10).clamp(1, 100) as usize;
        let rows = store::usage_top(&state.db, &filter, group)
            .await
            .map_err(|e| e.to_string())?;
        fold_other(rows, limit)
    };

    let units_since = store::units_since(&state.db)
        .await
        .map_err(|e| e.to_string())?;

    Ok(json!({
        "from": from_key,
        "to": to_key,
        "timezone": "UTC",
        "currency": currency,
        "units_since": units_since,
        "totals": usage_cell_view(&totals, &currency),
        "group_by": group_by.unwrap_or("alias"),
        "breakdown": breakdown.iter().map(|c| usage_cell_view(c, &currency)).collect::<Vec<_>>(),
        "hint": "cost is money in every billable unit (tokens, audio minutes, characters, \
                 images, request fees) and states its unpriced remainder inline, with the \
                 tokens and quantities that remainder holds — a total with unpriced requests \
                 folded in understates what was actually spent, not the whole story. \
                 audio_in_ms, chars_in and images_out are measured quantities (input audio, \
                 input characters, generated images), recorded since units_since; before it \
                 they read 0 because nothing was recorded. Sync catalog prices with \
                 lmgw__prices_sync, or check lmgw__prices for models with no price on file \
                 at all.",
    }))
}
