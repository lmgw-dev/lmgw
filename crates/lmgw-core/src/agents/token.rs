//! The agent token (container-runtime design §3.1).
//!
//! **One token per agent, not per run.** A run is a job row that may be
//! retried, reopened and re-applied; a credential whose lifetime is a job is a
//! credential that has to be minted, delivered and reaped four times for one
//! mailbox. The agent is the thing that has an allow list and a budget, so the
//! agent is the thing that has a key.
//!
//! It is a third [`ApiKeyKind`] variant rather than a new table, because
//! `api_keys` already carries everything a scoped, budgeted, revocable
//! credential needs — the policy columns, the `request_logs.key_id` link, the
//! hourly rollup and the dashboard's key page. A separate table would
//! re-implement all five.
//!
//! **What the token now is:** a principal (principals design §3.1). Every
//! route declares a capability and an agent token holds `Inference`, `Ledger`
//! and `AgentSelf` — so a container calling `/api/op/*` is refused rather than
//! reaching it the way any other local process once could. What it buys on top
//! of that is unchanged: the `/mcp` allow-list, the model-alias scope, the
//! budget and rate limits, and per-agent attribution in Logs.

use crate::config::{ApiKeyKind, ScopeMode};
use crate::state::SharedState;
use crate::store;

use super::manifest::Format;
use super::Agent;

/// The calling agent: the `agent` half of the principal `server::principal_mw`
/// resolved at the router root (§3.1, principals §3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    pub agent_id: String,
    /// `api_keys.id`, so a handler that needs the identity does not re-query.
    pub key_id: i64,
    /// `agent:<id>` — the name Logs and the usage page print.
    pub key_name: String,
}

/// The `api_keys.name` one agent's token carries. Reads like `internal:agents`
/// does in Logs and on the usage page.
pub fn key_name(agent_id: &str) -> String {
    format!("agent:{agent_id}")
}

/// A fresh token: 32 random bytes, hex-rendered behind a recognisable prefix.
///
/// Hex rather than the design's base64url only because it needs no new
/// dependency (`hex` is already here, no base64 crate is); the 256 bits the
/// design asks for are the part that matters and they are unchanged.
fn mint() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("lmgw-agent-{}", hex::encode(bytes))
}

/// The scope one agent's token gets right now (§3.1).
///
/// `scope_patterns` is the set of current values of every `format:
/// "model_alias"` config field, plus a literal `model.alias` when it is not a
/// template. **Recomputed**, at each run start and in `agent_config_set`,
/// because a model picker is exactly the field an owner changes between runs.
///
/// If the manifest declares no model-alias field, or every one of them is empty
/// — which is today's shipped mail labeler, whose `model` field has no
/// `default` — the policy is [`ScopeMode::All`] and the page says *"token: any
/// model"*. A derived allow-list that silently came out empty would refuse
/// every call the agent makes, which is the failure this rule exists to avoid.
pub fn derive_scope(agent: &Agent) -> (ScopeMode, String) {
    let mut patterns: Vec<String> = Vec::new();
    let mut push = |v: &str| {
        let v = v.trim();
        if !v.is_empty() && !patterns.iter().any(|p| p == v) {
            patterns.push(v.to_string());
        }
    };

    let config = agent.effective_config();
    if let Ok(fields) = agent.manifest.fields() {
        for f in fields
            .iter()
            .filter(|f| f.format == Some(Format::ModelAlias))
        {
            if let Some(v) = config.get(&f.name).and_then(serde_json::Value::as_str) {
                push(v);
            }
        }
    }
    // A templated `model.alias` names a config field, not an alias; the loop
    // above has already contributed whatever that field currently holds.
    let alias = &agent.manifest.model.alias;
    if !alias.contains("{{") {
        push(alias);
    }

    if patterns.is_empty() {
        (ScopeMode::All, String::new())
    } else {
        (ScopeMode::Allow, patterns.join("\n"))
    }
}

/// What the Definition tab prints next to **Copy token**, in words rather than
/// a mode name.
pub fn scope_note(mode: ScopeMode, patterns: &str) -> String {
    match mode {
        ScopeMode::All => "token: any model".to_string(),
        _ => format!("token: {}", patterns.replace('\n', ", ")),
    }
}

