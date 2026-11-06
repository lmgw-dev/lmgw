//! Prices & usage (usage-analytics design §2.2, §2.3, §5; billable-units
//! design §8)

use lmgw_api_types as dto;
use lmgw_api_types::self_admin::{Prices, Usage, UsageCellView};

use crate::catalog;
use crate::config::{PriceRow, PriceScope, PriceTier};
use crate::detached::detached;
use crate::pricing::{micro_to_units, PriceSource};
use crate::principal::Refusal;
use crate::state::SharedState;
use crate::store::{self, GroupBy, UsageFilter};
use crate::telemetry::RequestClass;

mod hosted;
mod units;
pub use hosted::unpriced_hosted_tools;
pub use units::PriceRates;

/// Sync catalog-advertised prices into the `prices` table and report what
/// happened, per upstream: rows written, models that advertised no usable
/// price (written nowhere — §2.3), and any upstream whose catalog fetch
/// failed. See [`catalog::sync_prices`] for the mechanics.
///
/// The whole sync is a task of its own (*changed 2026-10-24*, see
/// [`crate::detached::detached`]): it writes its rows one by one and
/// reloads after the last, so a dropped request would leave rows written
/// and the snapshot pricing by the old ones.
pub async fn prices_sync(
    state: &SharedState,
) -> Result<dto::PriceSyncSummary, crate::principal::Refusal> {
    let st = state.clone();
    detached("syncing prices", async move {
        catalog::sync_prices(&st).await.map_err(Into::into)
    })
    .await?
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
        // What the scope costs, not what one answer cost: Standard. A scope
        // priced at Standard that answers at another tier is on
        // [`unpriced_tiers`]' list instead.
        if !snap
            .sheet_for(
                &name,
                Some(route.upstream.id),
                Some(&route.upstream_model),
                Some(PriceTier::Standard),
            )
            .is_priced()
        {
            out.push((name, requests));
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(out)
}

/// Every model that answered at an OpenAI service tier it has no price row
/// for (openai egress design §5.6), with the requests left unpriced since
/// the first of them, busiest first: that traffic counts toward no budget
/// until a row of the tier is entered (decision 15).
///
/// Read from the raw log ([`store::unpriced_tier_traffic`]) and asked of
/// today's rows through [`Snapshot::sheet_for`](crate::config::Snapshot::sheet_for)
/// at the tier, under the alias the group was priced under, the path a request
/// takes: a group whose tier has a row now drops off the list (its old rows
/// keep the cost they were written with). Each entry names the units the row
/// has to price. A reported value that names no price tier (`auto`, none) has
/// no row to enter: it stays listed, without a tier, until its traffic leaves
/// the log. The row to enter is named on the upstream model's scope, which is
/// what an alias resolves to beneath its own rows.
pub async fn unpriced_tiers(state: &SharedState) -> Result<Vec<dto::UnpricedTier>, String> {
    let snap = state.snapshot();
    let traffic = store::unpriced_tier_traffic(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let mut out: Vec<dto::UnpricedTier> = Vec::new();
    for t in traffic {
        let (Some(up), Some(model)) = (t.upstream_id, t.upstream_model.as_deref()) else {
            continue;
        };
        let reported = t.service_tier.as_deref().unwrap_or_default();
        // `None`: the answers named no tier a row prices (`auto`, a value
        // that is new, none at all). No row can fix that, so it is listed
        // without an action.
        let tier = PriceTier::from_reported(reported);
        let mut missing: Vec<String> = Vec::new();
        if let Some(tier) = tier {
            // The rows the group was priced by: the answering alias's.
            let alias = t.priced_alias.as_deref().unwrap_or(&t.alias);
            let Some(gap) = snap
                .sheet_for(alias, Some(up), Some(model), Some(tier))
                .tier_gap
            else {
                continue;
            };
            missing = gap
                .missing_units()
                .map(|u| u.as_str().to_string())
                .collect();
        }
        let tier_name = tier.map_or("", PriceTier::as_str);
        let reported = if tier.is_some() { "" } else { reported };
        let scope_key = crate::config::upstream_scope_key(up, model);
        // `priority` and `fast` are one price tier: one entry.
        if let Some(e) = out.iter_mut().find(|e| {
            e.name == t.alias
                && e.scope_key == scope_key
                && e.service_tier == tier_name
                && e.reported == reported
        }) {
            e.requests += t.requests;
            if t.first_ts < e.since {
                e.since = t.first_ts;
            }
            for u in missing {
                if !e.missing_units.contains(&u) {
                    e.missing_units.push(u);
                }
            }
            continue;
        }
        out.push(dto::UnpricedTier {
            name: t.alias,
            scope_kind: PriceScope::UpstreamModel.as_str().to_string(),
            scope_key,
            service_tier: tier_name.to_string(),
            reported: reported.to_string(),
            missing_units: missing,
            requests: t.requests,
            since: t.first_ts,
        });
    }
    out.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// One row of `prices`, as the dashboard's Prices page and `lmgw__prices`
/// both list it. Every field of the row is named: one added to [`PriceRow`] or
/// to the view fails the build here, where the answer is decided.
pub fn price_row_view(r: &PriceRow) -> dto::PriceRowView {
    let PriceRow {
        id,
        scope_kind,
        scope_key,
        unit,
        price_in,
        price_out,
        price_cache_read,
        price_cache_write,
        source,
        note,
        updated_at,
        price,
        service_tier,
    } = r;
    dto::PriceRowView {
        id: *id,
        scope_kind: scope_kind.as_str().to_string(),
        scope_key: scope_key.clone(),
        unit: unit.as_str().to_string(),
        price_in: *price_in,
        price_out: *price_out,
        price_cache_read: *price_cache_read,
        price_cache_write: *price_cache_write,
        source: source.as_str().to_string(),
        note: note.clone(),
        updated_at: updated_at.clone(),
        price: *price,
        service_tier: service_tier.as_str().to_string(),
    }
}

/// Every price sheet on file, plus the enabled aliases that resolve to no
/// price at all — the worklist for `price_set` (an upstream that publishes no
/// pricing, e.g. Gemini, is priced by hand or not at all, and this is how an
/// owner or agent finds out which aliases that currently means).
pub async fn prices(state: &SharedState) -> Result<Prices, String> {
    let snap = state.snapshot();
    let rows = store::list_prices(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let sheets = rows.iter().map(price_row_view).collect();

    let unpriced = unpriced_models(state)
        .await?
        .into_iter()
        .map(|(name, requests)| dto::UnpricedModel { name, requests })
        .collect();
    let unpriced_tiers = unpriced_tiers(state).await?;
    let unpriced_hosted_tools = unpriced_hosted_tools(state).await?;

    Ok(Prices {
        currency: snap.settings.currency.clone(),
        sheets,
        unpriced_models: unpriced,
        unpriced_tiers,
        unpriced_hosted_tools,
        hint: format!(
            "Each sheet prices one scope in one unit at one service tier, and a scope may \
             hold a row per unit (tokens plus a per-request fee, say); a request costs the \
             sum of every unit priced for it, and is unpriced when any of those units' \
             quantities is unknown. Units: {}. Service tiers ({}) are OpenAI's pricing \
             tiers: an OpenAI platform answer is priced with the rows of the tier it \
             reported (default is standard, priority is fast), and is unpriced, with a \
             price note, when that tier has no row — never priced at standard. Every other \
             provider's rows are standard. A manual row always wins over a catalog row for \
             the same scope, unit and tier. lmgw__prices_sync refreshes the catalog rows \
             (standard only); lmgw__price_set writes a manual one (needed for upstreams \
             like Gemini that publish no pricing at all, and for every tier but standard). \
             unpriced_models ({{name, requests}}) covers configured aliases *and* the \
             passthrough models an expose_all upstream serves, counted from the usage \
             rollup, and lists a model only when no unit resolves a standard price for it, \
             through the same order a real request uses — so a local model never appears \
             here: it is free_local, not unpriced. unpriced_tiers ({{name, scope_kind, \
             scope_key, service_tier, reported, missing_units, requests, since}}) lists the models \
             that answered at a tier with no row, from the request log (an empty \
             service_tier means the answers named no tier a row can price: reported \
             says what they did name, and no price fixes it): a lmgw__price_set with that \
             scope, service_tier and a missing unit prices what comes next. \
             unpriced_hosted_tools ({{name, scope_key, calls: [{{item_type, calls}}], \
             requests, since}}) lists the models whose native /v1/responses answers ran a \
             hosted tool that bills beyond its tokens (web_search_call, file_search_call, \
             code_interpreter_call, image_generation_call, a hosted shell_call), from the \
             request log: such a row is unpriced, its price_note naming the calls and what \
             the tokens alone would cost, and no price row fixes it yet.",
            units::unit_table(),
            PriceTier::ALL.map(PriceTier::as_str).join(", "),
        ),
    })
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
///
/// `service_tier` is the OpenAI service tier the row prices (openai egress
/// design §5.7), omitted for `standard`; it is part of the key too, so a
/// flex row stands beside the standard one.
#[allow(clippy::too_many_arguments)]
pub async fn price_set(
    state: &SharedState,
    scope_kind: &str,
    scope_key: Option<&str>,
    unit: Option<&str>,
    service_tier: Option<&str>,
    rates: PriceRates,
    note: Option<&str>,
) -> Result<dto::PriceSaved, String> {
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
    let tier = parse_tier(service_tier)?;
    let (p, price) = units::manual_row(unit, rates)?;
    let note = note
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    // The write and the reload are a task of their own (*changed
    // 2026-10-24*, see [`crate::detached::detached`]): a dropped request
    // would leave the snapshot pricing by the old row.
    let st = state.clone();
    let key = scope_key.to_string();
    detached("saving the price", async move {
        store::upsert_price(&st.db, scope, &key, unit, tier, &p, price, note.as_deref())
            .await
            .map_err(|e| e.to_string())?;
        st.reload_snapshot().await.map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    })
    .await
    .map_err(|r| r.message)??;
    Ok(dto::PriceSaved {
        ok: true,
        scope_kind: scope.as_str().to_string(),
        scope_key: scope_key.to_string(),
        unit: unit.as_str().to_string(),
        service_tier: tier.as_str().to_string(),
        source: PriceSource::Manual.as_str().to_string(),
    })
}

/// A `price_set` call's `service_tier`: `standard` when it names none, an
/// unknown one refused by name.
fn parse_tier(tier: Option<&str>) -> Result<PriceTier, String> {
    match tier.map(str::trim).filter(|t| !t.is_empty()) {
        None => Ok(PriceTier::Standard),
        Some(t) => PriceTier::parse(t).ok_or_else(|| {
            format!(
                "unknown service_tier '{t}' ({}); OpenAI's response values map onto them: \
                 default is standard, priority is fast",
                PriceTier::ALL.map(PriceTier::as_str).join("|")
            )
        }),
    }
}

/// Delete one price row by id (from `lmgw__prices`).
///
/// An id no row has is a [`Refusal::not_found`]: `404 not_found` on the HTTP
/// plane, its message on the tool plane.
pub async fn price_delete(state: &SharedState, id: i64) -> Result<dto::IdAck, Refusal> {
    // The delete and the reload are a task of their own (*changed
    // 2026-10-24*, see [`crate::detached::detached`]): a dropped request
    // would leave the snapshot pricing by the deleted row.
    let st = state.clone();
    let existed = detached("deleting the price", async move {
        let existed = store::delete_price(&st.db, id)
            .await
            .map_err(|e| e.to_string())?;
        if existed {
            st.reload_snapshot().await.map_err(|e| e.to_string())?;
        }
        Ok::<_, String>(existed)
    })
    .await??;
    if !existed {
        return Err(Refusal::not_found(format!(
            "no price row {id} — ids are listed by lmgw__prices"
        )));
    }
    Ok(dto::IdAck::new(id))
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
fn usage_cell_view(c: &store::UsageCell, currency: &str) -> UsageCellView {
    let avg_ttfb_ms = (c.ttfb_count > 0).then(|| c.ttfb_sum as f64 / c.ttfb_count as f64);
    let avg_total_ms = (c.total_count > 0).then(|| c.total_sum as f64 / c.total_count as f64);
    let decode_tok_s = (c.decode_ms > 0.0).then(|| c.decode_tokens as f64 / (c.decode_ms / 1000.0));
    UsageCellView {
        series: (!c.series.is_empty()).then(|| c.series.clone()),
        requests: c.requests,
        tokens_in: c.tokens_in,
        tokens_out: c.tokens_out,
        tokens_cached: c.tokens_cached,
        tokens_cache_write: c.tokens_cache_write,
        tokens_reasoning: c.tokens_reasoning,
        audio_in_ms: c.audio_in_ms,
        chars_in: c.chars_in,
        images_out: c.images_out,
        cost: money_with_unpriced(c, currency),
        cost_micro: c.cost_micro,
        unpriced_requests: c.cost_unknown_requests,
        unpriced_tokens: c.cost_unknown_tokens,
        unpriced_audio_in_ms: c.cost_unknown_audio_in_ms,
        unpriced_chars_in: c.cost_unknown_chars_in,
        unpriced_images_out: c.cost_unknown_images_out,
        errors: c.errors,
        refusals: c.refusals,
        avg_ttfb_ms,
        avg_total_ms,
        decode_tok_s,
    }
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
/// `from`: an ISO date, or an `<N>d` shorthand meaning the last N whole days
/// ending with `to` (`7d`, `30d`, …: `to` and the N-1 days before it); omitted
/// means `30d`. The range covers whole days, `from` 00:00 through the end of
/// `to`, both inclusive.
fn parse_usage_range(from: Option<&str>, to: Option<&str>) -> Result<(String, String), String> {
    use chrono::{Days, Duration, NaiveDate, Utc};

    let today = Utc::now().date_naive();
    let to_date = match to.map(str::trim).filter(|s| !s.is_empty()) {
        None => today,
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| format!("invalid 'to' date '{s}' (expected YYYY-MM-DD)"))?,
    };
    let last_days = |n: u64| -> Result<NaiveDate, String> {
        if n == 0 {
            return Err("'from' as '<N>d' needs N of at least 1".to_string());
        }
        to_date
            .checked_sub_days(Days::new(n - 1))
            .ok_or_else(|| format!("'from' '{n}d' reaches before the calendar starts"))
    };
    let from_date = match from.map(str::trim).filter(|s| !s.is_empty()) {
        None => last_days(30)?,
        Some(s) => match s.strip_suffix('d').and_then(|n| n.parse::<u64>().ok()) {
            Some(n) => last_days(n)?,
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

/// An hour key (`2026-10-12T00`) as the ISO 8601 UTC instant it names
/// (`2026-10-12T00:00:00Z`).
fn hour_key_instant(key: &str) -> String {
    format!("{key}:00:00Z")
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
) -> Result<Usage, String> {
    let (from_key, to_key) = parse_usage_range(from, to)?;
    let group_name = group_by
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("alias");
    let group = match group_name {
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

    let alias = alias
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let filter = UsageFilter {
        from: from_key.clone(),
        to: to_key.clone(),
        alias: alias.clone(),
        key_id: None,
        class: class.clone(),
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

    Ok(Usage {
        from: hour_key_instant(&from_key),
        to: hour_key_instant(&to_key),
        class,
        alias,
        timezone: "UTC".to_string(),
        totals: usage_cell_view(&totals, &currency),
        breakdown: breakdown
            .iter()
            .map(|c| usage_cell_view(c, &currency))
            .collect(),
        currency,
        units_since,
        group_by: group_name.to_string(),
        hint: "cost is money in every billable unit (tokens, audio minutes, characters, \
                 images, request fees) and states its unpriced remainder inline, with the \
                 tokens and quantities that remainder holds — a total with unpriced requests \
                 folded in understates what was actually spent, not the whole story. \
                 audio_in_ms, chars_in and images_out are measured quantities (input audio, \
                 input characters, generated images), recorded since units_since; before it \
                 they read 0 because nothing was recorded. Sync catalog prices with \
                 lmgw__prices_sync, or check lmgw__prices for models with no price on file \
                 at all."
            .to_string(),
    })
}
