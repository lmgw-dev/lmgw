//! Upstream model catalogs: fetch `GET <base>/models` per protocol, cached a
//! few minutes. Used by `/v1/models` aggregation for expose-all upstreams,
//! the alias form's model picker, and the wiring page.
//!
//! Field derivation follows
//! `docs/design/2026-09-17-model-capabilities-design.md` §4: a
//! field the catalog does not publish is absent (`None`), never defaulted.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::config::{upstream_scope_key, PriceRow, PriceScope, PriceUnit, Protocol, Upstream};
use crate::pricing::{per_token_to_per_mtok, PriceSource, Prices};
use crate::state::SharedState;
use crate::store;

const TTL: Duration = Duration::from_secs(300);

/// How long a *failed* catalog fetch is remembered (§5.4).
///
/// Much shorter than [`TTL`], because a failure is usually temporary and a
/// catalog is how `/v1/models` and the Anthropic `max_tokens` default learn
/// what a provider can do. But not zero: without it, every request on an
/// upstream whose `/models` is unreachable pays the full fetch timeout again,
/// turning one provider's outage into per-request latency on a path the client
/// never asked to be on.
const ERR_TTL: Duration = Duration::from_secs(60);

/// Canonical reasoning-effort vocabulary, least to most.
pub(crate) const CANONICAL_LEVELS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// One model entry from an upstream's catalog.
#[derive(Debug, Clone, Default)]
pub struct ModelInfo {
    pub id: String,
    /// Max context window in tokens, when the upstream advertises it.
    pub context_length: Option<u64>,
    /// The catalog's prices, when the upstream advertises a prompt and a
    /// completion price (Kilo/OpenRouter shape).
    pub pricing: Option<Pricing>,
    /// Unix seconds. OpenAI-protocol `created`; Anthropic `created_at`
    /// (RFC3339) parsed to unix seconds; Gemini never publishes one.
    pub created: Option<i64>,
    /// One response's generation cap: OpenAI-protocol
    /// `top_provider.max_completion_tokens`, Gemini `outputTokenLimit`,
    /// Anthropic `max_tokens`. Never derived from the context size.
    pub max_output_tokens: Option<u64>,
    /// OpenAI-protocol `architecture.input_modalities`; Anthropic derived
    /// from `capabilities.image_input`; Gemini never publishes this.
    pub input_modalities: Option<Vec<String>>,
    /// OpenAI-protocol `architecture.output_modalities`; Anthropic `[text]`
    /// when a `capabilities` object exists; Gemini never publishes this.
    pub output_modalities: Option<Vec<String>>,
    /// `chat` | `embedding` | `image_generation`, only when the catalog
    /// states it (§4) — `None` for an OpenAI-protocol entry with no output
    /// modalities, whose task `capabilities::for_catalog` reads off its name.
    pub task: Option<String>,
    /// Reasoning / effort support, when the catalog states anything about it.
    pub reasoning: Option<CatalogReasoning>,
    /// Whether the model accepts tool/function-calling.
    pub tools: Option<bool>,
    /// Structured-output support (JSON schema / JSON mode).
    pub structured_output: Option<CatalogStructured>,
}

/// A catalog's statement about reasoning/effort support for one model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogReasoning {
    /// `"levels"` | `"toggle"` | `"fixed"`.
    pub kind: String,
    /// The default on/off state, when a source states it.
    pub enabled: Option<bool>,
    /// Accepted effort values, least → most, canonical order. Only
    /// meaningful for `kind == "levels"`.
    pub levels: Vec<String>,
    /// Whether the request can switch thinking off entirely, when a source
    /// states it.
    pub can_disable: Option<bool>,
}

/// A catalog's statement about structured-output support for one model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogStructured {
    /// `response_format: {type: "json_schema", ...}` / OpenAI `structured_outputs`.
    pub json_schema: Option<bool>,
    /// `response_format: {type: "json_object"}`.
    pub json_object: Option<bool>,
}

/// Prices in USD, as advertised by an upstream catalog's pricing object
/// (the OpenRouter shape, which Kilo follows).
/// Kept as the upstream's verbatim decimal strings (e.g. `"0.000003"`) so we
/// neither lose precision nor reformat what the provider published.
///
/// Field meanings, from OpenRouter's models API: the Pricing Object at
/// <https://openrouter.ai/docs/guides/overview/models> ("All pricing values are
/// in USD per token/request/unit. A value of "0" indicates the feature is
/// free.") and the `PublicPricing` schema at
/// <https://openrouter.ai/docs/api/api-reference/models/get-models>. What the
/// price sync makes of each is billable-units design §6.
#[derive(Debug, Clone, Default)]
pub struct Pricing {
    /// USD per input (prompt) token.
    pub prompt: String,
    /// USD per output (completion) token.
    pub completion: String,
    /// USD per cached input token read (`input_cache_read`), when published.
    /// A replacement rate for those tokens, not a surcharge.
    pub cache_read: Option<String>,
    /// USD per cached input token written (`input_cache_write`), when published.
    pub cache_write: Option<String>,
    /// USD per API request (`request`: "Fixed cost per API request"), when
    /// published. Synced as a `per_request` row, but only beside a usable
    /// token row (§6).
    pub request: Option<String>,
    /// USD per **input** image (`image`: "Cost per image input"), when
    /// published. Not synced in v1 (§6, Q5): `per_image` counts *generated*
    /// images, and the docs do not say whether this adds to the image's prompt
    /// tokens or replaces them.
    pub image: Option<String>,
    /// USD per web search (`web_search`: "Cost per web search operation"),
    /// when published. Not synced in v1: `per_web_search` is reserved, and no
    /// route can carry a hosted search (§4.6).
    pub web_search: Option<String>,
    /// Every other field of the pricing object the catalog published with a
    /// value other than zero, by name, sorted: `internal_reasoning`,
    /// `image_output`, `audio`, `input_cache_write_1h`, `discount`,
    /// `overrides`, and whatever a provider adds next. lmgw prices none of
    /// them; they are named so a sync can count them (`not_synced`) instead of
    /// dropping them unseen.
    pub other: Vec<String>,
}

/// A cached catalog fetch — the models, or why there are none. Failures are
/// cached too, for [`ERR_TTL`] rather than [`TTL`].
type CacheEntry = (Instant, Result<Vec<ModelInfo>, String>);