/// The agent's token, minting it if this is the first time (§3.1: created on
/// first Start or on demand from the UI, never at import — an agent that is
/// never run never mints a credential). The scope is recomputed on the way
/// through, so the value handed out is always scoped to the config as it is
/// now.
pub async fn ensure(state: &SharedState, agent: &Agent) -> Result<String, String> {
    let id = &agent.row.id;
    let existing = store::agent_key(&state.db, id)
        .await
        .map_err(|e| e.to_string())?;
    let plaintext = match existing {
        Some((_, plain)) => plain,
        None => {
            let plain = mint();
            store::upsert_agent_key(
                &state.db,
                id,
                &key_name(id),
                &crate::config::hash_api_key(&plain),
                &plain,
                agent.row.enabled,
            )
            .await
            .map_err(|e| e.to_string())?;
            plain
        }
    };
    write_scope(state, agent).await?;
    Ok(plaintext)
}

/// A new token for the same agent: both columns rewritten in one write, the
/// scope recomputed, and the old value dead the moment the snapshot reloads.
///
/// Returns `(token, warning)`. **Nothing after the write can turn this into an
/// `Err`** (final review): the upsert is what kills the old token, so by the
/// time the scope write or the snapshot reload can fail the rotation has
/// already happened and the caller is holding the only copy of the value the
/// gateway now expects. Answering "rotation failed" there would throw that copy
/// away and leave the owner with an agent whose token nobody knows — so a
/// failure past the commit is a sentence handed back with the token, not a
/// refusal.
pub async fn rotate(
    state: &SharedState,
    agent: &Agent,
) -> Result<(String, Option<String>), String> {
    let id = &agent.row.id;
    let plain = mint();
    store::upsert_agent_key(
        &state.db,
        id,
        &key_name(id),
        &crate::config::hash_api_key(&plain),
        &plain,
        agent.row.enabled,
    )
    .await
    .map_err(|e| e.to_string())?;
    let warning = write_scope(state, agent).await.err().map(|e| {
        tracing::warn!("agent '{id}': the rotated token was stored but not re-scoped: {e}");
        format!(
            "the new token is stored and is the one this gateway accepts, but its scope could \
             not be rewritten ({e}). It is carrying the scope the previous token had; saving the \
             config or restarting lmgw re-derives it."
        )
    });
    Ok((plain, warning))
}

/// Recompute and store the derived scope, then reload the snapshot so the
/// change is live on the next request. A no-op against an agent that has no
/// token yet: there is nothing to scope.
pub async fn recompute_scope(state: &SharedState, agent: &Agent) -> Result<(), String> {
    if store::agent_key(&state.db, &agent.row.id)
        .await
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Ok(());
    }
    write_scope(state, agent).await
}

