//! Usage analytics, cost & policy
//! (docs/design/2026-09-18-usage-analytics-cost-policy-design.md)

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One (bucket, series) cell.
///
/// Money is integer **micro-units** of the configured currency everywhere on
/// this plane; a float that has been through JSON twice does not add up.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageCell {
    /// Local-time bucket label: `2026-09-18`, `2026-09-18T14`, `2026-W38`, `2026-09`.
    pub bucket: String,
    /// The series key this cell belongs to (`""` when ungrouped).
    pub series: String,
    pub requests: i64,
    /// **Total** input, cache included — the same meaning `ir::Usage`'s
    /// `prompt_tokens` carries, so fresh (uncached) input is
    /// `tokens_in - tokens_cached - tokens_cache_write` and never a fourth
    /// stored number that could drift from the other three.
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Input served from the provider's prompt cache, billed at the cheap
    /// read rate.
    pub tokens_cached: i64,
    /// Input written *into* the prompt cache — Anthropic's 1.25x tier, the
    /// dearest input a window can contain. Zero everywhere else, because no
    /// other provider bills a cache write.
    pub tokens_cache_write: i64,
    pub tokens_reasoning: i64,
    pub cost_micro: i64,
    /// What `cost_micro` does **not** cover. Every total that is displayed is
    /// obliged to show this beside it — a cost figure with a
    /// silent hole in it is worse than no cost figure.
    pub cost_unknown_requests: i64,
    pub cost_unknown_tokens: i64,
    pub errors: i64,
    pub refusals: i64,
    pub ttfb_sum: i64,
    pub ttfb_count: i64,
    pub total_sum: i64,
    pub total_count: i64,
    /// Local-model throughput as a ratio pair: tok/s is Σtokens / Σms, never a
    /// mean of per-request rates.
    pub decode_tokens: i64,
    pub decode_ms: f64,
    pub prefill_ms: f64,
    pub prompt_n: i64,
    pub cache_n: i64,
    pub draft_n: i64,
    pub draft_accepted: i64,
    // Billable-units design §5.3.
    /// What the requests processed besides tokens, as **measured** or
    /// reported by the provider: input audio in milliseconds, input
    /// characters, generated images. A request that measured nothing adds 0,
    /// so these are measured totals, and before the series' `units_since`
    /// nothing was recorded at all.
    pub audio_in_ms: i64,
    pub chars_in: i64,
    pub images_out: i64,
    /// The same quantities of the requests `cost_unknown_requests` counts,
    /// so the remainder can say what it holds beside its tokens.
    pub cost_unknown_audio_in_ms: i64,
    pub cost_unknown_chars_in: i64,
    pub cost_unknown_images_out: i64,
}

/// Identity of one series, with the colour slot it wears everywhere.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SeriesMeta {
    pub key: String,
    /// What to print — a key id resolves to its name here, an upstream id to
    /// its name, an alias is itself.
    pub label: String,
    /// Chart colour slot 0..5, or `null` for the neutral "Other" bucket.
    ///
    /// Assigned from the entity's position in the gateway's own sorted list of
    /// aliases, **not** from its rank in this response: a filter that drops a
    /// series must not repaint the survivors, or a reader who learned "qwen is
    /// teal" is being lied to.
    pub slot: Option<u8>,
    /// True for a locally-served model — it can never appear in a spend chart,
    /// because its money cost is a real zero.
    pub local: bool,
    /// On the folded "Other" series only: how many entities it sums, so a
    /// legend can say "Other (49 aliases)" instead of hiding the tail's size.
    /// `null` on every named series.
    pub folded: Option<u32>,
}