#[derive(Default)]
pub struct CatalogCache {
    inner: RwLock<HashMap<i64, CacheEntry>>,
    /// Upstreams whose catalog a background read is fetching now
    /// ([`cached_only`]): one read each, however many ask.
    refreshing: std::sync::Mutex<HashSet<i64>>,
}

tokio::task_local! {
    /// Set inside [`cached_only`]: a catalog read is served from the cache
    /// alone, and a read it could not serve is recorded here.
    static CACHED_ONLY: Arc<AtomicBool>;
}

/// Run `f` with every upstream catalog read served from the cache alone
/// (voice-audio-input review V9): what is cached answers, a stale entry
/// too; what is not cached is read in the background, and `f` sees that
/// catalog as not read yet. The second value says whether that happened, so
/// a caller can say "not known yet" rather than guess. For a page that must
/// not wait on a provider: no time limit of its own, nothing dropped — the
/// background read is the normal one, and its answer is cached for the next
/// look.
pub async fn cached_only<F: Future>(f: F) -> (F::Output, bool) {
    let missed = Arc::new(AtomicBool::new(false));
    let out = CACHED_ONLY.scope(missed.clone(), f).await;
    (out, missed.load(Ordering::Relaxed))
}

/// Read `u`'s catalog into the cache behind the caller's back, once at a
/// time per upstream ([`cached_only`]).
fn refresh_in_background(state: &SharedState, u: &Upstream) {
    let fresh = state
        .catalog
        .refreshing
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(u.id);
    if !fresh {
        return;
    }
    let (state, u) = (state.clone(), u.clone());
    tokio::spawn(async move {
        let fetched = fetch_models(&state, &u).await;
        state
            .catalog
            .inner
            .write()
            .await
            .insert(u.id, (Instant::now(), fetched));
        state
            .catalog
            .refreshing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&u.id);
    });
}

impl CatalogCache {
    pub async fn invalidate(&self, upstream_id: i64) {
        self.inner.write().await.remove(&upstream_id);
    }
}

/// Model catalog served by an upstream, through the cache.
pub async fn upstream_models(state: &SharedState, u: &Upstream) -> Result<Vec<ModelInfo>, String> {
    let cached_only = CACHED_ONLY.try_with(Arc::clone).ok();
    if let Some((at, cached)) = state.catalog.inner.read().await.get(&u.id) {
        let ttl = if cached.is_ok() { TTL } else { ERR_TTL };
        if at.elapsed() < ttl {
            return cached.clone();
        }
        // Inside `cached_only`: the stale answer now, a fresh one later.
        if cached_only.is_some() {
            refresh_in_background(state, u);
            return cached.clone();
        }
    }
    if let Some(missed) = cached_only {
        missed.store(true, Ordering::Relaxed);
        refresh_in_background(state, u);
        return Err(format!("{}: models list not read yet", u.name));
    }
    let fetched = fetch_models(state, u).await;
    state
        .catalog
        .inner
        .write()
        .await
        .insert(u.id, (Instant::now(), fetched.clone()));
    fetched
}

// ---------------------------------------------------------------------------
// Price sync (usage-analytics design §2.2; billable-units design §6)
// ---------------------------------------------------------------------------
//
// Catalog rows have always been parsed (`pricing` on `ModelInfo`, above) and
// republished on `/v1/models`; nothing has ever multiplied one by a token
// count. This is the half that turns "we know the number" into "the number
// is on file where pricing (§2.3) can find it".

/// One price row [`sync_prices`] would write.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceUpdate {
    pub scope_kind: PriceScope,
    pub scope_key: String,
    /// `per_mtok`, with its rates in `prices`, or `per_request`, with its
    /// rate in `price` (billable-units §6).
    pub unit: PriceUnit,
    /// The token rates of a `per_mtok` row. On any other unit only the
    /// `source` is set and every rate stays `None`: the table's CHECK keeps
    /// the two shapes apart.
    pub prices: Prices,
    /// The rate of a non-token row, per its unit's scale; `None` on a
    /// `per_mtok` row.
    pub price: Option<f64>,
    /// Write only over an existing catalog row of this scope and unit, never
    /// create one. That is a fee the catalog now states as `"0"` (§6): a new
    /// zero row would change nothing, and an old non-zero one left in place
    /// would keep charging a fee the provider dropped.
    pub update_only: bool,
}

/// The pure conversion behind [`sync_prices`]: one upstream's catalog listing,
/// turned into the price rows it implies. No IO, so the rules this stands on —
/// a model with no usable price upserts nothing, never a zero (§2.3); a fee
/// goes on file only beside a usable token row (billable-units §6) — are
/// testable without a DB or an upstream fetch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PriceUpdates {
    pub rows: Vec<PriceUpdate>,
    /// Models the catalog listed with no usable price (none advertised, or
    /// numbers that failed to parse). These upsert nothing — the count is how
    /// the caller reports that instead of staying silent about it.
    pub unpriced_models: usize,
    /// Per pricing field, the models priced here that publish it and had it
    /// go into no row ([`UpstreamPriceSync::not_synced`]).
    pub not_synced: BTreeMap<String, usize>,
    /// The scopes of every model priced here whose pricing object has no
    /// `request` field at all. A catalog `per_request` row on file for one of
    /// them is a fee the catalog no longer states, and goes
    /// ([`to_remove`](Self::to_remove)).
    pub no_fee: Vec<(PriceScope, String)>,
}

impl PriceUpdates {
    /// The catalog `per_request` rows on file that the catalog no longer
    /// backs: their model's token row is synced, and it publishes no
    /// `request` at all. Catalog rows mirror the catalog, so a fee the
    /// provider dropped must not keep charging; a later sync recreates the
    /// row if the fee comes back. A manual row is the owner's and never one
    /// of these.
    ///
    /// `per_request` only. A stale catalog `per_mtok` row (a model that left
    /// the catalog, or stopped publishing a usable price) stays on file as it
    /// always has; that is older behaviour, unchanged here.
    pub fn to_remove<'a>(&'a self, on_file: &'a [PriceRow]) -> impl Iterator<Item = &'a PriceRow> {
        on_file.iter().filter(move |p| {
            p.source == PriceSource::Catalog
                && p.unit == PriceUnit::PerRequest
                && self
                    .no_fee
                    .iter()
                    .any(|(kind, key)| p.scope_kind == *kind && p.scope_key == *key)
        })
    }