/// Re-derive scope and the enabled mirror for an agent named by id, from
/// whatever the catalog holds right now.
///
/// The one call every write path uses — `agent_set`, the import endpoint,
/// `agent_reset`, `agent_enable`, `agent_config_set`, a run start — so the
/// token's terms cannot lag the row that determines them. Silent when the
/// agent has no token yet (nothing to scope) and when its manifest is one this
/// build cannot read (the scope it already has is a better answer than an
/// empty one).
pub async fn resync(state: &SharedState, agent_id: &str) -> Result<(), String> {
    let Some(row) = store::get_agent(&state.db, agent_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    let enabled = row.enabled;
    match Agent::from_row(row) {
        Ok(agent) => recompute_scope(state, &agent).await,
        Err(_) => {
            store::set_agent_key_enabled(&state.db, agent_id, enabled)
                .await
                .map_err(|e| e.to_string())?;
            state.reload_snapshot().await.map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

async fn write_scope(state: &SharedState, agent: &Agent) -> Result<(), String> {
    let (mode, patterns) = derive_scope(agent);
    store::set_agent_key_scope(&state.db, &agent.row.id, mode.as_str(), &patterns)
        .await
        .map_err(|e| e.to_string())?;
    // Same trip: every path that re-scopes also re-syncs the kill switch, so
    // the two cannot drift when an agent is toggled and edited in either order.
    store::set_agent_key_enabled(&state.db, &agent.row.id, agent.row.enabled)
        .await
        .map_err(|e| e.to_string())?;
    state.reload_snapshot().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// What an agent token reads as once it has been taken out of a log line.
pub const REDACTED: &str = "<agent token>";

/// This agent's token in plaintext, from the snapshot rather than the DB.
///
/// The snapshot already holds every `api_keys` row, so the redaction below
/// costs no query — which matters, because it runs on the path that renders a
/// log tail.
pub fn plaintext_of(snap: &crate::config::Snapshot, agent_id: &str) -> Option<String> {
    snap.api_keys
        .iter()
        .find(|k| k.kind == ApiKeyKind::Agent && k.agent_id.as_deref() == Some(agent_id))
        .and_then(|k| k.key_plain.as_ref())
        .map(|s| s.expose().to_string())
}

/// Take this agent's token out of text that is about to be stored or returned
/// (§3.1, final review).
///
/// §3.1 always said the token can end up in a run log — a container that
/// `cat`s its own `/lmgw/secrets.json` puts it there itself. What made that
/// more than a documented escape hatch is *where the text goes*: a service
/// container's `podman logs` tail is rendered into `AgentDetail.service`, into
/// `lmgw__agent_get`, and into the body of the 503 a failed start returns — so
/// one debug `cat` in an image would hand the agent's live bearer to anything
/// that can read a page or call a tool.
///
/// A substring replace, deliberately: the token is a single opaque string with
/// no structure to parse, and it leaks by being *printed*, whatever quoting or
/// JSON escaping surrounds it. It does **not** cover a container that prints
/// the token in pieces, base64s it, or sends it somewhere else entirely —
/// nothing inside the container is lmgw's to police. What it covers is every
/// path by which lmgw itself would republish it.
pub fn redact(secret: Option<&str>, text: &str) -> String {
    match secret {
        Some(s) if !s.is_empty() && text.contains(s) => text.replace(s, REDACTED),
        _ => text.to_string(),
    }
}

/// What a presented bearer turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presented {
    /// Not an agent token at all — a gateway key, or nothing this build knows.
    /// The caller falls through to whatever it did before agent tokens existed.
    Other,
    /// An agent token whose agent is switched off. **Disable is the kill
    /// switch**: the token stops working everywhere, and the refusal says so
    /// rather than reading as a bad credential.
    Disabled(String),
    Agent(AgentIdentity),
}

/// Resolve a presented bearer to the agent behind it.
///
/// The hash is still the lookup key, so [`Snapshot::verify_api_key`] is
/// untouched — `key_plain` exists to hand the token *out*, never to find it.
/// The **disabled** case is carried rather than folded into `Other`, because
/// `verify_api_key` already drops a row with `enabled = 0` and a caller that
/// could not tell the two apart would answer "invalid token" to an owner who
/// merely flipped a switch.
///
/// [`Snapshot::verify_api_key`]: crate::config::Snapshot::verify_api_key
pub fn resolve(snap: &crate::config::Snapshot, presented: &str) -> Presented {
    if let Some(key) = snap.verify_api_key(presented) {
        if key.kind != ApiKeyKind::Agent {
            return Presented::Other;
        }
        let Some(agent_id) = key.agent_id.clone() else {
            return Presented::Other;
        };
        return Presented::Agent(AgentIdentity {
            agent_id,
            key_id: key.id,
            key_name: key.name.clone(),
        });
    }
    // Not admitted by `verify_api_key`, which filters on `enabled`. A hash that
    // matches a *disabled* agent row is the one case worth naming.
    let hash = crate::config::hash_api_key(presented);
    match snap
        .api_keys
        .iter()
        .find(|k| !k.enabled && k.kind == ApiKeyKind::Agent && k.key_hash == hash)
    {
        Some(k) => Presented::Disabled(k.agent_id.clone().unwrap_or_default()),
        None => Presented::Other,
    }
}

/// The bearer on a request, in either of the two spellings `/v1` accepts.
pub fn presented(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
}

// ---------------------------------------------------------------------------
// Owner keys (principals design §3.1, §6)
// ---------------------------------------------------------------------------
//
// Here rather than in a module of their own because an owner key is the same
// kind of thing an agent token is — an `api_keys` row with a plaintext lmgw
// has to hand back — and because `presented` and `resolve` above are what the
// resolver in `crate::principal` builds on.

/// The session the browser, the shell and the login link present (§3.1).
///
/// **The door**: it cannot be disabled or deleted, only rotated, because a
/// gateway whose only owner credential is switched off has no way left to
/// switch it back on.
pub const OWNER_DASHBOARD: &str = "owner:dashboard";

/// The credential `POST /mcp/admin` accepts (§3.7). Disabling *this* row is
/// how "the self-admin plane is closed" is said now that the Settings string
/// is gone — a closed surface and a locked one were two words for one thing.
pub const OWNER_SELF_ADMIN: &str = "owner:self-admin";

/// A fresh owner key: 32 random bytes in hex behind its own prefix.
///
/// The prefix is the point — `lmgw-owner-…` beside `lmgw-agent-…` and a client
/// key's `lmgw-…`, so a string found in a config file or a log says which of
/// the three it is (§3.12). Hex for [`mint`]'s reason: it needs no new
/// dependency, and the 256 bits are the part that matters.
pub fn mint_owner() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("lmgw-owner-{}", hex::encode(bytes))
}

/// Create or rewrite one owner key, and publish it: both credential columns in
/// one write, then a snapshot reload so the new value is live on the next
/// request and the old one is dead.
///
/// The primitive behind rotation and behind a test that wants a known owner
/// credential. Returns the row id.
pub async fn set_owner_key(
    state: &SharedState,
    name: &str,
    plaintext: &str,
    enabled: bool,
) -> Result<i64, String> {
    let id = store::upsert_owner_key(
        &state.db,
        name,
        &crate::config::hash_api_key(plaintext),
        plaintext,
        enabled,
    )
    .await
    .map_err(|e| e.to_string())?;
    state.reload_snapshot().await.map_err(|e| e.to_string())?;
    Ok(id)
}

/// Seed the two owner rows, once per install (§6).
///
/// Called from `AppState::init` **before** `service::resync_all` and before
/// anything can serve a request, and ending with a snapshot reload so that the
/// snapshot `init` returns already carries the rows — the shell's window, the
/// login link and the first request all read it.
///
/// Idempotent by insert-or-nothing rather than by read-then-write: see
/// [`store::insert_owner_key`]. A failure here is fatal to startup on purpose.
/// The dashboard key is the only way into the `Admin` plane, so a gateway that
/// could not write it would come up reachable and permanently locked, which is
/// worse than not coming up.
pub async fn seed_owner_keys(state: &SharedState) -> anyhow::Result<()> {
    let plain = mint_owner();
    if store::insert_owner_key(
        &state.db,
        OWNER_DASHBOARD,
        &crate::config::hash_api_key(&plain),
        &plain,
        true,
    )
    .await?
    .is_some()
    {
        tracing::info!("minted the dashboard key ('{OWNER_DASHBOARD}')");
    }

    // The one-release migration of `Settings::self_admin_token` (§3.7): a
    // token that is set today becomes the plaintext of this row, so the MCP
    // client config presenting it keeps working without an edit, and a blank
    // one — today's "the plane is closed" — becomes a row that exists but is
    // switched off. Either way the meaning the owner configured survives.
    let carried = state
        .snapshot()
        .settings
        .self_admin_token
        .trim()
        .to_string();
    let plain = if carried.is_empty() {
        mint_owner()
    } else {
        carried.clone()
    };
    let seeded = store::insert_owner_key(
        &state.db,
        OWNER_SELF_ADMIN,
        &crate::config::hash_api_key(&plain),
        &plain,
        !carried.is_empty(),
    )
    .await?
    .is_some();
    if seeded && !carried.is_empty() {
        // Blanked only on the start that actually carried it over, so a second
        // start cannot re-seed a row someone has since rotated. No
        // `settings_write` guard: nothing else can be writing settings yet.
        let mut settings = state.snapshot().settings.clone();
        settings.self_admin_token.clear();
        store::save_settings(&state.db, &settings).await?;
        tracing::info!(
            "moved the self-admin token into the keys table as '{OWNER_SELF_ADMIN}' — \
             rotate or disable it on Usage → Keys"
        );
    } else if seeded {
        tracing::info!(
            "minted the self-admin key ('{OWNER_SELF_ADMIN}'), disabled — /mcp/admin stays \
             closed until it is enabled on Usage → Keys"
        );
    }

    state.reload_snapshot().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::manifest;
    use crate::store::AgentRow;

    fn agent_of(manifest_text: &str, config: &str) -> Agent {
        manifest::load(manifest_text).expect("the fixture manifest parses");
        Agent::from_row(AgentRow {
            id: "a".into(),
            manifest: manifest_text.into(),
            config: config.into(),
            enabled: true,
            source: "authored".into(),
            provenance: String::new(),
            dev_url: None,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap()
    }

    fn doc(model_alias: &str, model_field: Option<&str>) -> String {
        let field = match model_field {
            Some(default) if !default.is_empty() => format!(
                r#""config": {{ "schema": {{ "type": "object", "properties": {{
                     "model": {{ "type": "string", "format": "model_alias", "default": "{default}" }} }} }} }},"#
            ),
            Some(_) => r#""config": { "schema": { "type": "object", "properties": {
                     "model": { "type": "string", "format": "model_alias" } } } },"#
                .to_string(),
            None => String::new(),
        };
        format!(
            r#"{{ "schema_version": 1, "id": "a", "name": "A",
                 "model": {{ "alias": "{model_alias}" }},
                 {field}
                 "run": {{ "kind": "batch",
                          "source": {{ "tool": "t" }},
                          "item": {{ "id": "{{{{item.id}}}}" }} }} }}"#
        )
    }

    #[test]
    fn no_model_field_with_a_value_means_any_model() {
        // Today's shipped mail labeler: `model` is a `model_alias` field with
        // no default, and `model.alias` is the template that reads it. An
        // allow-list derived from that would be empty, and an empty allow-list
        // refuses every call the agent makes.
        let a = agent_of(&doc("{{config.model}}", Some("")), "{}");
        let (mode, patterns) = derive_scope(&a);
        assert_eq!(mode, ScopeMode::All);
        assert_eq!(patterns, "");
        assert_eq!(scope_note(mode, &patterns), "token: any model");
    }

    #[test]
    fn the_current_value_of_every_model_field_is_the_scope() {
        let a = agent_of(&doc("{{config.model}}", Some("")), r#"{"model":"qwen3.8"}"#);
        let (mode, patterns) = derive_scope(&a);
        assert_eq!(mode, ScopeMode::Allow);
        assert_eq!(patterns, "qwen3.8");

        // And it follows the config, which is the whole reason it is derived:
        // a model picker is the field an owner changes between runs.
        let b = agent_of(
            &doc("{{config.model}}", Some("")),
            r#"{"model":"claude-opus-5"}"#,
        );
        assert_eq!(derive_scope(&b).1, "claude-opus-5");
    }

    #[test]
    fn a_literal_alias_scopes_itself_and_a_template_does_not() {
        let literal = agent_of(&doc("qwen3.8", None), "{}");
        assert_eq!(derive_scope(&literal), (ScopeMode::Allow, "qwen3.8".into()));

        // A default on the field and a literal alias: both, deduplicated.
        let both = agent_of(&doc("{{config.model}}", Some("gpt-5-mini")), "{}");
        assert_eq!(derive_scope(&both).1, "gpt-5-mini");
    }

    #[test]
    fn a_fresh_token_is_256_bits_and_never_repeats() {
        let a = mint();
        let b = mint();
        assert_ne!(a, b);
        assert_eq!(a.len(), "lmgw-agent-".len() + 64, "32 bytes, hex");
    }

    #[test]
    fn an_owner_key_is_256_bits_behind_its_own_prefix() {
        let a = mint_owner();
        assert_ne!(a, mint_owner());
        assert_eq!(a.len(), "lmgw-owner-".len() + 64, "32 bytes, hex");
        // The three formats stay distinguishable at a glance (§3.12).
        assert!(!a.starts_with("lmgw-agent-"));
    }

    // -----------------------------------------------------------------------
    // The startup seed (§6)
    //
    // Against `init_for_tests` rather than `AppState::init`: the real `init`
    // spawns boot container reconciliation, which shells `podman` and removes
    // every `lmgw.kind=agent` container under the default prefix — on a
    // developer's box that is their *live* agents, not the test's.
    // -----------------------------------------------------------------------

    use crate::config::{ApiKeyKind, Settings};
    use crate::state::{AppState, SharedState};

    fn owner_row(state: &SharedState, name: &str) -> (i64, String, bool) {
        let snap = state.snapshot();
        let k = snap
            .api_keys
            .iter()
            .find(|k| k.name == name)
            .unwrap_or_else(|| panic!("'{name}' is seeded"));
        assert_eq!(k.kind, ApiKeyKind::Owner);
        (
            k.id,
            k.key_plain
                .as_ref()
                .expect("an owner key keeps its plaintext")
                .expose()
                .to_string(),
            k.enabled,
        )
    }

    /// A gateway standing exactly where the **first** start after this
    /// release stands: the Settings string set, and no owner row yet.
    ///
    /// `init_for_tests` seeds the rows, as `init` does — which is what every
    /// other suite wants and what makes the seed itself untestable through it,
    /// since it is insert-or-nothing. So the rows are cleared here, and the
    /// tests below drive `seed_owner_keys` against the state it was written
    /// for. A second seed over the top is [`a_second_start_seeds_nothing`].
    async fn with_self_admin_token(token: &str) -> SharedState {
        let state = AppState::init_for_tests().await.unwrap();
        sqlx::query("DELETE FROM api_keys WHERE kind = 'owner'")
            .execute(&state.db)
            .await
            .unwrap();
        let settings = Settings {
            self_admin_token: token.into(),
            ..Settings::default()
        };
        store::save_settings(&state.db, &settings).await.unwrap();
        state.reload_snapshot().await.unwrap();
        state
    }

    #[tokio::test]
    async fn a_gateway_with_no_self_admin_token_seeds_the_plane_closed() {
        let state = with_self_admin_token("").await;
        seed_owner_keys(&state).await.unwrap();

        // Read straight off the snapshot the seed left behind — the seed ends
        // with a reload precisely so that the one `init` returns already has
        // the rows.
        let (_, dashboard, enabled) = owner_row(&state, OWNER_DASHBOARD);
        assert!(enabled, "the door is never seeded shut");
        assert!(dashboard.starts_with("lmgw-owner-"), "{dashboard}");

        let (_, _, enabled) = owner_row(&state, OWNER_SELF_ADMIN);
        assert!(
            !enabled,
            "a gateway whose self-admin plane was closed stays closed"
        );
    }

    #[tokio::test]
    async fn a_configured_self_admin_token_moves_into_the_row_it_now_lives_in() {
        let state = with_self_admin_token("  the-configured-token  ").await;
        seed_owner_keys(&state).await.unwrap();

        let (_, plaintext, enabled) = owner_row(&state, OWNER_SELF_ADMIN);
        assert!(enabled, "a plane that was open stays open");
        assert_eq!(
            plaintext, "the-configured-token",
            "the MCP client config presenting it keeps working without an edit"
        );
        assert_eq!(
            state.snapshot().settings.self_admin_token,
            "",
            "and the setting it came from is blanked"
        );
    }

    #[tokio::test]
    async fn a_second_start_seeds_nothing() {
        let state = with_self_admin_token("the-configured-token").await;
        seed_owner_keys(&state).await.unwrap();
        let before = [
            owner_row(&state, OWNER_DASHBOARD),
            owner_row(&state, OWNER_SELF_ADMIN),
        ];

        seed_owner_keys(&state).await.unwrap();
        assert_eq!(
            before,
            [
                owner_row(&state, OWNER_DASHBOARD),
                owner_row(&state, OWNER_SELF_ADMIN)
            ],
            "ids, plaintexts and the enabled flags all survive a restart"
        );
        assert_eq!(
            state
                .snapshot()
                .api_keys
                .iter()
                .filter(|k| k.kind == ApiKeyKind::Owner)
                .count(),
            2,
            "and no third row appears"
        );
    }
}
