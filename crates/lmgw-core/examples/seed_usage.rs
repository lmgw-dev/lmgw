//! Fill a **dev** data dir with synthetic traffic so the Usage page can be
//! looked at with data in it.
//!
//! ```sh
//! LMGW_DATA_DIR=/tmp/lmgw-dev cargo run -p lmgw-core --example seed_usage
//! LMGW_DATA_DIR=/tmp/lmgw-dev cargo run -p lmgw-core --example headless -- 127.0.0.1:8899
//! ```
//!
//! Never point this at `~/.local/share/lmgw`: it writes hundreds of fake
//! request rows, and it refuses to run against the default data dir for that
//! reason. Rows are inserted with historical timestamps and the rollups are
//! then rebuilt from them through `store::rebuild_usage`, which is the same
//! aggregation the write path performs — so what the page renders is the real
//! query over real rollups, not a fixture.

use chrono::{Duration, Utc};
use lmgw_core::store;
use sqlx::Row;

const DAYS: i64 = 30;

/// (alias, upstream, local, in-price, out-price) — three cloud models and three
/// local ones, which is the split the page is built to make legible.
const MODELS: &[(&str, &str, bool, f64, f64)] = &[
    ("claude-opus-5", "anthropic", false, 3.0, 15.0),
    ("gpt-5-mini", "openai", false, 0.15, 0.6),
    ("gemini-3-flash", "gemini", false, 0.1, 0.4),
    ("qwen3.8", "llama-server", true, 0.0, 0.0),
    ("deepseek-v4-flash", "llama-server", true, 0.0, 0.0),
    ("embed/bge-m3", "llama-server-aux", true, 0.0, 0.0),
];