/// `GET /api/usage/series`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageSeriesResponse {
    /// Every bucket label in the window, in order, including empty ones — a
    /// chart that silently drops quiet days draws a lie about its own x-axis.
    pub buckets: Vec<String>,
    pub series: Vec<SeriesMeta>,
    pub cells: Vec<UsageCell>,
    pub totals: UsageCell,
    /// Totals over the immediately preceding window of the same length, for the
    /// tiles' "vs previous" delta. `None` when that window predates the data.
    pub previous: Option<UsageCell>,
    pub currency: String,
    /// p50 / p95 of total latency and TTFB over the window, in ms. `None` where
    /// the window holds no successful request: an empty window has no p95, and
    /// 0 would draw a gateway that got infinitely fast.
    pub p50_total_ms: Option<f64>,
    pub p95_total_ms: Option<f64>,
    pub p50_ttfb_ms: Option<f64>,
    pub p95_ttfb_ms: Option<f64>,
    /// The same four percentiles **per bucket**, aligned with `buckets`, so a
    /// latency chart plots a real p50/p95 line rather than a mean. Dividing the
    /// rollup's sums would give a mean, and one 30-second outlier drags a mean
    /// four times above the typical request.
    pub latency: Vec<LatencyPoint>,
    /// When this install started recording the billable quantities
    /// (`audio_in_ms`, `chars_in`, `images_out`), RFC 3339 UTC. A bucket
    /// before it reads 0 because nothing was recorded, not because nothing
    /// was processed: show such a bucket as not recorded rather than as
    /// zero.
    // Billable-units design §5.5.
    ///
    /// Absent when the bookkeeping holds no such row.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub units_since: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LatencyPoint {
    pub bucket: String,
    pub p50_total_ms: Option<f64>,
    pub p95_total_ms: Option<f64>,
    pub p50_ttfb_ms: Option<f64>,
    pub p95_ttfb_ms: Option<f64>,
}

/// `GET /api/usage/top` — ranked totals for one dimension.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageTopResponse {
    pub rows: Vec<UsageCell>,
    pub series: Vec<SeriesMeta>,
    pub currency: String,
}

/// `GET /api/usage/heat` — weekday (0 = Sunday) × hour request counts, local time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageHeatResponse {
    pub cells: Vec<HeatCell>,
    pub max: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HeatCell {
    pub dow: i64,
    pub hour: i64,
    pub requests: i64,
}

/// `GET /api/usage/local` — the local/cloud split and the counterfactual.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageLocalResponse {
    pub local_tokens: i64,
    pub cloud_tokens: i64,
    /// What the local tokens **would** have cost at `reference_alias`'s price.
    /// Not a saving — nobody would have run all of it there.
    /// `None` when no reference alias is configured, which the page must say
    /// rather than quietly picking one.
    pub counterfactual_micro: Option<i64>,
    pub reference_alias: Option<String>,
    pub currency: String,
    /// Throughput and cache facts for the local models in the window.
    pub decode_tok_s: Option<f64>,
    pub kv_reuse: Option<f64>,
    pub draft_acceptance: Option<f64>,
}

/// A key's state moved without a request landing: the `keys` frame of
/// `/api/events`. A device's connection opened or closed — its online state
/// and `last_seen_at` — or a key was disabled, rotated or deleted. The Keys
/// page refetches its list on it, instead of polling.
// The client-apps design record, §1.6.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KeysChanged {
    /// The `api_keys.id` it is about.
    pub key_id: i64,
    /// `link` (a device's connection opened or closed), `disabled`,
    /// `rotated` or `deleted`; a device's pairing (device link design §9.3):
    /// `token` (a pairing token minted or ended), `enrolled` (a device sent
    /// its key: the row's code and peer address are in its `pairing`),
    /// `confirmed`, `contested` (a used token came back with another key);
    /// or `resync` (with `key_id` 0) when the event stream could not deliver
    /// every frame: refetch the keys.
    pub what: String,
}

