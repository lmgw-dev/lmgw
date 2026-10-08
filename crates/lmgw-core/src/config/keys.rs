//! API keys: the redacted-on-read bearer wrapper, the stored row, and its
//! scope/budget policy.

use serde::{Deserialize, Serialize};

use super::SelfAdmin;

/// A string that must not turn up in a log line or a panic message.
///
/// `ApiKey` derives `Debug`, and an `ApiKey` ends up inside a `Snapshot` that
/// gets `{:?}`-printed by anything from a `tracing` field to a test assertion
/// to a `.expect()` — so a bare `String` here is one careless format specifier
/// away from writing an agent's live bearer to the log (final review). The
/// value is still exactly a string; only its `Debug` lies, and
/// [`expose`](Self::expose) is the deliberate gesture that gets it out.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The plaintext. Named so that reading it is visible at the call site.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() {
            "<empty>"
        } else {
            "<redacted>"
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    /// hex(sha256(key))
    pub key_hash: String,
    pub enabled: bool,
    /// `key` = a real credential; `internal` = a synthetic identity for one of
    /// the gateway's own consumers (Admin Chat, quickdoc ingest, the mail
    /// workflow …). Internal rows carry no usable hash and **must never**
    /// authenticate anything — they exist so that spend which used to log as
    /// "no key" is visible and budgetable (§4.4). `agent` = one catalog agent's
    /// own token (container-runtime §3.1): authenticable, unlike `internal`.
    /// `owner` = the dashboard's own session and the self-admin credential
    /// (principals §3.1): the only kind a cookie is accepted for.
    #[serde(default)]
    pub kind: ApiKeyKind,
    /// The same token **in plaintext**, for `kind = "agent"` and
    /// `kind = "owner"` — lmgw has to hand an agent token to a container on the
    /// *second* run too, and an owner key to the login link and the Keys page,
    /// so "shown once at creation" is not available for either
    /// (container-runtime §3.1, principals §3.12). Never serialized: the value
    /// leaves the process through `agent_token_get`, the run's secrets file and
    /// `key_reveal`, and through nothing else.
    #[serde(default, skip_serializing)]
    pub key_plain: Option<Secret>,
    /// The catalog row this token belongs to, for `kind = "agent"` only.
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub policy: KeyPolicy,
    #[serde(default)]
    pub note: String,
    /// The label a **device** may host MCP tools under (client-apps design
    /// §1.5); `None` on every other kind, which the table's CHECK states.
    #[serde(default)]
    pub hosts_label: Option<String>,
    /// A **device**'s level of lmgw's admin tools (client-apps design L3/L5,
    /// 2026-10-07; a level since the pre-merge review's P-3): above `off` it
    /// sees and uses the Chat threads and folders that carry the self-admin
    /// toolset and its tool scope takes in the `lmgw` label; at `full` (after
    /// the cap) it may attach the toolset too. What the tools may do is this
    /// level capped by the gateway's own ([`DeviceAdmin::capped`]). Off by default; the owner's to set; `off` on
    /// every other kind, which the table's CHECK states.
    #[serde(default)]
    pub self_admin: DeviceAdmin,
}

/// A device's level of lmgw's admin tools (`ApiKey::self_admin`): stored as
/// `0`, `1` and `2` (migration 0068), said as `off`, `read_only` and `full`.
/// Its own type, not [`SelfAdmin`], whose default is the gateway's
/// `read_only`: a device's is `off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceAdmin {
    /// No admin tools, and none of the threads and folders with the toolset.
    #[default]
    Off,
    /// The read tools: lmgw's configuration and state.
    ReadOnly,
    /// The read and the write tools — among them the ones that register
    /// programs this machine runs as the lmgw user.
    Full,
}

impl DeviceAdmin {
    /// The level a stored column holds; anything but `1` and `2` is `off`
    /// (the table's triggers allow nothing else).
    pub fn from_column(v: i64) -> Self {
        match v {
            1 => Self::ReadOnly,
            2 => Self::Full,
            _ => Self::Off,
        }
    }