/// Uniform in **[0, 1)**.
fn rng(state: &mut u64) -> f64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
    ((*state >> 33) as f64) / (u32::MAX as f64 / 2.0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let data_dir = std::env::var("LMGW_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| lmgw_core::state::default_data_dir());
    if data_dir == lmgw_core::state::default_data_dir() {
        anyhow::bail!(
            "refusing to seed the real data dir ({}). Set LMGW_DATA_DIR to a scratch path.",
            data_dir.display()
        );
    }
    std::fs::create_dir_all(&data_dir)?;
    let pool = store::open(&data_dir.join("lmgw.sqlite")).await?;

    // Two upstreams and an alias per model. Without them `exposed_models()` is
    // empty, every series comes back with no colour slot, and nothing is
    // classified as local — so the seeded page would exercise none of the
    // paths it exists to show.
    for (id, name, kind) in [
        (1i64, "cloud", "generic"),
        (2, "llama-server", "llama_server"),
    ] {
        sqlx::query(
            "INSERT OR REPLACE INTO upstreams
                (id, name, protocol, kind, base_url, extra_headers, timeout_ms,
                 enabled, expose_all, expose_prefix, created_at, updated_at, supports_responses)
             VALUES (?1, ?2, 'openai', ?3, 'http://127.0.0.1:1', '[]', 60000,
                     1, 0, '', datetime('now'), datetime('now'), 0)",
        )
        .bind(id)
        .bind(name)
        .bind(kind)
        .execute(&pool)
        .await?;
    }
    for (alias, _, local, _, _) in MODELS {
        sqlx::query(
            "INSERT OR REPLACE INTO models
                (alias, upstream_id, upstream_model_id, param_overrides, enabled,
                 created_at, updated_at)
             VALUES (?1, ?2, ?1, '{}', 1, datetime('now'), datetime('now'))",
        )
        .bind(alias)
        .bind(if *local { 2 } else { 1 })
        .execute(&pool)
        .await?;
    }

    // Prices, so the cloud rows are actually priceable. Local models need none:
    // `prices_for` forces free_local for anything on a local container.
    for (alias, _, local, pin, pout) in MODELS {
        if *local {
            continue;
        }
        store::upsert_price(
            &pool,
            lmgw_core::config::PriceScope::Alias,
            alias,
            lmgw_core::config::PriceUnit::PerMtok,
            &lmgw_core::pricing::Prices {
                price_in: Some(*pin),
                price_out: Some(*pout),
                price_cache_read: Some(pin / 10.0),
                price_cache_write: Some(pin * 1.25),
                source: lmgw_core::pricing::PriceSource::Manual,
            },
            None,
            Some("seeded"),
        )
        .await?;
    }

    // A real key with a budget, so the Keys & policy card has a meter to draw
    // and the spend attribution is not all "(no key)".
    sqlx::query(
        "INSERT OR REPLACE INTO api_keys
            (id, name, key_hash, enabled, kind, scope_mode, scope_patterns,
             budget_micro, budget_period, rpm_limit, note)
         VALUES (100, 'laptop-cli', 'seeded-not-a-real-hash', 1, 'key', 'all', '',
                 12000000, 'month', 120, 'seeded')",
    )
    .execute(&pool)
    .await?;

    let mut seed = 20260918u64;
    let mut rows = 0u64;
    let now = Utc::now();

    for day in 0..DAYS {
        let date = now - Duration::days(DAYS - 1 - day);
        let weekend = matches!(
            chrono::Datelike::weekday(&date),
            chrono::Weekday::Sat | chrono::Weekday::Sun
        );
        let last_hour = if day == DAYS - 1 {
            chrono::Timelike::hour(&now) as i64
        } else {
            23
        };
        for hour in 0..=last_hour {
            // Work hours, plus the nightly corpus ingest at 03:00.
            let work = (9..20).contains(&hour);
            let load = if weekend { 0.25 } else { 1.0 } * if work { 1.0 } else { 0.12 };
            let ingest = hour == 3;
            let n = ((load * 14.0 * (0.6 + rng(&mut seed))) as i64) + i64::from(ingest) * 9;

            for _ in 0..n {
                let (alias, upstream, local, pin, pout) =
                    MODELS[(rng(&mut seed) * MODELS.len() as f64) as usize % MODELS.len()];
                let ts = date
                    .date_naive()
                    .and_hms_opt(hour as u32, (rng(&mut seed) * 59.0) as u32, 0)
                    .unwrap()
                    .and_utc();

                let prompt = (400.0 + rng(&mut seed) * 6000.0) as i64;
                let completion = (40.0 + rng(&mut seed) * 700.0) as i64;
                let cached = if rng(&mut seed) > 0.55 { prompt / 2 } else { 0 };
                // Only Anthropic bills a cache *write*, and it bills it at
                // 1.25x — the dearest input tier there is. Seeding it is what
                // makes the tier visible on the page at all; with every row at
                // zero, a column that prints nothing looks the same whether it
                // works or not.
                let cache_write = if upstream == "anthropic" && rng(&mut seed) > 0.7 {
                    prompt / 4
                } else {
                    0
                };
                let cost = if local {
                    Some(0)
                } else {
                    Some(
                        ((prompt - cached - cache_write) as f64 * pin
                            + cached as f64 * pin / 10.0
                            + cache_write as f64 * pin * 1.25
                            + completion as f64 * pout) as i64,
                    )
                };
                // One alias is deliberately left unpriced for a stretch, so the
                // "unpriced remainder" line has something to say.
                let unpriced = alias == "gemini-3-flash" && day > 24;

                let key_name = if rng(&mut seed) > 0.35 {
                    Some("laptop-cli")
                } else {
                    None
                };
                let total_ms = (300.0 + rng(&mut seed) * 2500.0) as i64;
                // One rate per model per day, and the duration derived from it —
                // the rollup computes throughput as Sigma(tokens)/Sigma(ms), so a
                // duration that does not follow from the rate makes every model
                // report the same number.
                let decode_rate = if local && !alias.starts_with("embed/") {
                    let base = if alias == "qwen3.8" && day > 19 {
                        33.5
                    } else {
                        41.0
                    };
                    Some(if alias == "deepseek-v4-flash" {
                        96.0
                    } else {
                        base
                    })
                } else {
                    None
                };
                let decode_ms = decode_rate.map(|r: f64| completion as f64 / r * 1000.0);

                let roll = rng(&mut seed);
                let (status, error_kind) = if roll > 0.985 {
                    (500, Some("upstream"))
                } else if roll > 0.978 {
                    (504, Some("timeout"))
                } else if roll > 0.972 && local {
                    // The owner took the card back for an evening.
                    (503, Some("gpu_hold"))
                } else {
                    (200, None)
                };

                sqlx::query(
                    "INSERT INTO request_logs (ts, client_key, ingress_proto, requested_alias,
                        upstream_id, upstream_name, egress_proto, status, ttfb_ms, total_ms,
                        prompt_tokens, completion_tokens, streamed, error_kind,
                        class, cost_micro, price_source, cached_in_tokens, key_id,
                        prefill_ms, decode_ms, decode_tok_s, prompt_n, cache_n,
                        draft_n, draft_accepted, predicted_n, cache_write_tokens)
                     VALUES (?1,?2,'openai',?3,?4,?5,'openai',?6,?7,?8,?9,?10,1,?11,
                             ?12,?13,?14,?15,?23,?16,?17,?18,?19,?20,?21,?22,?24,?25)",
                )
                .bind(ts.format("%Y-%m-%d %H:%M:%S").to_string())
                .bind(key_name)
                .bind(alias)
                .bind(if local { 2 } else { 1 })
                .bind(upstream)
                .bind(status)
                .bind((total_ms as f64 * 0.3) as i64)
                .bind(total_ms)
                .bind(prompt)
                .bind(completion)
                .bind(error_kind)
                .bind(if alias.starts_with("embed/") {
                    "aux"
                } else {
                    "chat"
                })
                .bind(if unpriced { None } else { cost })
                .bind(if unpriced {
                    "unknown"
                } else if local {
                    "free_local"
                } else {
                    "manual"
                })
                .bind(cached)
                // llama.cpp timings, local rows only — and qwen3.8 loses ~18%
                // of its decode rate two thirds of the way through, which is
                // the regression the Local performance panel exists to show.
                .bind(if local {
                    Some(60.0 + rng(&mut seed) * 40.0)
                } else {
                    None
                })
                .bind(decode_ms)
                // An encoder has no decode phase, so it reports no decode rate
                // — the same asymmetry a real aux model has.
                .bind(decode_rate)
                .bind(if local { Some(prompt) } else { None })
                .bind(if local {
                    Some((prompt as f64 * 0.68) as i64)
                } else {
                    None
                })
                .bind(if alias == "deepseek-v4-flash" {
                    Some(completion / 2)
                } else {
                    None
                })
                .bind(if alias == "deepseek-v4-flash" {
                    Some((completion as f64 * 0.37) as i64)
                } else {
                    None
                })
                .bind(key_name.map(|_| 100i64))
                // The decode count itself — what the rollup sums, and what a
                // rebuild reads back rather than reconstructing.
                .bind(if local && !alias.starts_with("embed/") {
                    Some(completion)
                } else {
                    None
                })
                .bind(cache_write)
                .execute(&pool)
                .await?;
                rows += 1;
            }
        }
    }

    let rolled = store::rebuild_usage(&pool).await?;
    let spend: i64 = sqlx::query("SELECT COALESCE(SUM(cost_micro),0) AS c FROM usage_hourly")
        .fetch_one(&pool)
        .await?
        .get("c");
    println!(
        "seeded {rows} request rows over {DAYS} days → {rolled} rollup rows, \
         {:.2} total spend",
        lmgw_core::pricing::micro_to_units(spend)
    );
    println!(
        "now: LMGW_DATA_DIR={} cargo run -p lmgw-core --example headless -- 127.0.0.1:8899",
        data_dir.display()
    );
    Ok(())
}