/// One key on the Keys & policy table. `GET /api/usage/keys`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KeyRow {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    /// `key` | `internal` | `agent` | `owner` | `device` — an internal
    /// identity is budgetable but can never authenticate anything; an owner
    /// key holds every capability; a device is a paired client app.
    pub kind: String,
    pub scope_mode: String,
    pub scope_patterns: String,
    /// `all` | `allow` | `deny` over exposed MCP tool names — which tools the
    /// key sees on `/mcp` and may attach in a `/v1/responses` run. Only a
    /// client key's is read: an agent's tools are its manifest's, and an owner
    /// key is not scoped.
    pub tool_scope_mode: String,
    pub tool_scope_patterns: String,
    /// Client and device keys: the device-hosted labels whose tools the key
    /// reaches (`["kai"]`), whatever its tool scope's mode, which then
    /// filters those tools as any server's. A label no device hosts now is
    /// kept and reaches nothing until one does: compare it with the device
    /// rows' `hosts_label`. Empty for every other kind.
    // The client-apps design record, L16 as changed 2026-10-15.
    pub device_tools: Vec<String>,
    pub budget_micro: i64,
    pub budget_period: String,
    pub rpm_limit: i64,
    pub tpm_limit: i64,
    pub concurrency_limit: i64,
    pub expires_at: Option<String>,
    pub note: String,
    /// Spend so far in the current budget period.
    pub spent_micro: i64,
    /// What that figure does **not** cover — the same obligation every other
    /// money total on this plane carries. Without it the per-key meter would be
    /// the one number on the page that cannot state its own remainder.
    pub spent_unknown_requests: i64,
    pub spent_unknown_tokens: i64,
    pub requests: i64,
    pub last_used: Option<String>,
    /// Devices only: when one of its long-lived connections (the Chat
    /// feed, a voice session, its tool link) last opened or closed,
    /// RFC 3339 UTC.
    // The client-apps design record, §1.6.
    pub last_seen_at: Option<String>,
    /// Devices only: the label it may host MCP tools under; `null` without
    /// a hosting grant.
    // The client-apps design record, §1.5.
    pub hosts_label: Option<String>,
    /// Devices with a hosting grant only: every principal that reaches the
    /// hosted label's tools (client-apps design §5.6, L16 as changed
    /// 2026-10-15) — `owner`, the device itself, each key and device whose
    /// `device_tools` grants the label, and each agent whose manifest names
    /// it — as a sentence each ("owner", "device 'desktop' (hosts it)", "key
    /// 'ci'", "agent 'board'"). Empty without a grant.
    pub reaches: Vec<String>,
    /// Devices only: the device's level of lmgw's admin tools — `off`,
    /// `read_only` (it reads lmgw's configuration and state) or `full` (it
    /// also changes it, and may register programs that run on the gateway's
    /// machine). Above `off` it sees and uses the Chat threads and folders
    /// that carry the self-admin toolset (`lmgw`); at `full` it may also
    /// attach the toolset and change such threads and folders. The
    /// self-admin level in Settings caps what the tools may do. Admin Chat
    /// stays hidden from every device. `off` for every other kind.
    // The client-apps design record, L3/L5 (2026-10-07); a level since the
    // pre-merge review's P-3.
    pub self_admin: crate::AdminLevel,
    /// Devices only: which of its connections are open now — `feed`,
    /// `voice`, `tools`, in that order. Empty means offline.
    pub online: Vec<String>,
    /// Devices only: how many connections of each kind in `online` are
    /// open, in the same order. More than one of a kind is a client that
    /// opened another before its last one closed.
    pub open_links: Vec<OpenLinks>,
    /// Devices only: its pairing over the device link (device link design
    /// §3, §9.3) — its token, its enrolment and the code to confirm, a
    /// contested token, its certificate's dates and its URLs. `null` for
    /// every other kind.
    pub pairing: Option<crate::device_link::DevicePairingView>,
}

/// How many connections of one kind a device holds open.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OpenLinks {
    /// `feed`, `voice` or `tools`.
    pub kind: String,
    pub count: u32,
    /// Each of them, the oldest first (device link design §9.3): "feed
    /// (keep-alive 120 s, since 09:00, from 192.0.2.2)".
    pub links: Vec<OpenLink>,
}

