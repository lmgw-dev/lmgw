//! Per-key scope, rate and budget enforcement (usage-analytics design §4).
//!
//! An API key used to be a boolean: any key that worked, worked for every
//! alias, forever, at any rate. This is a single-owner desktop gateway, so
//! "policy" here is not tenancy — it is the owner fencing off their own keys
//! from their own mistakes: an agent stuck in a loop, a script left running
//! overnight, an unattended corpus ingest pointed at a cloud embedder.
//!
//! **Three checks, three places, for a reason.**
//!
//! | Check | Where | Why there |
//! |---|---|---|
//! | expiry, rate, concurrency | the auth middleware | it has the resolved key and wraps the whole request, which is what a concurrency guard needs |
//! | alias scope | after the alias is known | the alias is in the *body*; the middleware cannot see it without buffering every request |
//! | budget | after the alias is known | same trip, and it is the check most worth doing last |
//!
//! **The overshoot is stated, not engineered away.** A request's cost is
//! knowable only after the response, so enforcement is "spend so far ≥ budget →
//! refuse the *next* request", and one request can cross the line. That bound
//! is exactly one request, it is documented, and the dashboard shows the
//! overshoot rather than clamping the display to the budget. The alternative —
//! reserving `max_tokens × output price` up front — refuses requests that would
//! have fit, on a number most clients never send, and makes the *reserved*
//! figure the one the dashboard would have to explain.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::{ApiKey, ApiKeyKind, Settings, Snapshot};
use crate::error::GatewayError;
use crate::pricing::micro_to_units;

/// Rolling window for the per-minute limits.
const WINDOW: Duration = Duration::from_secs(60);

/// How long a cached period spend is trusted before it is read back from the
/// rollup. See [`KeyState::spend_seeded_at`].
const RESEED: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct KeyState {
    window_start: Instant,
    requests: i64,
    tokens: i64,
    in_flight: i64,
    /// Spend in the current budget period, in micro-units. Seeded from the
    /// rollup the first time the key is checked, then kept current by
    /// [`PolicyGate::note_spend`] — so the hot path never runs a SUM.
    spend_micro: i64,
    /// The period key `spend_micro` was seeded for; a rollover re-seeds.
    spend_period: String,
    /// When it was last read from the rollup. The cache is an optimisation, not
    /// a ledger: a request in flight across the seeding instant has its
    /// `note_spend` dropped *and* is not yet in the SUM, so the cached figure
    /// can start a period slightly low. Re-seeding on this interval bounds that
    /// drift to a minute instead of letting it stand until the month rolls
    /// over — and until then the dashboard, which reads the DB directly, and
    /// the gate would disagree with no way to tell which was live.
    spend_seeded_at: Instant,
}

impl KeyState {
    fn new(now: Instant) -> Self {
        Self {
            window_start: now,
            requests: 0,
            tokens: 0,
            in_flight: 0,
            spend_micro: 0,
            spend_period: String::new(),
            spend_seeded_at: now,
        }
    }

    fn roll(&mut self, now: Instant) {
        if now.duration_since(self.window_start) >= WINDOW {
            self.window_start = now;
            self.requests = 0;
            self.tokens = 0;
        }
    }

    fn retry_after(&self, now: Instant) -> u64 {
        WINDOW
            .saturating_sub(now.duration_since(self.window_start))
            .as_secs()
            .max(1)
    }
}

/// In-memory half of the policy plane. Lives on `AppState`.
#[derive(Debug, Default)]
pub struct PolicyGate {
    keys: Mutex<HashMap<i64, KeyState>>,
    /// Gateway-wide spend for the global budget, same seeding rule:
    /// `(period key, micro-units, seeded at)`.
    global: Mutex<(String, i64, Option<Instant>)>,
}

/// Held for the lifetime of a request so the concurrency count is released even
/// when a handler returns early or panics.
#[derive(Debug)]
pub struct ConcurrencyGuard {
    gate: std::sync::Arc<PolicyGate>,
    key_id: i64,
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.gate.keys.lock() {
            if let Some(st) = m.get_mut(&self.key_id) {
                st.in_flight = (st.in_flight - 1).max(0);
            }
        }
    }
}