    /// The column's value.
    pub fn column(self) -> i64 {
        match self {
            Self::Off => 0,
            Self::ReadOnly => 1,
            Self::Full => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ReadOnly => "read_only",
            Self::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "off" => Some(Self::Off),
            "read_only" => Some(Self::ReadOnly),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// Above `off`: the device reaches the toolset's threads and folders and
    /// the `lmgw` label.
    pub fn is_on(self) -> bool {
        self != Self::Off
    }

    /// The same level as the gateway's own vocabulary.
    pub fn as_self_admin(self) -> SelfAdmin {
        match self {
            Self::Off => SelfAdmin::Off,
            Self::ReadOnly => SelfAdmin::ReadOnly,
            Self::Full => SelfAdmin::Full,
        }
    }

    /// What the device's admin tools may do: this level capped by the
    /// gateway's `global` one — a device at `full` under a global
    /// `read_only` reads only.
    pub fn capped(self, global: SelfAdmin) -> SelfAdmin {
        self.as_self_admin().min(global)
    }

    /// A level the wire carried (`key_create`, `key_set`); `None` for one
    /// this build does not know. A known word with spaces around it reads
    /// as the word, as it always did.
    pub fn from_wire(level: &lmgw_api_types::AdminLevel) -> Option<Self> {
        Self::parse(level.as_str())
    }
}

impl From<DeviceAdmin> for lmgw_api_types::AdminLevel {
    fn from(level: DeviceAdmin) -> Self {
        match level {
            DeviceAdmin::Off => Self::Off,
            DeviceAdmin::ReadOnly => Self::ReadOnly,
            DeviceAdmin::Full => Self::Full,
        }
    }
}

impl ApiKey {
    /// How a refusal names the key: "device 'phone'" for a device (its name
    /// without `device:`, as every device message names it), "key 'laptop'"
    /// for every other kind — the `/v1` refusals' words, the Chat's, a
    /// realtime upgrade's and a mid-turn one's alike (review W2-18).
    pub fn described(&self) -> String {
        match self.kind {
            ApiKeyKind::Device => {
                format!("device '{}'", crate::devices::short_name(&self.name))
            }
            _ => format!("key '{}'", self.name),
        }
    }

    /// Which credential the row holds now: the head of its hash, which a
    /// Rotate changes (client-apps design §1.6, review W2-2). A principal
    /// carries the one it was resolved with, so a request that outlives a
    /// Rotate — a turn, a stream, a session — is told apart from one made
    /// with the new key. Not a secret: a hash of a 256-bit key, cut short.
    pub fn fingerprint(&self) -> String {
        self.key_hash.chars().take(16).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyKind {
    #[default]
    Key,
    Internal,
    /// One catalog agent's token (container-runtime §3.1). Authenticable —
    /// the invariant `Internal` carries ("internal identities never
    /// authenticate") is about `Internal` alone.
    Agent,
    /// An owner credential (principals §3.1): `owner:dashboard`, the session
    /// the shell and the browser present, and `owner:self-admin`, the one
    /// `/mcp/admin` accepts. Holds every capability, and is the only kind the
    /// `lmgw_session` cookie is honoured for.
    Owner,
    /// One paired client app (client-apps design §1.1): `device:<name>`,
    /// hash-only like a client key, holding `Chat` on top of a client key's
    /// `Inference` and never anything of the admin plane.
    Device,
}

impl ApiKeyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Internal => "internal",
            Self::Agent => "agent",
            Self::Owner => "owner",
            Self::Device => "device",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "internal" => Self::Internal,
            "agent" => Self::Agent,
            "owner" => Self::Owner,
            "device" => Self::Device,
            _ => Self::Key,
        }
    }
}

/// What a key is allowed to do (usage-analytics design §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct KeyPolicy {
    pub scope_mode: ScopeMode,
    /// Newline-delimited globs over alias names — the same list syntax the
    /// dashboard's textareas and the self-admin tools already use.
    pub scope_patterns: String,
    /// Which MCP tools the key sees and may call, on `/mcp` and in a
    /// `/v1/responses` run — the same three modes, over exposed tool names.
    #[serde(default)]
    pub tool_scope_mode: ScopeMode,
    #[serde(default)]
    pub tool_scope_patterns: String,
    /// Currency micro-units. `0` = no budget.
    pub budget_micro: i64,
    pub budget_period: BudgetPeriod,
    /// `0` = no limit, for all three.
    pub rpm_limit: i64,
    pub tpm_limit: i64,
    pub concurrency_limit: i64,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScopeMode {
    #[default]
    All,
    Allow,
    Deny,
}

impl ScopeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            _ => Self::All,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod {
    Day,
    #[default]
    Month,
    Total,
}

impl BudgetPeriod {
    /// First UTC hour key of the period that contains `now`, for the rollup
    /// query that answers "spent so far".
    ///
    /// Deliberately UTC, matching the buckets: a budget that resets at local
    /// midnight would need the viewer's offset, and a *refusal* must not depend
    /// on which browser last looked at the dashboard.
    pub fn start_hour_key(self, now: chrono::DateTime<chrono::Utc>) -> String {
        use chrono::Datelike;
        match self {
            Self::Day => now.format("%Y-%m-%dT00").to_string(),
            Self::Month => format!("{:04}-{:02}-01T00", now.year(), now.month()),
            // Every bucket key sorts after this one.
            Self::Total => "0000-01-01T00".to_string(),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Month => "month",
            Self::Total => "total",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "day" => Self::Day,
            "total" => Self::Total,
            _ => Self::Month,
        }
    }
}

impl KeyPolicy {
    /// Does this key's scope admit `alias`?
    ///
    /// Patterns are shell-style globs, matched case-insensitively because
    /// aliases resolve case-insensitively everywhere else.
    pub fn admits(&self, alias: &str) -> bool {
        match self.scope_mode {
            ScopeMode::All => true,
            ScopeMode::Allow => self.matches_any(alias),
            ScopeMode::Deny => !self.matches_any(alias),
        }
    }

    fn matches_any(&self, alias: &str) -> bool {
        lmgw_api_types::scope::patterns_match(&self.scope_patterns, alias)
    }

    /// Does this key's **tool** scope admit the exposed tool `name`?
    ///
    /// The same globs and the same case-folding as [`admits`](Self::admits),
    /// over the names a client sees on `/mcp` (`github__search`, `docs__*`).
    /// Only a client key's own list — an agent's tools come from its manifest
    /// and an owner key is not scoped (see `mcp::scope`).
    pub fn admits_tool(&self, name: &str) -> bool {
        match self.tool_scope_mode {
            ScopeMode::All => true,
            ScopeMode::Allow => {
                lmgw_api_types::scope::patterns_match(&self.tool_scope_patterns, name)
            }
            ScopeMode::Deny => {
                !lmgw_api_types::scope::patterns_match(&self.tool_scope_patterns, name)
            }
        }
    }
}

/// hex(sha256(key)) — the only form gateway keys are stored in.
pub fn hash_api_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(key.as_bytes()))
}