/// One open connection of a device.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OpenLink {
    /// When it opened (RFC 3339 UTC).
    pub since: String,
    /// The address it connected from (IP and port); `null` when lmgw did
    /// not see one.
    pub peer: Option<String>,
    /// The keep-alive it asked for, in seconds: a feed's `keepalive_s`;
    /// `null` for the other kinds.
    pub keepalive_s: Option<u32>,
    /// A voice link that is a WebRTC call (`POST /v1/realtime/calls`): the
    /// call's id, as the status block's `webrtc.calls` lists it; `null` for
    /// a WebSocket session and for the other kinds.
    pub call: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KeysResponse {
    pub keys: Vec<KeyRow>,
    pub currency: String,
    /// The global budget and what has gone against it this period.
    pub global_budget_micro: i64,
    pub global_spent_micro: i64,
    /// `day` | `month` | `total` — what "this period" means, so a projection
    /// to the period's end can aim at the right horizon instead of assuming a
    /// calendar month.
    pub global_budget_period: String,
    pub global_spent_unknown_requests: i64,
    /// *Require API key* (Settings → Network & access): off, an anonymous
    /// `/v1` call is served — what the Devices card warns of, since a
    /// device's scope and budget then bound only its own key.
    pub auth_enabled: bool,
}

/// One price sheet. `GET /api/usage/prices`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PriceRowView {
    pub id: i64,
    pub scope_kind: String,
    pub scope_key: String,
    pub unit: String,
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    pub price_cache_read: Option<f64>,
    pub price_cache_write: Option<f64>,
    pub source: String,
    pub note: Option<String>,
    pub updated_at: String,
    /// The rate of a unit other than tokens, per that unit's scale (per
    /// minute of input audio, per 1M input characters, per generated image,
    /// per answered request). `None` on a `per_mtok` row, whose rates are the
    /// four above; those four are `None` on every other row.
    pub price: Option<f64>,
}

/// One model that resolves to no price at all — an entry on the "why is my
/// spend incomplete" worklist.
///
/// `requests` is what it has already cost that nobody can name, from the
/// rollup. A configured alias that has never been called is still listed, at
/// `0`: it is about to be the same problem.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UnpricedModel {
    pub name: String,
    pub requests: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PricesResponse {
    pub prices: Vec<PriceRowView>,
    pub currency: String,
    /// Every model the gateway serves that no price sheet covers — **including
    /// the passthrough models an `expose_all` upstream serves**, which are the
    /// ones a deployment with no configured aliases actually uses. Ordered by the
    /// requests each has already spent unpriced.
    pub unpriced_models: Vec<UnpricedModel>,
}

/// `GET /api/usage/errors` — refusals and errors by kind.
///
/// Read from the **raw** request rows, so its range is the log-retention
/// window rather than the full rollup history. The response says so; a chart
/// that silently showed 30 days where every other chart on the page shows 12
/// months would read as "the errors stopped".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageErrorsResponse {
    pub buckets: Vec<String>,
    /// One entry per distinct kind, ordered by total descending.
    pub kinds: Vec<ErrorKindSeries>,
    /// How far back the raw rows actually go — the honest x-axis of this card.
    pub retention_days: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ErrorKindSeries {
    pub kind: String,
    /// True when this kind is lmgw refusing, rather than an upstream or a
    /// client failing — the two belong in different colours.
    pub refusal: bool,
    pub total: i64,
    /// Aligned with `buckets`.
    pub counts: Vec<i64>,
}