    /// The rows to write, given the rows on file: all of them, except an
    /// [`update_only`](PriceUpdate::update_only) row with no catalog row of
    /// its scope and unit to update.
    ///
    /// The match is exact, as the upsert's conflict key
    /// `(scope_kind, scope_key, source, unit)` is: a row this skips is one
    /// the upsert would have created rather than updated.
    pub fn to_write<'a>(
        &'a self,
        on_file: &'a [PriceRow],
    ) -> impl Iterator<Item = &'a PriceUpdate> + 'a {
        self.rows.iter().filter(move |u| {
            !u.update_only
                || on_file.iter().any(|p| {
                    p.source == PriceSource::Catalog
                        && p.unit == u.unit
                        && p.scope_kind == u.scope_kind
                        && p.scope_key == u.scope_key
                })
        })
    }
}

/// A catalog value as a price: a finite, non-negative number, or `None`.
///
/// `f64::from_str` accepts "inf" and "NaN". An infinite price times a token
/// count saturates to `i64::MAX` in `cost_micro`, and from there into the
/// rollup's running sum and the budget gate — one malformed catalog entry
/// would refuse every request on the gateway for the rest of the period.
/// A price that is not a finite, non-negative number is not a price.
fn sane(v: &str) -> Option<f64> {
    v.parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0)
}

/// Whether a published value says something costs money, or might: anything
/// but an empty string and a zero. A field published as `"0"` is free
/// ("A value of "0" indicates the feature is free",
/// <https://openrouter.ai/docs/guides/overview/models>), so leaving it out of
/// every row loses nothing and is not worth reporting. A value that is not a
/// price at all (`"-1"`, garbage) does count: what it would have charged is
/// unknown.
fn charges(v: &str) -> bool {
    !v.is_empty() && v.parse::<f64>() != Ok(0.0)
}

/// [`charges`] for a raw pricing-object value of any JSON type: a non-zero
/// number or string, a non-empty `overrides` array or object, `true`.
fn value_charges(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => charges(s),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Convert a catalog's per-token pricing into a per-Mtok sheet, or `None` when
/// either number fails to parse. A model that publishes garbage pricing is
/// treated exactly like one that publishes none — §2.3 does not carve out an
/// exception for "the provider's own data looked wrong".
fn catalog_prices(p: &Pricing) -> Option<Prices> {
    let prompt = sane(&p.prompt)?;
    let completion = sane(&p.completion)?;
    // Cache rates are replacement rates for the cached subset of `prompt`
    // (OpenRouter models API, Pricing Object: "input_cache_read: Cost per
    // cached input token read", per token like `prompt`). Absent or
    // unparseable is `None`, which bills at the input rate.
    //
    // A "0" here with a non-zero prompt price is also `None`. OpenRouter's
    // docs say "A value of "0" indicates the feature is free", but that is a
    // blanket sentence for the whole pricing object, not a statement about
    // cache fields, and a placeholder zero would make cached tokens free:
    // wrong downward, the worst failure here. Falling back to the input
    // rate errs upward and is visible.
    // (https://openrouter.ai/docs/guides/overview/models)
    // A zero on a model whose prompt is zero too stays a real zero.
    let cache = |v: &Option<String>| {
        let n = sane(v.as_deref()?)?;
        (n > 0.0 || prompt == 0.0).then(|| per_token_to_per_mtok(n))
    };
    Some(Prices {
        price_in: Some(per_token_to_per_mtok(prompt)),
        price_out: Some(per_token_to_per_mtok(completion)),
        price_cache_read: cache(&p.cache_read),
        price_cache_write: cache(&p.cache_write),
        source: PriceSource::Catalog,
    })
}

/// Turn one upstream's catalog listing into the price rows it implies
/// (billable-units design §6):
///
/// | Field | Row |
/// |---|---|
/// | `prompt`, `completion`, `input_cache_read`, `input_cache_write` | `per_mtok` (`catalog_prices`) |
/// | `request` | `per_request`, beside a usable token row only; `"0"` updates an existing catalog row and creates none; absent removes it |
/// | `image`, `web_search`, anything else | none; counted in `not_synced` unless published as zero |
///
/// `aliases` is every enabled alias pointing at this upstream, as
/// `(upstream_model_id, alias_name)` pairs. A model with one or more matching
/// aliases is scoped to each alias by name; a model with none is scoped to
/// [`upstream_scope_key`] instead, which is what an `expose_all` passthrough
/// request resolves to. Every row of one model has the same scopes.
pub fn price_updates_for_upstream(
    upstream_id: i64,
    models: &[ModelInfo],
    aliases: &[(&str, &str)],
) -> PriceUpdates {
    let mut out = PriceUpdates::default();
    for m in models {
        let Some((pricing, prices)) = m
            .pricing
            .as_ref()
            .and_then(|p| Some((p, catalog_prices(p)?)))
        else {
            out.unpriced_models += 1;
            continue;
        };
        let matching: Vec<&str> = aliases
            .iter()
            .filter(|(model_id, _)| *model_id == m.id)
            .map(|(_, alias)| *alias)
            .collect();
        let scopes: Vec<(PriceScope, String)> = if matching.is_empty() {
            vec![(
                PriceScope::UpstreamModel,
                upstream_scope_key(upstream_id, &m.id),
            )]
        } else {
            matching
                .into_iter()
                .map(|alias| (PriceScope::Alias, alias.to_string()))
                .collect()
        };
        let mut push = |unit, prices, price, update_only| {
            for (scope_kind, scope_key) in &scopes {
                out.rows.push(PriceUpdate {
                    scope_kind: *scope_kind,
                    scope_key: scope_key.clone(),
                    unit,
                    prices,
                    price,
                    update_only,
                });
            }
        };
        push(PriceUnit::PerMtok, prices, None, false);

        // The fee rides on the token row, never alone: a model whose token
        // price did not parse is unpriced and on the worklist, and a fee
        // written without it would price that model at the fee alone — a
        // silent number, wrong downward. A request-only sheet (a per-clip
        // music model) is legitimate, but only as a manual row.
        let mut skipped = Vec::new();
        match pricing.request.as_deref().filter(|v| !v.is_empty()) {
            Some(v) => match sane(v) {
                Some(fee) => push(
                    PriceUnit::PerRequest,
                    Prices {
                        source: PriceSource::Catalog,
                        ..Default::default()
                    },
                    Some(fee),
                    fee == 0.0,
                ),
                None => skipped.push("request"),
            },
            // Published in a shape no price comes in: named in `other`, and
            // counted below. Not "absent", so a fee on file stays.
            None if pricing.other.iter().any(|f| f == "request") => {}
            // No fee published at all: a catalog fee on file is stale.
            None => out.no_fee.extend(scopes.iter().cloned()),
        }
        // A cache rate that is published but did not become one: falling back
        // to the input rate errs upward, but it is still a published number
        // lmgw did not apply. A "0" on a paid model is not advertised
        // (`catalog_prices`) and is not counted.
        for (name, published, synced) in [
            (
                "input_cache_read",
                &pricing.cache_read,
                prices.price_cache_read,
            ),
            (
                "input_cache_write",
                &pricing.cache_write,
                prices.price_cache_write,
            ),
        ] {
            if published.as_deref().is_some_and(charges) && synced.is_none() {
                skipped.push(name);
            }
        }
        // Not synced in v1 (§6, Q5): `image` is per *input* image and so not
        // `per_image`, and `per_web_search` is reserved.
        for (name, published) in [
            ("image", &pricing.image),
            ("web_search", &pricing.web_search),
        ] {
            if published.as_deref().is_some_and(charges) {
                skipped.push(name);
            }
        }
        skipped.extend(pricing.other.iter().map(String::as_str));
        for name in skipped {
            *out.not_synced.entry(name.to_string()).or_default() += 1;
        }
    }
    out
}

/// What a price sync did to one upstream.
#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct UpstreamPriceSync {
    pub upstream_id: i64,
    pub upstream: String,
    /// Rows created or updated: one `per_mtok` row per priced model and
    /// scope, and a `per_request` row beside it where the catalog states a
    /// fee.
    pub rows_written: usize,
    /// Catalog `per_request` rows deleted because their model, priced here,
    /// no longer publishes a `request` fee at all.
    pub rows_removed: usize,
    pub unpriced_models: usize,
    /// Per field of the catalog's pricing object, how many models had their
    /// token prices synced here and publish that field with a value other
    /// than zero that went into no row. Left out on purpose: `image` (a
    /// price per *input* image, not a generated one),
    /// `web_search` (no route carries a hosted search), and the fields lmgw
    /// does not model, such as `internal_reasoning`, `image_output`,
    /// `overrides` or `discount`. Also a `request` fee or cache rate that is
    /// not a price (`"-1"`, unparseable). A model counted in
    /// `unpriced_models` had nothing synced and is not counted here again.
    /// Empty when nothing published was left out.
    // Billable-units design §6.
    pub not_synced: BTreeMap<String, usize>,
    /// The catalog fetch failed; nothing was synced for this upstream. Not
    /// fatal to the run as a whole — one unreachable provider should not stop
    /// every other upstream's prices from refreshing.
    pub error: Option<String>,
}