/// The gate's concurrency slot, offered to the handler of the request it was
/// taken for (realtime design §10.3).
///
/// The gate normally keeps the slot until the response *body* ends
/// (`server::hold_until_body_end`). A WebSocket's response is the 101, and
/// hyper drops its (empty) body the moment the connection is upgraded — so a
/// realtime session would give its slot back as it starts, and a
/// `concurrency_limit` of 1 would admit any number of open sessions. A
/// handler whose work outlives its response [`take`](Self::take)s the guard
/// and holds it for as long as that work runs; one that does not leaves it
/// to the gate. Taking it, rather than admitting a second time, keeps one
/// session at one request for both the concurrency and the per-minute count.
#[derive(Debug, Clone)]
pub struct SlotHandover(std::sync::Arc<Mutex<Option<ConcurrencyGuard>>>);

impl SlotHandover {
    pub fn new(guard: ConcurrencyGuard) -> Self {
        Self(std::sync::Arc::new(Mutex::new(Some(guard))))
    }

    /// The guard, if nobody has taken it yet.
    pub fn take(&self) -> Option<ConcurrencyGuard> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl PolicyGate {
    /// Expiry, requests-per-minute and concurrency, checked at the door.
    ///
    /// Returns a guard that releases the concurrency slot on drop. `None` back
    /// means there was nothing to hold (no limits configured), which keeps the
    /// common path allocation-free.
    pub fn admit(
        self: &std::sync::Arc<Self>,
        key: &ApiKey,
        now_utc: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<ConcurrencyGuard>, GatewayError> {
        self.admit_counting(key, now_utc, true)
    }

    /// [`Self::admit`] for a request that is not itself a model call and
    /// makes its model calls later, each counted with [`Self::count_call`]:
    /// a realtime session (realtime design §10.3 — rpm and tpm count model
    /// calls, not the session). Expiry, the per-minute windows as they stand
    /// and the concurrency slot are checked and the slot is taken, but the
    /// request is not counted as one.
    pub fn admit_session(
        self: &std::sync::Arc<Self>,
        key: &ApiKey,
        now_utc: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<ConcurrencyGuard>, GatewayError> {
        self.admit_counting(key, now_utc, false)
    }

    /// One concurrency slot and nothing else, for work whose model calls
    /// are each checked and counted as they are made ([`Self::count_call`]):
    /// a device's Chat turn (client-apps design §1.3, review W3-8). The
    /// windows and the expiry are the calls' to check; the slot is what
    /// bounds how many turns run at once. `None` without a limit.
    pub fn take_slot(
        self: &std::sync::Arc<Self>,
        key: &ApiKey,
    ) -> Result<Option<ConcurrencyGuard>, GatewayError> {
        let limit = key.policy.concurrency_limit;
        if limit <= 0 {
            return Ok(None);
        }
        let now = Instant::now();
        let mut m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let st = m.entry(key.id).or_insert_with(|| KeyState::new(now));
        if st.in_flight >= limit {
            return Err(GatewayError::KeyRate {
                key: key.described(),
                limit_kind: "concurrent requests",
                limit,
                retry_after: 1,
            });
        }
        st.in_flight += 1;
        Ok(Some(ConcurrencyGuard {
            gate: self.clone(),
            key_id: key.id,
        }))
    }

    fn admit_counting(
        self: &std::sync::Arc<Self>,
        key: &ApiKey,
        now_utc: chrono::DateTime<chrono::Utc>,
        count: bool,
    ) -> Result<Option<ConcurrencyGuard>, GatewayError> {
        usable(key, now_utc)?;
        let p = &key.policy;
        if p.rpm_limit <= 0 && p.tpm_limit <= 0 && p.concurrency_limit <= 0 {
            return Ok(None);
        }

        let now = Instant::now();
        let mut m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let st = m.entry(key.id).or_insert_with(|| KeyState::new(now));
        st.roll(now);

        per_minute(st, key, now)?;
        if p.concurrency_limit > 0 && st.in_flight >= p.concurrency_limit {
            return Err(GatewayError::KeyRate {
                key: key.described(),
                limit_kind: "concurrent requests",
                limit: p.concurrency_limit,
                // Concurrency clears when something finishes, not on a clock;
                // one second is the honest "ask again shortly".
                retry_after: 1,
            });
        }

        if count {
            st.requests += 1;
        }
        st.in_flight += 1;
        Ok(Some(ConcurrencyGuard {
            gate: self.clone(),
            key_id: key.id,
        }))
    }

    /// Expiry, requests-per-minute and tokens-per-minute for one model call
    /// made **inside** a request that already holds its key's concurrency
    /// slot — a realtime session's chat, ASR and TTS calls (realtime design
    /// §10.3), whose rate limits count model calls, not the session.
    ///
    /// [`Self::admit`] takes a second slot along with the count, which a
    /// session under `concurrency_limit: 1` could never get; this counts the
    /// call and takes none — the session's own slot is what holds its place.
    pub fn count_call(
        &self,
        key: &ApiKey,
        now_utc: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), GatewayError> {
        usable(key, now_utc)?;
        let p = &key.policy;
        if p.rpm_limit <= 0 && p.tpm_limit <= 0 {
            return Ok(());
        }
        let now = Instant::now();
        let mut m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let st = m.entry(key.id).or_insert_with(|| KeyState::new(now));
        st.roll(now);
        per_minute(st, key, now)?;
        st.requests += 1;
        Ok(())
    }

    /// Whether the gate keeps any window for `key_id` — what a test reads
    /// to see that a deleted key got none (`proxy::recording`).
    #[cfg(test)]
    pub(crate) fn tracks(&self, key_id: i64) -> bool {
        self.keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&key_id)
    }

    /// Fold a finished request's tokens into the per-minute token window.
    pub fn note_tokens(&self, key_id: i64, tokens: i64) {
        if tokens <= 0 {
            return;
        }
        let now = Instant::now();
        let mut m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let st = m.entry(key_id).or_insert_with(|| KeyState::new(now));
        st.roll(now);
        st.tokens += tokens;
    }

    /// Fold a finished request's cost into the cached period spend, so the
    /// budget check never has to run a SUM on the request path.
    ///
    /// A key's budget period and the gateway's need not be the same (a daily
    /// key inside a monthly global is a perfectly sensible setup), so the two
    /// counters are advanced separately, each against its own period key. An
    /// increment against a period that is not the cached one is dropped: the
    /// next check re-seeds from the rollup, which is the authority.
    pub fn note_spend(&self, key: Option<(i64, &str)>, global_period: &str, micro: i64) {
        if micro == 0 {
            return;
        }
        if let Some((key_id, period_key)) = key {
            let mut m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
            let st = m
                .entry(key_id)
                .or_insert_with(|| KeyState::new(Instant::now()));
            if st.spend_period == period_key {
                st.spend_micro += micro;
            }
        }
        let mut g = self.global.lock().unwrap_or_else(|e| e.into_inner());
        if g.0 == global_period {
            g.1 += micro;
        }
    }

    /// Spend so far for a key in `period_key`, seeding from the rollup on first
    /// use or after a period rollover.
    async fn spend_for(
        &self,
        db: &sqlx::SqlitePool,
        key_id: i64,
        period_key: &str,
    ) -> Result<i64, GatewayError> {
        {
            let m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(st) = m.get(&key_id) {
                if st.spend_period == period_key && st.spend_seeded_at.elapsed() < RESEED {
                    return Ok(st.spend_micro);
                }
            }
        }
        let spent = crate::store::key_spend_micro(db, key_id, period_key).await?;
        let mut m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let st = m
            .entry(key_id)
            .or_insert_with(|| KeyState::new(Instant::now()));
        st.spend_period = period_key.to_string();
        st.spend_micro = spent;
        Ok(spent)
    }

    async fn global_spend(
        &self,
        db: &sqlx::SqlitePool,
        period_key: &str,
    ) -> Result<i64, GatewayError> {
        {
            let g = self.global.lock().unwrap_or_else(|e| e.into_inner());
            if g.0 == period_key && g.2.is_some_and(|at| at.elapsed() < RESEED) {
                return Ok(g.1);
            }
        }
        let spent = crate::store::total_spend_micro(db, period_key).await?;
        let mut g = self.global.lock().unwrap_or_else(|e| e.into_inner());
        *g = (period_key.to_string(), spent, Some(Instant::now()));
        Ok(spent)
    }
}

/// A key that may be used at all: never an internal identity, never past
/// its expiry.
fn usable(key: &ApiKey, now_utc: chrono::DateTime<chrono::Utc>) -> Result<(), GatewayError> {
    // An internal identity is budgetable but must never authenticate
    // anything; it has no usable hash, and this is the backstop.
    if key.kind == ApiKeyKind::Internal {
        return Err(GatewayError::Unauthorized(
            "missing or invalid gateway API key",
        ));
    }
    // One reading of the expiry for `/v1` and `Chat` (review W2-22).
    check_expiry(key, now_utc).map_err(|expired_at| GatewayError::KeyExpired {
        key: key.described(),
        expired_at,
    })
}

/// The per-minute windows, checked before a request is counted into them.
fn per_minute(st: &KeyState, key: &ApiKey, now: Instant) -> Result<(), GatewayError> {
    let p = &key.policy;
    if p.rpm_limit > 0 && st.requests >= p.rpm_limit {
        return Err(GatewayError::KeyRate {
            key: key.described(),
            limit_kind: "requests/minute",
            limit: p.rpm_limit,
            retry_after: st.retry_after(now),
        });
    }
    if p.tpm_limit > 0 && st.tokens >= p.tpm_limit {
        return Err(GatewayError::KeyRate {
            key: key.described(),
            limit_kind: "tokens/minute",
            limit: p.tpm_limit,
            retry_after: st.retry_after(now),
        });
    }
    Ok(())
}

/// Scope, then budget — the two checks that need the resolved alias.
///
/// `key_name` is `None` when auth is off, which is the default and means no key
/// policy applies. The **global** budget still does: it is the owner's ceiling
/// on the gateway, not on a credential.
pub async fn check_alias(
    gate: &PolicyGate,
    db: &sqlx::SqlitePool,
    snap: &Snapshot,
    key_name: Option<&str>,
    alias: &str,
) -> Result<(), GatewayError> {
    check_identity(gate, db, snap, key_name, alias).await
}

/// The same two checks for work the gateway does on its own behalf
/// (usage-analytics §4.4).
///
/// Admin Chat, corpus ingest, golden-query generation and the rest already
/// *log* under an `internal:*` identity; without this they were budgetable only
/// in the sense that the number went up. The design's own motivating scenario —
/// an unattended re-embed against a cloud embedder — ran past a €10 global
/// budget without one refusal, because the only place a budget was evaluated
/// was a code path that ingest never enters.
pub async fn check_internal(
    gate: &PolicyGate,
    db: &sqlx::SqlitePool,
    snap: &Snapshot,
    ingress_proto: &str,
    alias: &str,
) -> Result<(), GatewayError> {
    let name = crate::telemetry::internal_identity(ingress_proto);
    check_identity(gate, db, snap, name, alias).await
}

/// The key's alias scope alone, without either budget — what the token
/// counters check (api-docs design §12 entry 15).
///
/// A count costs nothing, so a budget refusal there would only block free
/// work — sizing the very request a client is about to trim to fit. The scope
/// still applies: it is access, and counting on a local model starts its
/// container, so a key fenced off an alias must not cold-load it by counting.
/// Synchronous, and no spend query: the counters run it on every count.
pub fn check_scope(
    snap: &Snapshot,
    key_name: Option<&str>,
    alias: &str,
) -> Result<(), GatewayError> {
    let Some(key) = key_name.and_then(|n| snap.api_keys.iter().find(|k| k.name == n)) else {
        return Ok(());
    };
    let p = &key.policy;
    if p.admits(alias) {
        return Ok(());
    }
    Err(GatewayError::KeyScope {
        key: key.described(),
        alias: alias.to_string(),
        reason: match p.scope_mode {
            crate::config::ScopeMode::Allow => {
                "its scope lists the aliases it may use and this is not one".into()
            }
            _ => "its scope excludes this alias".into(),
        },
    })
}

async fn check_identity(
    gate: &PolicyGate,
    db: &sqlx::SqlitePool,
    snap: &Snapshot,
    key_name: Option<&str>,
    alias: &str,
) -> Result<(), GatewayError> {
    let settings: &Settings = &snap.settings;
    let cur = &settings.currency;

    check_scope(snap, key_name, alias)?;
    if let Some(key) = key_name.and_then(|n| snap.api_keys.iter().find(|k| k.name == n)) {
        let p = &key.policy;
        if p.budget_micro > 0 {
            let period = p.budget_period.start_hour_key(chrono::Utc::now());
            let spent = gate.spend_for(db, key.id, &period).await?;
            if spent >= p.budget_micro {
                return Err(GatewayError::KeyBudget {
                    scope: key.described(),
                    spent: money(spent, cur),
                    budget: money(p.budget_micro, cur),
                    period: p.budget_period.as_str(),
                    set_on: "Usage → Keys",
                });
            }
        }
    }

    if settings.global_budget_micro > 0 {
        let period = settings
            .global_budget_period
            .start_hour_key(chrono::Utc::now());
        let spent = gate.global_spend(db, &period).await?;
        if spent >= settings.global_budget_micro {
            return Err(GatewayError::KeyBudget {
                scope: "this gateway".into(),
                spent: money(spent, cur),
                budget: money(settings.global_budget_micro, cur),
                period: settings.global_budget_period.as_str(),
                set_on: "Settings → Usage & cost",
            });
        }
    }
    Ok(())
}

fn money(micro: i64, currency: &str) -> String {
    format!("{:.2} {currency}", micro_to_units(micro))
}

/// Is `when` (a date or RFC3339 timestamp) in the past?
///
/// A bare `YYYY-MM-DD` means the *end* of that day — an owner who types
/// "expires 2026-12-31" means the key works on the 31st, not that it died at
/// midnight as the 31st began.
fn expired(when: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
    match expiry_deadline(when) {
        Some(end) => end <= now,
        // Unparseable: refusing on a typo would lock the owner out of their
        // own gateway over a date field. Log-worthy, not refusal-worthy.
        None => {
            tracing::warn!("api key expiry '{when}' is not a date — ignoring");
            false
        }
    }
}

/// The instant an `expires_at` value means, in [`expired`]'s reading (a
/// bare date is the end of that day); `None` for a value that is not a date.
/// What a device's revocation watch sleeps until (client-apps design L17).
pub fn expiry_deadline(when: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(when) {
        return Some(ts.with_timezone(&chrono::Utc));
    }
    chrono::NaiveDate::parse_from_str(when, "%Y-%m-%d")
        .ok()
        .map(|d| d.and_hms_opt(23, 59, 59).unwrap_or_default().and_utc())
}

/// The key's expiry alone, as `/v1`'s gate checks it (`usable`): what the
/// gate runs on a `Chat` route, which takes no slot and counts no window
/// (client-apps design §1.2, L17). `Err` carries the date it expired on.
pub fn check_expiry(key: &ApiKey, now_utc: chrono::DateTime<chrono::Utc>) -> Result<(), String> {
    match key.policy.expires_at.as_deref().filter(|e| !e.is_empty()) {
        Some(exp) if expired(exp, now_utc) => Err(exp.to_string()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BudgetPeriod, KeyPolicy, ScopeMode};
    use std::sync::Arc;

    fn key(policy: KeyPolicy) -> ApiKey {
        ApiKey {
            id: 1,
            name: "k".into(),
            key_hash: "h".into(),
            enabled: true,
            policy,
            ..Default::default()
        }
    }

    #[test]
    fn scope_globs_match_the_way_a_text_box_implies() {
        let p = KeyPolicy {
            scope_mode: ScopeMode::Allow,
            scope_patterns: "claude-*\n*-mini\nqwen3.8".into(),
            ..Default::default()
        };
        assert!(p.admits("claude-opus-5"));
        assert!(
            p.admits("CLAUDE-opus-5"),
            "aliases resolve case-insensitively"
        );
        assert!(p.admits("gpt-5-mini"));
        assert!(p.admits("qwen3.8"));
        assert!(!p.admits("gpt-5"));
        assert!(
            !p.admits("gpt-5-mini-2"),
            "a trailing literal must end the name"
        );
        assert!(!p.admits("claude"), "claude-* needs the dash");

        // Backtracking: the trailing literal also occurs earlier in the name. A
        // greedy scan takes the first `-mini`, finds the value does not end
        // there, and gives up — refusing a request it should allow.
        assert!(p.admits("gpt-mini-mini"));
        assert!(p.admits("claude-5-opus-mini"));

        let five = KeyPolicy {
            scope_mode: ScopeMode::Deny,
            scope_patterns: "*-5".into(),
            ..Default::default()
        };
        // …and in deny mode the same bug fails OPEN: the key the owner fenced
        // off from the -5 generation would have been admitted.
        assert!(!five.admits("claude-5-opus-5"), "deny must not fail open");
        assert!(!five.admits("gpt-5"));
        assert!(five.admits("gpt-4o"));

        let inner = KeyPolicy {
            scope_mode: ScopeMode::Allow,
            scope_patterns: "gpt-*-preview\na*bc\n*".into(),
            ..Default::default()
        };
        assert!(inner.admits("gpt-5-preview-preview"));
        assert!(inner.admits("abcbc"));
        assert!(inner.admits("literally-anything"), "a bare * matches all");

        let d = KeyPolicy {
            scope_mode: ScopeMode::Deny,
            scope_patterns: "claude-*".into(),
            ..Default::default()
        };
        assert!(!d.admits("claude-opus-5"));
        assert!(d.admits("qwen3.8"));

        // The default admits everything — adding policy must not silently
        // narrow an existing key.
        assert!(KeyPolicy::default().admits("anything-at-all"));
    }

    #[test]
    fn rpm_limit_refuses_with_a_real_retry_after() {
        let gate = Arc::new(PolicyGate::default());
        let k = key(KeyPolicy {
            rpm_limit: 2,
            ..Default::default()
        });
        let now = chrono::Utc::now();
        let _g1 = gate.admit(&k, now).unwrap();
        let _g2 = gate.admit(&k, now).unwrap();
        let e = gate.admit(&k, now).unwrap_err();
        assert_eq!(e.kind(), "key_rate");
        assert_eq!(e.http_status(), 429);
        match e {
            GatewayError::KeyRate { retry_after, .. } => {
                assert!(
                    (1..=60).contains(&retry_after),
                    "retry_after was {retry_after}"
                )
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn concurrency_is_released_when_the_guard_drops() {
        let gate = Arc::new(PolicyGate::default());
        let k = key(KeyPolicy {
            concurrency_limit: 1,
            ..Default::default()
        });
        let now = chrono::Utc::now();
        let g = gate.admit(&k, now).unwrap();
        assert_eq!(gate.admit(&k, now).unwrap_err().kind(), "key_rate");
        drop(g);
        assert!(gate.admit(&k, now).is_ok(), "the slot came back");
    }

    #[test]
    fn a_call_inside_a_held_slot_counts_rpm_and_takes_no_second_slot() {
        let gate = Arc::new(PolicyGate::default());
        let k = key(KeyPolicy {
            rpm_limit: 3,
            concurrency_limit: 1,
            ..Default::default()
        });
        let now = chrono::Utc::now();
        // The session: one request, holding the only slot.
        let _session = gate.admit(&k, now).unwrap();
        // Its model calls are not refused for concurrency…
        gate.count_call(&k, now).unwrap();
        gate.count_call(&k, now).unwrap();
        // …but each one counted towards the minute.
        let e = gate.count_call(&k, now).unwrap_err();
        assert_eq!(e.kind(), "key_rate");
        assert!(e.to_string().contains("requests/minute"), "{e}");
    }

    #[test]
    fn an_internal_identity_can_never_authenticate() {
        let gate = Arc::new(PolicyGate::default());
        let mut k = key(KeyPolicy::default());
        k.kind = ApiKeyKind::Internal;
        k.key_hash = String::new();
        assert_eq!(
            gate.admit(&k, chrono::Utc::now()).unwrap_err().kind(),
            "auth"
        );
    }

    #[test]
    fn expiry_gives_the_owner_the_whole_day_they_typed() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-12-31T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(!expired("2026-12-31", now), "still the 31st");
        assert!(expired("2026-12-30", now));
        assert!(!expired("2027-01-01", now));
        assert!(
            !expired("not a date", now),
            "a typo must not lock the owner out"
        );
    }

    #[tokio::test]
    async fn a_budget_refuses_with_403_not_429() {
        // 429 is every SDK's retry-with-backoff signal, and a monthly budget
        // will not clear during any retry window.
        let pool = crate::store::open_in_memory().await.unwrap();
        let gate = PolicyGate::default();
        let mut snap = Snapshot::default();
        snap.settings.currency = "EUR".into();
        snap.api_keys.push(key(KeyPolicy {
            budget_micro: 1_000_000,
            budget_period: BudgetPeriod::Month,
            ..Default::default()
        }));

        // Under budget: allowed.
        check_alias(&gate, &pool, &snap, Some("k"), "any")
            .await
            .unwrap();

        // Spend past it, then the *next* request is refused — the overshoot is
        // one request, by design.
        let period = BudgetPeriod::Month.start_hour_key(chrono::Utc::now());
        gate.spend_for(&pool, 1, &period).await.unwrap();
        gate.note_spend(Some((1, &period)), &period, 1_500_000);

        let e = check_alias(&gate, &pool, &snap, Some("k"), "any")
            .await
            .unwrap_err();
        assert_eq!(e.kind(), "key_budget");
        assert_eq!(
            e.http_status(),
            403,
            "not 429 — see the variant's doc comment"
        );
        assert!(e.to_string().contains("EUR"), "message names the currency");
        assert!(
            e.to_string().contains("1.50"),
            "and what was actually spent"
        );
    }

    #[tokio::test]
    async fn an_internal_consumer_is_budgeted_too() {
        // The design's own motivating scenario: an unattended re-embed against
        // a cloud embedder. Its spend was attributed and rolled up, and nothing
        // ever checked it, because the only place a budget was evaluated was a
        // code path ingest never enters.
        let pool = crate::store::open_in_memory().await.unwrap();
        let gate = PolicyGate::default();
        let mut snap = Snapshot::default();
        snap.settings.global_budget_micro = 1_000_000;

        // The ingest identity, as the migration seeds it.
        let mut ingest = key(KeyPolicy::default());
        ingest.id = 42;
        ingest.name = "internal:quickdoc-ingest".into();
        ingest.kind = ApiKeyKind::Internal;
        snap.api_keys.push(ingest);

        check_internal(&gate, &pool, &snap, "quickdoc-ingest", "embed/cloud")
            .await
            .expect("under budget");

        let period = snap
            .settings
            .global_budget_period
            .start_hour_key(chrono::Utc::now());
        gate.global_spend(&pool, &period).await.unwrap();
        gate.note_spend(None, &period, 2_000_000);

        let e = check_internal(&gate, &pool, &snap, "quickdoc-ingest", "embed/cloud")
            .await
            .unwrap_err();
        assert_eq!(e.kind(), "key_budget");

        // A proto with no internal identity still gets the global check — the
        // ceiling is on the gateway, not on a credential.
        assert_eq!(
            check_internal(&gate, &pool, &snap, "something-else", "x")
                .await
                .unwrap_err()
                .kind(),
            "key_budget"
        );
    }

    #[tokio::test]
    async fn the_global_budget_applies_even_with_auth_off() {
        let pool = crate::store::open_in_memory().await.unwrap();
        let gate = PolicyGate::default();
        let mut snap = Snapshot::default();
        snap.settings.global_budget_micro = 500_000;
        let period = snap
            .settings
            .global_budget_period
            .start_hour_key(chrono::Utc::now());
        gate.global_spend(&pool, &period).await.unwrap();
        gate.note_spend(None, &period, 600_000);

        let e = check_alias(&gate, &pool, &snap, None, "any")
            .await
            .unwrap_err();
        assert_eq!(e.kind(), "key_budget");
        assert!(e.to_string().contains("this gateway"));
    }
}