/// A write to one key's policy: `POST /api/op/key_set`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KeySaved {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The key's `api_keys.id`.
    pub id: i64,
    /// The key's name.
    pub name: String,
    /// The fields the call changed (`enabled`, `scope_mode`, `budget_micro`,
    /// …), in the order the op checks them; empty when every field sent
    /// already read that way and nothing was written.
    pub changed: Vec<String>,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/key_create` for a client or an owner key: the new row, and
/// its key in the clear.
///
/// A device key answers [`DevicePaired`] instead. The wrapper
/// [`KeyCreateAnswer`] is the document's description of the one op; a reader
/// knows the `kind` it sent and reads that kind's type.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KeyCreated {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The new key's `api_keys.id`.
    pub id: i64,
    /// The key's name; an owner key's carries the `owner:` prefix the server
    /// writes.
    pub name: String,
    /// The key. Secret. A client key's is shown here and nowhere else (only
    /// its hash is stored); an owner key's stays readable with `key_reveal`.
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::secret))]
    pub key: String,
    /// What was done, for a person to read.
    pub message: String,
}

impl std::fmt::Debug for KeyCreated {
    /// Everything but the key, which a log line must not carry.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyCreated")
            .field("ok", &self.ok)
            .field("id", &self.id)
            .field("name", &self.name)
            .field("key", &"<redacted>")
            .field("message", &self.message)
            .finish()
    }
}

/// An owner key handed out: `POST /api/op/key_reveal` (its stored plaintext)
/// and `key_rotate` (the new one). Both refuse a device row: it holds no
/// secret of lmgw's making, and pairs again with `device_token`.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KeySecret {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The key's `api_keys.id`.
    pub id: i64,
    /// The key's name.
    pub name: String,
    /// The key, in the clear. Secret.
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::secret))]
    pub key: String,
    /// What was done, for a person to read; after a rotation it also says when
    /// the configuration could not be reloaded.
    pub message: String,
}

impl std::fmt::Debug for KeySecret {
    /// Everything but the key, which a log line must not carry.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeySecret")
            .field("ok", &self.ok)
            .field("id", &self.id)
            .field("name", &self.name)
            .field("key", &"<redacted>")
            .field("message", &self.message)
            .finish()
    }
}

/// A device's pairing link minted: `POST /api/op/key_create` with `kind:
/// device` (the row and its first pairing token) and `device_token` (a new
/// token for the same row; device link design §3.2, §5.2). The token goes to
/// the owner once, inside the link.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DevicePaired {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The device row's `api_keys.id`; a new token keeps it.
    pub id: i64,
    /// `device:<name>`.
    pub name: String,
    /// `lmgw-pair:?v=2&name=…&url=…[&url=…]&fp=sha256:…&token=lmgw-enrol-…`:
    /// carries the one-time pairing token to the device. Secret while its
    /// token lives: shown once, never logged.
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::secret))]
    pub link: String,
    /// The device's lmgw URLs: `https` base URLs of the device listener, in
    /// the owner's order, as the link carries them and enrolment answers
    /// them.
    pub urls: Vec<String>,
    /// Paired as "this computer": the device reaches lmgw on a loopback
    /// address only.
    pub local_only: bool,
    /// When the pairing token lapses (RFC 3339, UTC): `devices.token_minutes`
    /// after it was minted.
    pub token_expires_at: String,
    /// `sha256:` and the hex SHA-256 of the device CA's SubjectPublicKeyInfo:
    /// the link's `fp`, which the device's enrolment pins.
    pub fp: String,
    /// What was done, for a person to read: what the device does next, and
    /// whether the device listener reaches it from where it is.
    pub message: String,
}

impl std::fmt::Debug for DevicePaired {
    /// Everything but the link, which carries the token and a log line must
    /// not.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevicePaired")
            .field("ok", &self.ok)
            .field("id", &self.id)
            .field("name", &self.name)
            .field("link", &"<redacted>")
            .field("urls", &self.urls)
            .field("local_only", &self.local_only)
            .field("token_expires_at", &self.token_expires_at)
            .field("fp", &self.fp)
            .field("message", &self.message)
            .finish()
    }
}

/// What `key_create` answers, by `kind`: [`DevicePaired`] for `device`,
/// [`KeyCreated`] for a client or an owner key.
///
/// The two shapes share only `ok`, `id`, `name` and `message`, so this is the
/// document's description of the one op, not a type to read an answer into: a
/// reader knows the kind it sent and reads that kind's type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum KeyCreateAnswer {
    /// `kind: client` (or `key`, or none) and `kind: owner`.
    Key(KeyCreated),
    /// `kind: device`.
    Device(DevicePaired),
}