/// Summary of one price sync run.
#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct PriceSyncSummary {
    pub rows_written: usize,
    /// The upstreams' `rows_removed`, summed.
    pub rows_removed: usize,
    pub unpriced_models: usize,
    /// The upstreams' `not_synced`, summed per field.
    pub not_synced: BTreeMap<String, usize>,
    pub upstreams: Vec<UpstreamPriceSync>,
}

/// Sync catalog-advertised prices into the `prices` table for every enabled
/// upstream, then refresh the snapshot so the new numbers price the very next
/// request rather than waiting for a restart. Which catalog fields become
/// which rows is [`price_updates_for_upstream`].
///
/// Local containers (`llama_server`, `audio_cpp`, `sd_cpp`) are skipped: a local route
/// is always `free_local` regardless of any price row — `Snapshot::prices_for`
/// decides that from the upstream kind before it ever looks at the prices
/// table — so there is nothing for a price row to do there.
///
/// Manual rows are never touched by this: `store::upsert_price` is keyed on
/// `(scope_kind, scope_key, source, unit)` and every row written here carries
/// `source = catalog`, so a catalog refresh can only ever create or update
/// another catalog row, never a `manual` one. The one delete, of a fee the
/// catalog stopped publishing, picks catalog rows only.
pub async fn sync_prices(state: &SharedState) -> Result<PriceSyncSummary, String> {
    let snap = state.snapshot();
    let mut upstreams: Vec<&Upstream> = snap.upstreams.values().filter(|u| u.enabled).collect();
    upstreams.sort_by_key(|u| u.id);

    let mut summary = PriceSyncSummary::default();
    for u in upstreams {
        if snap.is_local_upstream(u.id) {
            continue;
        }
        let mut row = UpstreamPriceSync {
            upstream_id: u.id,
            upstream: u.name.clone(),
            ..Default::default()
        };
        match upstream_models(state, u).await {
            Ok(models) => {
                let aliases: Vec<(&str, &str)> = snap
                    .aliases
                    .values()
                    .filter(|a| a.enabled && a.upstream_id == u.id)
                    .map(|a| (a.upstream_model_id.as_str(), a.alias.as_str()))
                    .collect();
                let updates = price_updates_for_upstream(u.id, &models, &aliases);
                for pu in updates.to_write(&snap.prices) {
                    store::upsert_price(
                        &state.db,
                        pu.scope_kind,
                        &pu.scope_key,
                        pu.unit,
                        &pu.prices,
                        pu.price,
                        None,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    row.rows_written += 1;
                }
                for stale in updates.to_remove(&snap.prices) {
                    store::delete_price(&state.db, stale.id)
                        .await
                        .map_err(|e| e.to_string())?;
                    row.rows_removed += 1;
                }
                row.unpriced_models = updates.unpriced_models;
                row.not_synced = updates.not_synced;
            }
            Err(e) => row.error = Some(e),
        }
        summary.rows_written += row.rows_written;
        summary.rows_removed += row.rows_removed;
        summary.unpriced_models += row.unpriced_models;
        for (field, models) in &row.not_synced {
            *summary.not_synced.entry(field.clone()).or_default() += models;
        }
        summary.upstreams.push(row);
    }

    state.reload_snapshot().await.map_err(|e| e.to_string())?;
    Ok(summary)
}

/// One protocol-appropriate models-list call (no cache).
pub async fn fetch_models(state: &SharedState, u: &Upstream) -> Result<Vec<ModelInfo>, String> {
    let base = u.base();
    let key = u.api_key.as_deref().filter(|k| !k.is_empty());
    let rb = match u.protocol {
        // llama-server lists its models the OpenAI way.
        Protocol::Openai | Protocol::LlamaCpp => {
            let mut rb = state.http.get(format!("{base}/models"));
            if let Some(k) = key {
                rb = rb.bearer_auth(k);
            }
            rb
        }
        Protocol::Anthropic => {
            let b = base.trim_end_matches("/v1");
            let mut rb = state
                .http
                .get(format!("{b}/v1/models?limit=1000"))
                .header("anthropic-version", "2023-06-01");
            if let Some(k) = key {
                rb = rb.header("x-api-key", k);
            }
            rb
        }
        Protocol::Gemini => {
            let b = base.trim_end_matches("/v1beta");
            let mut rb = state.http.get(format!("{b}/v1beta/models"));
            if let Some(k) = key {
                rb = rb.header("x-goog-api-key", k);
            }
            rb
        }
    };
    // "unreachable" leads when nothing answers at all, so the dashboard can
    // say that in one word instead of reqwest's "error sending request for
    // url (…)"; the detail stays behind it for the tooltip and the logs.
    let resp = rb
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| {
            if e.is_connect() {
                format!("unreachable ({e})")
            } else {
                e.to_string()
            }
        })?;
    if !resp.status().is_success() {
        return Err(format!(
            "{}: models list failed ({})",
            u.name,
            resp.status()
        ));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let mut out: Vec<ModelInfo> = match u.protocol {
        // OpenAI (and llama-server): {data: [{id, context_window?, context_length?, ...}]}
        Protocol::Openai | Protocol::LlamaCpp => body["data"]
            .as_array()
            .map(|arr| arr.iter().filter_map(parse_openai_entry).collect())
            .unwrap_or_default(),
        // Anthropic: {data: [{id, max_input_tokens?, max_tokens?, capabilities?, ...}]}
        Protocol::Anthropic => body["data"]
            .as_array()
            .map(|arr| arr.iter().filter_map(parse_anthropic_entry).collect())
            .unwrap_or_default(),
        // Gemini: {models: [{name: "models/gemini-…", inputTokenLimit?, ...}]}
        Protocol::Gemini => body["models"]
            .as_array()
            .map(|arr| arr.iter().filter_map(parse_gemini_entry).collect())
            .unwrap_or_default(),
    };
    if out.is_empty() {
        let preview = body.to_string();
        let truncated: String = preview.chars().take(400).collect();
        tracing::warn!(
            upstream = %u.name,
            "models list returned 0 models; response body: {truncated}"
        );
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    Ok(out)
}

/// Parse one entry of an OpenAI-protocol (Kilo/OpenRouter-shaped) catalog.
pub fn parse_openai_entry(m: &Value) -> Option<ModelInfo> {
    let id = m["id"].as_str().map(String::from)?;
    Some(ModelInfo {
        id,
        context_length: parse_context_length_openai(m),
        pricing: parse_pricing_openai(m),
        created: m["created"].as_i64(),
        max_output_tokens: m["top_provider"]["max_completion_tokens"].as_u64(),
        input_modalities: parse_modalities_openai(m, "input_modalities"),
        output_modalities: parse_modalities_openai(m, "output_modalities"),
        task: parse_task_openai(m),
        reasoning: parse_reasoning_openai(m),
        tools: parse_tools_openai(m),
        structured_output: parse_structured_output_openai(m),
    })
}

/// Parse one entry of an Anthropic `models.list` catalog.
pub fn parse_anthropic_entry(m: &Value) -> Option<ModelInfo> {
    let id = m["id"].as_str().map(String::from)?;
    Some(ModelInfo {
        id,
        context_length: parse_context_length_anthropic(m),
        // Not part of the documented Anthropic `models.list` shape, but kept
        // for upstreams that happen to serve the Kilo/OpenRouter pricing
        // block on an Anthropic-protocol route (unchanged prior behaviour).
        pricing: parse_pricing_openai(m),
        created: parse_created_anthropic(m),
        max_output_tokens: m["max_tokens"].as_u64(),
        input_modalities: parse_input_modalities_anthropic(m),
        output_modalities: parse_output_modalities_anthropic(m),
        // Anthropic's models.list is chat models only.
        task: Some("chat".to_string()),
        reasoning: parse_reasoning_anthropic(m),
        // Tool calling is a base Messages API feature, independent of the
        // (optional) `capabilities` metadata.
        tools: Some(true),
        // Anthropic egress drops `response_format` today; the catalog says
        // nothing about structured output either.
        structured_output: None,
    })
}

/// Parse one entry of a Gemini `models.list` catalog.
pub fn parse_gemini_entry(m: &Value) -> Option<ModelInfo> {
    let id = m["name"]
        .as_str()
        .map(|n| n.trim_start_matches("models/").to_string())?;
    Some(ModelInfo {
        id,
        context_length: m["inputTokenLimit"].as_u64(),
        // Gemini's models API does not advertise pricing.
        pricing: None,
        // Gemini's models.list has no timestamp field.
        created: None,
        max_output_tokens: m["outputTokenLimit"].as_u64(),
        // Gemini's catalog says nothing about modalities; model-name hints
        // (-tts, -image, native-audio) are not promoted to facts here.
        input_modalities: None,
        output_modalities: None,
        task: Some(parse_task_gemini(m)),
        reasoning: parse_reasoning_gemini(m),
        tools: parse_tools_gemini(m),
        // Gemini egress drops `response_format` today.
        structured_output: None,
    })
}

/// Extract context window from an OpenAI-shaped model object.
/// Checks several field names used by different providers:
///
/// - `n_ctx`          — llama.cpp runtime context (respects --ctx-size)
/// - `context_window` — Anthropic, Kilo, some OpenAI-compat providers
/// - `context_length` — Together AI, Groq, others
///
/// Deliberately excludes `meta.n_ctx_train` (training context; ignores --ctx-size).
fn parse_context_length_openai(m: &Value) -> Option<u64> {
    m["n_ctx"]
        .as_u64()
        .or_else(|| m["context_window"].as_u64())
        .or_else(|| m["context_length"].as_u64())
}

/// Extract context window from an Anthropic `models.list` entry:
/// `max_input_tokens` (the documented field) first, `context_window` (kept
/// for upstreams that serve the older/Kilo shape on an Anthropic route) next.
fn parse_context_length_anthropic(m: &Value) -> Option<u64> {
    m["max_input_tokens"]
        .as_u64()
        .or_else(|| m["context_window"].as_u64())
}

/// The pricing-object fields [`Pricing`] reads by name. Everything else lands
/// in [`Pricing::other`].
const PRICING_FIELDS: [&str; 7] = [
    "prompt",
    "completion",
    "input_cache_read",
    "input_cache_write",
    "request",
    "image",
    "web_search",
];

/// Extract pricing from an OpenAI-shaped model object.
/// Mirrors the Kilo/OpenRouter shape: `pricing: { prompt, completion, … }`,
/// USD per token (per request, per image, per search for the fields named so;
/// see [`Pricing`]). `prompt` and `completion` must both be present (number
/// or numeric string) or we report no pricing rather than a half-known price.
fn parse_pricing_openai(m: &Value) -> Option<Pricing> {
    let p = &m["pricing"];
    let prompt = price_field(&p["prompt"])?;
    let completion = price_field(&p["completion"])?;
    // A named field of a type no price comes in (an object where a string
    // belongs) goes to `other` with the unknown ones, so it is counted rather
    // than read as "not published".
    let mut other: Vec<String> = p
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, v)| {
            value_charges(v) && (!PRICING_FIELDS.contains(&k.as_str()) || price_field(v).is_none())
        })
        .map(|(k, _)| k.clone())
        .collect();
    other.sort_unstable();
    Some(Pricing {
        prompt,
        completion,
        // Optional: a model that publishes no cache price keeps `None`.
        cache_read: price_field(&p["input_cache_read"]),
        cache_write: price_field(&p["input_cache_write"]),
        request: price_field(&p["request"]),
        image: price_field(&p["image"]),
        web_search: price_field(&p["web_search"]),
        other,
    })
}

/// One price value, accepting either a JSON string (`"0.000003"`) or a bare
/// number, normalised to a verbatim decimal string.
fn price_field(v: &Value) -> Option<String> {
    v.as_str()
        .map(String::from)
        .or_else(|| v.is_number().then(|| v.to_string()))
}

/// `architecture.input_modalities` / `architecture.output_modalities`.
fn parse_modalities_openai(m: &Value, key: &str) -> Option<Vec<String>> {
    m["architecture"][key].as_array().map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    })
}

/// `embedding` when `output_modalities == [embedding]`; `image_generation`
/// when the model's outputs are images and not text (image-generation design
/// §5); else `chat` — and `None` when the entry states no output modalities
/// at all (a stock `api.openai.com` list): the catalog said nothing, and
/// `capabilities::for_catalog` goes by the name then
/// (`capabilities::task::by_name`, realtime live run 3 D1).
///
/// "Images and not text" rather than "contains image": `task` is what picks a
/// model's `endpoints`, and a chat model that can also draw (`[text, image]`,
/// which is how the Gemini image models appear in an OpenRouter-shaped
/// catalog) would lose `/v1/chat/completions` — the route it is actually
/// used through — to gain an images route its provider may not even serve.
fn parse_task_openai(m: &Value) -> Option<String> {
    let out: Vec<&str> = m["architecture"]["output_modalities"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if out.len() == 1 && out[0] == "embedding" {
        return Some("embedding".to_string());
    }
    if out.contains(&"image") && !out.contains(&"text") {
        return Some("image_generation".to_string());
    }
    Some("chat".to_string())
}

/// `embedContent` in `supportedGenerationMethods` ⇒ `embedding`;
/// `generateContent` ⇒ `chat`; neither (e.g. `bidiGenerateContent`-only) ⇒
/// `chat` (lmgw has no route to it; that fact belongs in a note, not here).
fn parse_task_gemini(m: &Value) -> String {
    let has_embed = m["supportedGenerationMethods"]
        .as_array()
        .map(|arr| arr.iter().any(|v| v.as_str() == Some("embedContent")))
        .unwrap_or(false);
    if has_embed {
        "embedding".to_string()
    } else {
        "chat".to_string()
    }
}

/// `supported_parameters ∋ reasoning | reasoning_effort` ⇒ supported.
/// `opencode.variants` keys that are effort levels ⇒ `levels`, `can_disable =
/// Some(true)` when the variants had `none`; supported without such variants
/// ⇒ `toggle`. No `enabled`. Not supported ⇒ `None` (absent — the catalog does
/// not say the model cannot reason, only that no parameter controls it).
fn parse_reasoning_openai(m: &Value) -> Option<CatalogReasoning> {
    let supported_parameters = string_array(&m["supported_parameters"]);
    let has_reasoning = supported_parameters.contains(&"reasoning")
        || supported_parameters.contains(&"reasoning_effort");
    if !has_reasoning {
        return None;
    }
    // Only keys that are effort levels count as levels. Kilo also publishes
    // variants like `instant` / `thinking` for models with a plain on/off
    // switch; those are presets, not a ladder, and such a model is a toggle.
    let variants = m["opencode"]["variants"].as_object();
    let had_none = variants.is_some_and(|v| v.contains_key("none"));
    let levels: Vec<String> = variants
        .map(|v| {
            canonical_sort(
                v.keys()
                    .filter(|k| CANONICAL_LEVELS.contains(&k.as_str()))
                    .cloned()
                    .collect(),
            )
        })
        .unwrap_or_default();
    if !levels.is_empty() {
        Some(CatalogReasoning {
            kind: "levels".to_string(),
            enabled: None,
            levels,
            can_disable: had_none.then_some(true),
        })
    } else {
        Some(CatalogReasoning {
            kind: "toggle".to_string(),
            enabled: None,
            levels: Vec::new(),
            can_disable: had_none.then_some(true),
        })
    }
}

/// `thinking: true` ⇒ `toggle`, no `enabled`, no `can_disable`; `thinking`
/// absent/false ⇒ `fixed`, `enabled: false`. Always `Some` — Gemini always
/// states one or the other.
fn parse_reasoning_gemini(m: &Value) -> Option<CatalogReasoning> {
    if m["thinking"].as_bool().unwrap_or(false) {
        Some(CatalogReasoning {
            kind: "toggle".to_string(),
            enabled: None,
            levels: Vec::new(),
            can_disable: None,
        })
    } else {
        Some(CatalogReasoning {
            kind: "fixed".to_string(),
            enabled: Some(false),
            levels: Vec::new(),
            can_disable: None,
        })
    }
}

/// `capabilities.thinking.supported` ⇒ `levels` from
/// `capabilities.effort.<level>.supported`; no `enabled`, no `can_disable`
/// (the tree has no `types.disabled`). `thinking.supported == false` (or the
/// key is simply missing while `capabilities` exists) ⇒ `fixed`,
/// `enabled: false`. No `capabilities` object at all ⇒ `None` (absent).
fn parse_reasoning_anthropic(m: &Value) -> Option<CatalogReasoning> {
    let caps = m.get("capabilities")?;
    let thinking_supported = caps["thinking"]["supported"].as_bool().unwrap_or(false);
    if !thinking_supported {
        return Some(CatalogReasoning {
            kind: "fixed".to_string(),
            enabled: Some(false),
            levels: Vec::new(),
            can_disable: None,
        });
    }
    let mut levels = Vec::new();
    if let Some(effort) = caps.get("effort").and_then(|e| e.as_object()) {
        for lvl in CANONICAL_LEVELS {
            let supported = effort
                .get(lvl)
                .and_then(|v| v["supported"].as_bool())
                .unwrap_or(false);
            if supported {
                levels.push(lvl.to_string());
            }
        }
    }
    // `levels` with an empty list would be a contradiction: the consuming spec
    // reads `kind` to decide whether to *offer* an effort choice, and there is
    // none to offer here. The model thinks and can be switched — that is a
    // `toggle`.
    let kind = if levels.is_empty() {
        "toggle"
    } else {
        "levels"
    };
    Some(CatalogReasoning {
        kind: kind.to_string(),
        enabled: None,
        levels,
        can_disable: None,
    })
}

/// `supported_parameters ∋ tools`. `None` when the catalog does not publish
/// `supported_parameters` at all (a stock `api.openai.com` catalog).
fn parse_tools_openai(m: &Value) -> Option<bool> {
    m["supported_parameters"]
        .as_array()
        .map(|_| string_array(&m["supported_parameters"]).contains(&"tools"))
}

/// `native` for `generateContent` models. `None` when the catalog does not
/// publish `supportedGenerationMethods` at all.
fn parse_tools_gemini(m: &Value) -> Option<bool> {
    m["supportedGenerationMethods"]
        .as_array()
        .map(|arr| arr.iter().any(|v| v.as_str() == Some("generateContent")))
}

/// `structured_outputs` ∈ `supported_parameters` → `json_schema: Some(true)`;
/// `response_format` ∈ `supported_parameters` → `json_object: Some(true)`;
/// absent otherwise. `None` as a whole when the catalog does not publish
/// `supported_parameters` at all.
fn parse_structured_output_openai(m: &Value) -> Option<CatalogStructured> {
    let arr = m["supported_parameters"].as_array()?;
    let sp: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
    Some(CatalogStructured {
        json_schema: sp.contains(&"structured_outputs").then_some(true),
        json_object: sp.contains(&"response_format").then_some(true),
    })
}

/// `[text, image]` when `capabilities.image_input.supported`; `[text]` when
/// the key is present and false; absent otherwise (no `capabilities` object,
/// no `image_input` key, or a non-bool `supported`).
fn parse_input_modalities_anthropic(m: &Value) -> Option<Vec<String>> {
    let caps = m.get("capabilities")?;
    let image_input = caps.get("image_input")?;
    let supported = image_input["supported"].as_bool()?;
    Some(if supported {
        vec!["text".to_string(), "image".to_string()]
    } else {
        vec!["text".to_string()]
    })
}

/// `[text]` when `capabilities` is present (any value); absent otherwise.
fn parse_output_modalities_anthropic(m: &Value) -> Option<Vec<String>> {
    m.get("capabilities").map(|_| vec!["text".to_string()])
}

/// `created_at` (RFC3339) parsed to unix seconds.
fn parse_created_anthropic(m: &Value) -> Option<i64> {
    m["created_at"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp())
}

/// A JSON array of strings, as `&str`s. Empty when the value is not an array.
fn string_array(v: &Value) -> Vec<&str> {
    v.as_array()
        .map(|arr| arr.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default()
}

/// Sort levels least → most in the canonical order
/// `minimal < low < medium < high < xhigh < max`. A level outside that
/// vocabulary sorts after all canonical ones, alphabetically among itself.
fn canonical_sort(mut levels: Vec<String>) -> Vec<String> {
    levels.sort_by_key(|l| {
        let rank = CANONICAL_LEVELS
            .iter()
            .position(|c| *c == l.as_str())
            .unwrap_or(usize::MAX);
        (rank, l.clone())
    });
    levels
}

#[cfg(test)]
mod price_sync_tests {
    use super::*;

    fn model(id: &str, pricing: Option<Pricing>) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            pricing,
            ..Default::default()
        }
    }

    fn openrouter_pricing() -> Pricing {
        // Kilo/OpenRouter shape: USD per token, as a verbatim decimal string.
        Pricing {
            prompt: "0.000003".to_string(),
            completion: "0.000012".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn catalog_per_token_price_converts_to_per_mtok_row() {
        let models = vec![model("gpt-4o-mini", Some(openrouter_pricing()))];
        let updates = price_updates_for_upstream(7, &models, &[]);

        assert_eq!(updates.unpriced_models, 0);
        assert_eq!(updates.rows.len(), 1);
        let row = &updates.rows[0];
        assert_eq!(row.scope_kind, PriceScope::UpstreamModel);
        assert_eq!(row.scope_key, upstream_scope_key(7, "gpt-4o-mini"));
        assert_eq!(
            row.prices.price_in,
            Some(3.0),
            "0.000003 USD/token -> 3 USD/Mtok"
        );
        assert_eq!(
            row.prices.price_out,
            Some(12.0),
            "0.000012 USD/token -> 12 USD/Mtok"
        );
        assert_eq!(row.prices.source, PriceSource::Catalog);
        // No cache dimensions in the Kilo/OpenRouter shape: never invented.
        assert_eq!(row.prices.price_cache_read, None);
        assert_eq!(row.prices.price_cache_write, None);
    }

    #[test]
    fn a_model_with_no_advertised_price_writes_no_row() {
        // Gemini's models.list publishes no pricing at all.
        let models = vec![model("gemini-2.5-pro", None)];
        let updates = price_updates_for_upstream(3, &models, &[]);

        assert!(
            updates.rows.is_empty(),
            "no price advertised must upsert nothing — never a zero"
        );
        assert_eq!(updates.unpriced_models, 1);
    }

    #[test]
    fn unparseable_price_strings_write_no_row_either() {
        let models = vec![model(
            "weird",
            Some(Pricing {
                prompt: "not-a-number".to_string(),
                completion: "0.00001".to_string(),
                ..Default::default()
            }),
        )];
        let updates = price_updates_for_upstream(1, &models, &[]);

        assert!(updates.rows.is_empty());
        assert_eq!(updates.unpriced_models, 1);
    }

    #[test]
    fn a_mix_of_priced_and_unpriced_models_counts_both_correctly() {
        let models = vec![
            model("gpt-4o-mini", Some(openrouter_pricing())),
            model("gemini-2.5-pro", None),
        ];
        let updates = price_updates_for_upstream(7, &models, &[]);

        assert_eq!(updates.rows.len(), 1);
        assert_eq!(updates.unpriced_models, 1);
    }

    #[test]
    fn a_model_with_an_alias_is_scoped_to_the_alias_not_the_upstream_model() {
        let models = vec![model("gpt-4o-mini", Some(openrouter_pricing()))];
        let aliases = [("gpt-4o-mini", "cheap-chat")];
        let updates = price_updates_for_upstream(7, &models, &aliases);

        assert_eq!(updates.rows.len(), 1);
        assert_eq!(updates.rows[0].scope_kind, PriceScope::Alias);
        assert_eq!(updates.rows[0].scope_key, "cheap-chat");
    }

    #[test]
    fn multiple_aliases_onto_one_model_each_get_their_own_priced_row() {
        let models = vec![model("gpt-4o-mini", Some(openrouter_pricing()))];
        let aliases = [
            ("gpt-4o-mini", "cheap-chat"),
            ("gpt-4o-mini", "backup-chat"),
        ];
        let updates = price_updates_for_upstream(7, &models, &aliases);

        assert_eq!(updates.rows.len(), 2);
        let mut keys: Vec<&str> = updates.rows.iter().map(|r| r.scope_key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["backup-chat", "cheap-chat"]);
        assert!(updates
            .rows
            .iter()
            .all(|r| r.scope_kind == PriceScope::Alias));
    }

    #[test]
    fn a_catalog_that_states_a_real_zero_price_is_still_written() {
        // A genuinely free-tier model is a fact the catalog stated, not lmgw
        // inventing a number — distinct from "no price advertised" (§2.3).
        let models = vec![model(
            "free-model",
            Some(Pricing {
                prompt: "0".to_string(),
                completion: "0".to_string(),
                ..Default::default()
            }),
        )];
        let updates = price_updates_for_upstream(9, &models, &[]);

        assert_eq!(updates.rows.len(), 1);
        assert_eq!(updates.rows[0].prices.price_in, Some(0.0));
        assert_eq!(updates.rows[0].prices.source, PriceSource::Catalog);
    }

    fn with_cache(prompt: &str, read: Option<&str>, write: Option<&str>) -> Pricing {
        Pricing {
            prompt: prompt.to_string(),
            completion: "0.000025".to_string(),
            cache_read: read.map(String::from),
            cache_write: write.map(String::from),
            ..Default::default()
        }
    }

    fn prices_of(p: Pricing) -> Prices {
        let updates = price_updates_for_upstream(1, &[model("m", Some(p))], &[]);
        updates.rows.into_iter().next().expect("priced").prices
    }

    #[test]
    fn cache_prices_convert_per_token_to_per_mtok() {
        let p = prices_of(with_cache(
            "0.000005",
            Some("0.0000005"),
            Some("0.00000625"),
        ));
        assert_eq!(p.price_cache_read, Some(0.5));
        assert_eq!(p.price_cache_write, Some(6.25));
    }

    #[test]
    fn absent_or_unparseable_cache_prices_stay_none() {
        let p = prices_of(with_cache("0.000005", None, Some("garbage")));
        assert_eq!(p.price_cache_read, None);
        assert_eq!(p.price_cache_write, None);
        // A bad cache field never unprices the model itself.
        assert_eq!(p.price_in, Some(5.0));
    }

    #[test]
    fn a_zero_cache_price_on_a_paid_model_is_not_advertised() {
        let p = prices_of(with_cache("0.000005", Some("0"), Some("0")));
        assert_eq!(p.price_cache_read, None, "falls back to the input rate");
        assert_eq!(p.price_cache_write, None);
    }

    #[test]
    fn a_zero_cache_price_on_a_free_model_stays_zero() {
        let p = prices_of(with_cache("0", Some("0"), None));
        assert_eq!(p.price_cache_read, Some(0.0));
        assert_eq!(p.price_cache_write, None);
    }

    #[test]
    fn the_kilo_fixture_parses_its_cache_prices() {
        let raw = include_str!("../tests/fixtures/catalogs/kilo_anthropic_variants.json");
        let entry: Value = serde_json::from_str(raw).expect("fixture is JSON");
        let info = parse_openai_entry(&entry).expect("parses");
        let p = prices_of(info.pricing.expect("pricing"));
        assert_eq!(p.price_in, Some(5.0));
        assert_eq!(p.price_cache_read, Some(0.5));
        assert_eq!(p.price_cache_write, Some(6.25));
    }
}