/// `POST /api/op/device_token_end`: a device's pairing token ended (device
/// link design §3.3), or said to be none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceTokenEnded {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The device row's `api_keys.id`.
    pub id: i64,
    /// `device:<name>`.
    pub name: String,
    /// Whether a token was alive and is ended now; `false` when none was
    /// (never issued, lapsed, or ended already).
    pub ended: bool,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/device_confirm`: the owner confirmed a device's enrolment
/// (device link design §3.5); its next request is served.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceConfirmed {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The device row's `api_keys.id`.
    pub id: i64,
    /// `device:<name>`.
    pub name: String,
    /// The confirmation code of the key confirmed, `7F3A-91C2`: the one the
    /// device shows, and the one the call sent.
    pub confirm_code: String,
    /// Whether the confirmed key replaced one the device kept until now (a
    /// re-pair that kept the current key, §5.2): the kept key ended, and
    /// what it held open closed.
    pub replaced: bool,
    /// What was done, for a person to read.
    pub message: String,
}

/// `POST /api/op/device_ca_rotate`: lmgw's device CA replaced (device link
/// design §5.5); every enrolled device was reset to wait for a new token.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceCaRotated {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The new CA's pin: `sha256:` and the hex SHA-256 of its
    /// SubjectPublicKeyInfo, what every new pairing link carries.
    pub fp: String,
    /// The devices reset, by name without `device:`: each lost its key and
    /// its certificate and waits for **New token…**.
    pub devices_reset: Vec<String>,
    /// What was done, for a person to read.
    pub message: String,
}

/// A write to one price sheet: `POST /api/op/price_set`. It names the row the
/// call wrote, not the rates it carried.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PriceSaved {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// `alias` or `upstream_model`: what the row prices.
    pub scope_kind: String,
    /// The alias name, or `<upstream_id>:<upstream_model_id>`; trimmed.
    pub scope_key: String,
    /// The row's billable unit (`per_mtok` when the call named none).
    pub unit: String,
    /// Always `manual`: the op writes no other source.
    pub source: String,
}

/// What a price sync did to one upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// every other upstream's prices from refreshing. `null` when it was
    /// asked.
    pub error: Option<String>,
}

/// Summary of one price sync run: `POST /api/op/prices_sync`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PriceSyncSummary {
    pub rows_written: usize,
    /// The upstreams' `rows_removed`, summed.
    pub rows_removed: usize,
    pub unpriced_models: usize,
    /// The upstreams' `not_synced`, summed per field.
    pub not_synced: BTreeMap<String, usize>,
    pub upstreams: Vec<UpstreamPriceSync>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three answers that carry a secret print everything but it (a key,
    /// or the pairing link with its token): a `{:?}` in a log line leaks
    /// nothing.
    #[test]
    fn debug_never_prints_a_key() {
        let created = KeyCreated {
            ok: true,
            id: 7,
            name: "ci".into(),
            key: "lmgw-secret-created".into(),
            message: "done".into(),
        };
        let secret = KeySecret {
            ok: true,
            id: 8,
            name: "owner:ci".into(),
            key: "lmgw-owner-secret-key".into(),
            message: "done".into(),
        };
        let paired = DevicePaired {
            ok: true,
            id: 9,
            name: "device:phone".into(),
            link: "lmgw-pair:?v=2&name=phone&url=u&fp=f&token=lmgw-enrol-secret".into(),
            urls: vec!["https://127.0.0.1:8788".into()],
            local_only: true,
            token_expires_at: "2026-10-15T10:00:00Z".into(),
            fp: "sha256:00".into(),
            message: "paired".into(),
        };
        for (text, hidden) in [
            (format!("{created:?}"), "lmgw-secret-created"),
            (format!("{secret:?}"), "lmgw-owner-secret-key"),
            (format!("{paired:?}"), "enrol-secret"),
        ] {
            assert!(!text.contains(hidden), "{text}");
            assert!(text.contains("<redacted>"), "{text}");
        }
        assert!(format!("{paired:?}").contains("https://127.0.0.1:8788"));
    }
}
