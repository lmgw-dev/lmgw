//! The built-in agents that ship with lmgw, and the once-only seed (§3).
//!
//! They are embedded with `rust-embed`, the same mechanism the SPA bundle uses:
//! in a release build they are bytes in the binary, in a debug build they are
//! read off disk, so editing a shipped manifest is a restart rather than a
//! rebuild.
//!
//! The rule, and the reason for it: a built-in id is inserted **once**, tracked
//! in the KV key [`SEEDED_KEY`](super::SEEDED_KEY). A built-in the owner
//! deleted is **not** resurrected on the next start — an agent that comes back
//! from the dead every morning is a bug, not a feature. "Restore shipped
//! agents" (`agents_restore`) re-inserts the missing ones deliberately, which
//! is the only path that ignores the seeded set.
//!
//! Editing a built-in keeps `source = 'builtin'`, so "Reset to shipped"
//! ([`shipped`]) can put the embedded manifest back while keeping the config.

use serde_json::{Map, Value};

use crate::state::SharedState;
use crate::store;

use super::manifest::{self, Manifest};
use super::SEEDED_KEY;

/// Suffix every embedded manifest carries — the same name an export produces,
/// so a shipped agent and a downloaded one are the same kind of file.
const SUFFIX: &str = ".agent.json";

/// The KV key the retired IMAP mail workflow kept its config in (§7.5).
const MAIL_KV_KEY: &str = "workflow:mail";

/// The agent that workflow became.
const MAIL_AGENT_ID: &str = "mail-labeler";

#[derive(rust_embed::RustEmbed)]
#[folder = "builtin-agents/"]
struct Builtins;

/// Every shipped manifest, parsed, sorted by id.
///
/// A manifest that does not parse is a build defect, not a runtime condition:
/// it is logged loudly and skipped, so one bad file cannot stop the gateway
/// from starting. `every_builtin_is_valid` in this module's tests is what
/// actually keeps that from shipping.
pub fn shipped_all() -> Vec<Manifest> {
    let mut out: Vec<Manifest> = Vec::new();
    for file in Builtins::iter() {
        if !file.ends_with(SUFFIX) {
            continue;
        }
        let Some(raw) = Builtins::get(&file) else {
            continue;
        };
        let text = String::from_utf8_lossy(&raw.data).to_string();
        match manifest::load(&text) {
            Ok(m) => out.push(m),
            Err(e) => tracing::error!("built-in agent '{file}' is not a valid manifest: {e}"),
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// The shipped manifest for one id, if it is one of ours.
pub fn shipped(id: &str) -> Option<Manifest> {
    shipped_all().into_iter().find(|m| m.id == id)
}

/// What the seed did, for the startup log and for the `agents_restore` result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeedReport {
    pub inserted: Vec<String>,
    /// Already seeded once (present or deliberately deleted).
    pub skipped: Vec<String>,
    /// Replaced by a newer embedded manifest because the stored one was
    /// untouched since it was seeded (container-runtime §5.1).
    pub upgraded: Vec<String>,
    /// A newer manifest ships, but the row was edited (or predates the hash),
    /// so it was left exactly as it is. The card says so beside Reset.
    pub left_alone: Vec<String>,
    /// Config values an upgrade had to drop because the new manifest no longer
    /// declares the field. Named in the startup log rather than removed
    /// quietly: they are the owner's settings, even when they are stale.
    pub dropped_config: Vec<(String, Vec<String>)>,
}

/// The entry [`SEEDED_KEY`] carries for a built-in lmgw **cannot vouch for**:
/// seeded before hashes were recorded, and its stored text already differs from
/// what ships (container-runtime §5.1).
///
/// A sentinel rather than a real hash, and it has to be one: any hex string
/// here would eventually be compared equal to some stored text and let the
/// upgrade pass replace an edit nobody authorised. `"legacy"` is not 64 hex
/// characters, so it can never match [`manifest_hash`], and the row stays the
/// owner's until they press **Reset to shipped** — which records a real hash
/// and puts the row back under the rule.
pub const PIN: &str = "legacy";

/// `sha256` of a manifest's canonical text, hex — the value
/// [`SEEDED_KEY`] records per built-in id (container-runtime §5.1).
pub fn manifest_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    hex::encode(h.finalize())
}

/// What [`SEEDED_KEY`] holds, in either form.
///
/// The map is `{id: sha256(the manifest text at the moment it was seeded)}`.
/// The legacy form is the plain `["id", …]` array every install before this
/// version wrote: the ids are known, the hashes are not, and lmgw will not
/// guess whether a row it has no hash for was edited.
#[derive(Debug, Default, Clone, PartialEq)]
struct Seeded {
    hashes: std::collections::BTreeMap<String, Option<String>>,
}

impl Seeded {
    fn parse(raw: Option<&str>) -> Self {
        let mut hashes = std::collections::BTreeMap::new();
        let Some(v) = raw.and_then(|s| serde_json::from_str::<Value>(s).ok()) else {
            return Self { hashes };
        };
        match v {
            Value::Object(m) => {
                for (id, hash) in m {
                    hashes.insert(id, hash.as_str().map(str::to_string));
                }
            }
            // The legacy array: seeded, hash unknown.
            Value::Array(a) => {
                for id in a.into_iter().filter_map(|v| match v {
                    Value::String(s) => Some(s),
                    _ => None,
                }) {
                    hashes.insert(id, None);
                }
            }
            _ => {}
        }
        Self { hashes }
    }

    fn contains(&self, id: &str) -> bool {
        self.hashes.contains_key(id)
    }

    fn hash(&self, id: &str) -> Option<&str> {
        self.hashes.get(id).and_then(Option::as_deref)
    }

    fn set(&mut self, id: &str, hash: String) {
        self.hashes.insert(id.to_string(), Some(hash));
    }

    /// The map form, which is what is always written back — the legacy array is
    /// rewritten in the same sweep that read it.
    fn to_json(&self) -> String {
        let m: Map<String, Value> = self
            .hashes
            .iter()
            .map(|(id, h)| {
                (
                    id.clone(),
                    h.clone().map(Value::String).unwrap_or(Value::Null),
                )
            })
            .collect();
        Value::Object(m).to_string()
    }
}

async fn seeded(state: &SharedState) -> Seeded {
    let raw = store::get_kv(&state.db, SEEDED_KEY).await.ok().flatten();
    Seeded::parse(raw.as_deref())
}

async fn write_seeded(state: &SharedState, seen: &Seeded) {
    if let Err(e) = store::set_kv(&state.db, SEEDED_KEY, &seen.to_json()).await {
        tracing::warn!("recording seeded built-in agents: {e}");
    }
}

/// Does a **newer** built-in manifest ship than the one this row holds?
///
/// Pure, synchronous and hash-free on purpose: it answers the question the card
/// asks — "is what is stored different from what ships" — which is true for an
/// edited row and for a legacy row alike, and false the moment [`seed`] has
/// replaced an untouched one. The parsed manifests are compared rather than
/// their text, so a serialization change in an older build does not read as an
/// edit.
///
/// **The two comparisons in this module answer different questions on purpose.**
/// This one is *"is there something newer to adopt"* — parsed, so cosmetic
/// drift is not news. [`seed`]'s is *"is this row still exactly what we wrote"*
/// — the recorded text hash, because only byte equality can license replacing
/// the owner's copy without asking. A row can legitimately be "notice, yes" and
/// "replace, no" at the same time; that pair is precisely the edited built-in.
pub fn update_available(row: &crate::store::AgentRow) -> bool {
    if row.source != store::AGENT_SOURCE_BUILTIN {
        return false;
    }
    let Some(ships) = shipped(&row.id) else {
        return false;
    };
    match manifest::load(&row.manifest) {
        Ok(stored) => stored != ships,
        // Unreadable here, readable in the shipped copy: there is certainly
        // something newer to adopt.
        Err(_) => true,
    }
}

/// Insert every built-in that has never been seeded, and move an **untouched**
/// one forward when a newer manifest ships (container-runtime §5.1).
///
/// Idempotent across restarts; a deleted built-in stays deleted. The four
/// cases, all keyed off the hash recorded in [`SEEDED_KEY`]:
///
/// | recorded hash | meaning | action |
/// |---|---|---|
/// | equals `sha256` of the stored manifest | never edited since seeding | replaced, hash updated, **config kept** |
/// | differs | the owner edited it | left alone; the card shows the notice beside Reset |
/// | absent (the legacy array) | unknowable | left alone, and the hash of the *current stored text* is recorded so the row is under the rule from here on |
/// | the row is gone | deliberately deleted | still skipped |
pub async fn seed(state: &SharedState) -> SeedReport {
    let mut seen = seeded(state).await;
    let before = seen.clone();
    let mut report = SeedReport::default();
    for m in shipped_all() {
        let ships = m.to_json();
        let ships_hash = manifest_hash(&ships);
        let row = match store::get_agent(&state.db, &m.id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("seeding built-in agent '{}': {e}", m.id);
                continue;
            }
        };
        if !seen.contains(&m.id) {
            match row {
                // An id already taken by an agent the owner authored or
                // imported is left alone — the catalog key is theirs, and
                // silently replacing it would lose their work. It still counts
                // as seeded, so this does not re-run every start.
                Some(existing) => {
                    report.skipped.push(m.id.clone());
                    seen.set(&m.id, manifest_hash(&existing.manifest));
                }
                None => {
                    match store::insert_agent(&state.db, &m.id, &ships, store::AGENT_SOURCE_BUILTIN)
                        .await
                    {
                        Ok(()) => {
                            report.inserted.push(m.id.clone());
                            seen.set(&m.id, ships_hash.clone());
                        }
                        Err(e) => tracing::warn!("seeding built-in agent '{}': {e}", m.id),
                    }
                }
            }
            continue;
        }
        // Seeded before. The row may be gone (deleted on purpose), the owner's
        // (an authored row under a shipped id), unchanged, or edited.
        let Some(row) = row else {
            report.skipped.push(m.id.clone());
            continue;
        };
        if row.source != store::AGENT_SOURCE_BUILTIN {
            report.skipped.push(m.id.clone());
            continue;
        }
        let stored_hash = manifest_hash(&row.manifest);
        match seen.hash(&m.id) {
            Some(recorded) if recorded == stored_hash => {
                if stored_hash == ships_hash {
                    report.skipped.push(m.id.clone());
                    continue;
                }
                let mut next = seen.clone();
                next.set(&m.id, ships_hash.clone());
                let keep = store::AgentConfigField::of(&m);
                match store::put_builtin_manifest(
                    &state.db,
                    &m.id,
                    &ships,
                    SEEDED_KEY,
                    &next.to_json(),
                    Some(&keep),
                )
                .await
                {
                    Ok(dropped) => {
                        seen = next;
                        if !dropped.is_empty() {
                            report.dropped_config.push((m.id.clone(), dropped));
                        }
                        report.upgraded.push(m.id.clone());
                        // A different manifest can name a different
                        // model-alias field, so the token's scope moves with it.
                        if let Err(e) = super::service::resync(state, &m.id).await {
                            tracing::warn!(
                                "agent '{}': its own MCP registration could not be updated after \
                                 the built-in upgrade: {e}",
                                m.id
                            );
                        }
                        if let Err(e) = super::token::resync(state, &m.id).await {
                            tracing::warn!("re-scoping '{}' after its upgrade: {e}", m.id);
                        }
                    }
                    Err(e) => tracing::warn!("upgrading built-in agent '{}': {e}", m.id),
                }
            }
            Some(_) => report.left_alone.push(m.id.clone()),
            // The legacy form: no hash was ever recorded, so lmgw will not
            // guess whether this row was edited.
            //
            // Recording `sha256(the stored text)` here would be the same as
            // *claiming* it was never edited — the next start would read the
            // hash back, find it matching, and replace an owner's edit behind
            // their back. So the hash is only recorded when the row provably
            // needs nothing (it already holds what ships); otherwise the entry
            // is [`PIN`], which no real hash can equal, and the row stays the
            // owner's until they press Reset. The notice keeps showing, because
            // it is computed from the parsed comparison, not from this entry.
            None => {
                if stored_hash == ships_hash {
                    seen.set(&m.id, ships_hash.clone());
                    report.skipped.push(m.id.clone());
                } else {
                    seen.set(&m.id, PIN.to_string());
                    report.left_alone.push(m.id.clone());
                }
            }
        }
    }
    if seen != before {
        write_seeded(state, &seen).await;
    }
    if !report.inserted.is_empty() {
        tracing::info!("seeded built-in agent(s): {}", report.inserted.join(", "));
    }
    if !report.upgraded.is_empty() {
        tracing::info!(
            "built-in agent(s) moved to the manifest this version ships, config kept: {}",
            report.upgraded.join(", ")
        );
    }
    for (id, dropped) in &report.dropped_config {
        tracing::info!(
            "'{id}' no longer declares the config field(s) {} — the stored value(s) were removed \
             with the manifest that declared them, so the Run tab does not start with a value its \
             own schema refuses",
            dropped.join(", ")
        );
    }
    if !report.left_alone.is_empty() {
        tracing::info!(
            "a newer built-in manifest ships for {} — the stored one was edited (or predates the \
             recorded hash), so it was left alone; \"Reset to shipped\" adopts it",
            report.left_alone.join(", ")
        );
    }
    // After the insert, so a fresh install has the row to write the old
    // workflow's taxonomy into (§7.5).
    migrate_mail_kv(state).await;
    report
}

/// The `{id: hash}` entry a writer of a built-in manifest has to record
/// alongside the row (§5.1) — used by `agent_reset`, which writes both in one
/// transaction through [`store::put_builtin_manifest`].
pub async fn seeded_with(state: &SharedState, id: &str, hash: &str) -> String {
    let mut seen = seeded(state).await;
    seen.set(id, hash.to_string());
    seen.to_json()
}

// ---------------------------------------------------------------------------
// The mail workflow's KV row (§7.5)
// ---------------------------------------------------------------------------

/// What one `workflow:mail` row carries over, and what it does not.
///
/// The IMAP half — `host`, `port`, `username` and above all `password` — is
/// **dropped, not migrated**: the agent reaches Gmail through the Workspace MCP
/// server's own OAuth token, so there is no credential for lmgw to keep, and
/// the point of retiring this key is that a password stopped living in it. The
/// drop is logged by name so the owner can see what went.
struct MailKv {
    values: Map<String, Value>,
    dropped: Vec<&'static str>,
}

/// Split the old comma-separated taxonomy the way `parse_categories` did:
/// trimmed, deduped case-insensitively, and without `Other` — the manifest's
/// `fallback` supplies that one, exactly once and last (§2.4).
fn split_categories(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if c.eq_ignore_ascii_case("other") {
            continue;
        }
        if !out.iter().any(|x| x.eq_ignore_ascii_case(c)) {
            out.push(c.to_string());
        }
    }
    out
}

fn read_mail_kv(raw: &str) -> Option<MailKv> {
    let old: Map<String, Value> = match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(m)) => m,
        _ => return None,
    };
    let mut values = Map::new();
    for key in ["model", "label_prefix", "limit", "concurrency"] {
        match old.get(key) {
            // An empty model is the unconfigured state, not a choice; leaving
            // it out lets the schema's picker start blank instead of storing a
            // value that names nothing.
            Some(Value::String(s)) if s.is_empty() => {}
            Some(v) if !v.is_null() => {
                values.insert(key.to_string(), v.clone());
            }
            _ => {}
        }
    }
    if let Some(Value::String(raw_cats)) = old.get("categories") {
        let cats = split_categories(raw_cats);
        // A blank list fell back to the defaults in the old code; here the
        // schema's `default` is that same list, so omitting the field is the
        // faithful translation rather than storing an empty taxonomy.
        if !cats.is_empty() {
            values.insert(
                "categories".into(),
                Value::Array(cats.into_iter().map(Value::String).collect()),
            );
        }
    }
    let dropped = ["host", "port", "username", "password"]
        .into_iter()
        .filter(|k| old.contains_key(*k))
        .collect();
    Some(MailKv { values, dropped })
}

/// Move the retired mail workflow's config into the mail-labeler agent, then
/// delete the key (§7.5).
///
/// The key's **presence is the one-shot gate**: deleting it closes it, so this
/// is idempotent across restarts without a second marker to keep in sync with
/// [`SEEDED_KEY`]. If the agent row is missing — the owner deleted the built-in
/// before ever upgrading — the key is left in place and said so, because
/// throwing the taxonomy away to satisfy a migration is worse than running it
/// one start later after "Restore shipped agents".
pub async fn migrate_mail_kv(state: &SharedState) {
    let raw = match store::get_kv(&state.db, MAIL_KV_KEY).await {
        Ok(Some(raw)) => raw,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("reading '{MAIL_KV_KEY}' to migrate it: {e}");
            return;
        }
    };
    let Some(old) = read_mail_kv(&raw) else {
        tracing::warn!(
            "'{MAIL_KV_KEY}' does not hold a JSON object; leaving it alone rather than \
             guessing at it"
        );
        return;
    };
    let agent = match store::get_agent(&state.db, MAIL_AGENT_ID).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::warn!(
                "the retired mail workflow's config is still in '{MAIL_KV_KEY}', but there is no \
                 '{MAIL_AGENT_ID}' agent to move it into — use 'Restore shipped agents' and \
                 restart, or delete the key"
            );
            return;
        }
        Err(e) => {
            tracing::warn!("reading agent '{MAIL_AGENT_ID}' to migrate '{MAIL_KV_KEY}': {e}");
            return;
        }
    };

    let mut config: Map<String, Value> = match serde_json::from_str::<Value>(&agent.config) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    // The row wearing this id is not necessarily the shipped agent: the seed
    // deliberately leaves an authored or imported one in place, and writing
    // fields its schema never declared would make every run of it fail
    // validation on the migration's own output. A value the shipped schema
    // refuses — a hand-edited `limit` of 0 — lands here too, which is the
    // honest place for it: the key stays, so nothing is lost and a later start
    // can still carry it over. Only the values being written are checked, never
    // what the agent already holds, so nothing stored ends up in the log.
    let fit = manifest::load(&agent.manifest)
        .and_then(|m| m.fields().map_err(|e| e.join("; ")))
        .and_then(|fields| manifest::validate_present_values(&fields, &old.values));
    if let Err(why) = fit {
        tracing::warn!(
            "not migrating '{MAIL_KV_KEY}': its values do not fit the '{MAIL_AGENT_ID}' agent \
             that is in the catalog ({why}). The key is left in place — fix the value or the \
             agent and restart, or delete the key"
        );
        return;
    }
    let moved: Vec<String> = old.values.keys().cloned().collect();
    for (k, v) in old.values {
        config.insert(k, v);
    }
    let json = Value::Object(config).to_string();
    if let Err(e) = store::set_agent_config(&state.db, MAIL_AGENT_ID, &json).await {
        tracing::warn!("writing the migrated mail config onto '{MAIL_AGENT_ID}': {e}");
        return;
    }
    match store::delete_kv(&state.db, MAIL_KV_KEY).await {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!("deleting the migrated '{MAIL_KV_KEY}': {e}");
            return;
        }
    }
    tracing::info!(
        "migrated the mail workflow's config into agent '{MAIL_AGENT_ID}' ({}) and deleted \
         '{MAIL_KV_KEY}'; dropped {} — the Workspace MCP server holds the Gmail credential now, \
         so lmgw keeps none",
        if moved.is_empty() {
            "nothing was set".to_string()
        } else {
            moved.join(", ")
        },
        if old.dropped.is_empty() {
            "nothing".to_string()
        } else {
            old.dropped.join(", ")
        },
    );
}

/// "Restore shipped agents": re-insert every built-in the catalog is missing,
/// ignoring the seeded set. The deliberate counterpart to [`seed`].
pub async fn restore(state: &SharedState) -> Result<Value, String> {
    let mut seen = seeded(state).await;
    let before = seen.clone();
    let mut restored: Vec<String> = Vec::new();
    for m in shipped_all() {
        let existing = store::get_agent(&state.db, &m.id)
            .await
            .map_err(|e| e.to_string())?;
        if existing.is_some() {
            continue;
        }
        let text = m.to_json();
        // The hash goes in with the row, in one transaction (§5.1): a restored
        // row carrying the new manifest against no hash at all would read as
        // "edited" on the next start and stay pinned out of the upgrade path.
        seen.set(&m.id, manifest_hash(&text));
        store::put_builtin_manifest(&state.db, &m.id, &text, SEEDED_KEY, &seen.to_json(), None)
            .await
            .map_err(|e| e.to_string())?;
        // No `token::resync` here, and it is not an oversight: this branch only
        // runs for an id the catalog does **not** hold, and `agent_delete` took
        // the agent's key row with it (container-runtime §3.1). A restored
        // agent therefore has no token whose scope could be stale; the next
        // `agent_token_get` mints one against the manifest just written.
        restored.push(m.id.clone());
    }
    if seen != before {
        write_seeded(state, &seen).await;
    }
    Ok(serde_json::json!({
        "ok": true,
        "restored": restored,
        "message": if restored.is_empty() {
            "every shipped agent is already in the catalog".to_string()
        } else {
            format!("restored {}", restored.join(", "))
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded manifests are the gateway's own documents: if one of them
    /// is invalid, every install ships an agent that cannot start. This is the
    /// check that keeps [`shipped_all`]'s tolerant "log and skip" from hiding
    /// that.
    #[test]
    fn every_builtin_is_valid_and_carries_a_config_schema() {
        let all = shipped_all();
        assert!(
            !all.is_empty(),
            "no built-in agents were embedded — check the builtin-agents/ folder"
        );
        for m in &all {
            m.validate().unwrap_or_else(|e| panic!("{}: {e}", m.id));
            // Every built-in has to be runnable on a fresh install, which means
            // the model is chosen on the Run tab rather than hard-coded to an
            // alias this machine may not have.
            let fields = m.fields().unwrap();
            assert!(
                fields
                    .iter()
                    .any(|f| f.format == Some(manifest::Format::ModelAlias)),
                "{}: no model_alias field for the Run tab's picker",
                m.id
            );
        }
    }

    #[test]
    fn a_shipped_manifest_round_trips_through_its_canonical_form() {
        for m in shipped_all() {
            let again = manifest::load(&m.to_json()).unwrap();
            assert_eq!(m, again, "{} is not stable through to_json", m.id);
        }
    }

    #[test]
    fn splitting_the_old_taxonomy_dedups_and_drops_other() {
        assert_eq!(
            split_categories("Work, work,  Finance , Other, finance, other"),
            vec!["Work", "Finance"]
        );
        assert!(split_categories("  ,  , ").is_empty());
    }

    /// The whole of §7.5 against a real seeded row: the five carried fields
    /// land on the agent, the four IMAP ones do not, the key is gone, and a
    /// second start changes nothing.
    #[tokio::test]
    async fn the_mail_kv_moves_into_the_agent_once_and_the_key_goes() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        store::set_kv(
            &state.db,
            MAIL_KV_KEY,
            &serde_json::json!({
                "host": "imap.gmail.com",
                "port": 993,
                "username": "someone@example.com",
                "password": "hunter2",
                "model": "gemma4-e4b",
                "categories": "Work, work, Finance, Other",
                "label_prefix": "mail",
                "limit": 25,
                "concurrency": 2,
            })
            .to_string(),
        )
        .await
        .unwrap();

        // The migration is part of the seed step, not a separate startup hook.
        seed(&state).await;

        let row = store::get_agent(&state.db, MAIL_AGENT_ID)
            .await
            .unwrap()
            .expect("the mail-labeler built-in is seeded");
        let config: Value = serde_json::from_str(&row.config).unwrap();
        assert_eq!(config["model"], "gemma4-e4b");
        assert_eq!(config["label_prefix"], "mail");
        assert_eq!(config["limit"], 25);
        assert_eq!(config["concurrency"], 2);
        assert_eq!(
            config["categories"],
            serde_json::json!(["Work", "Finance"]),
            "the taxonomy is split, deduped and loses 'Other' (the fallback supplies it)"
        );
        for gone in ["host", "port", "username", "password"] {
            assert!(
                config.get(gone).is_none(),
                "{gone} must not survive the migration: {config}"
            );
        }
        // No IMAP credential anywhere in the stored config.
        assert!(!row.config.contains("hunter2"), "{}", row.config);
        assert_eq!(store::get_kv(&state.db, MAIL_KV_KEY).await.unwrap(), None);

        // The stored values validate against the shipped schema, so the Run tab
        // renders them instead of reporting the migration's own output as bad.
        let m = shipped(MAIL_AGENT_ID).unwrap();
        let fields = m.fields().unwrap();
        let Value::Object(values) = config else {
            panic!("config is not an object")
        };
        manifest::validate_values(&fields, &values).unwrap();

        // Idempotent: the key is the gate, and it is gone.
        store::set_agent_config(&state.db, MAIL_AGENT_ID, "{}")
            .await
            .unwrap();
        seed(&state).await;
        let again = store::get_agent(&state.db, MAIL_AGENT_ID)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.config, "{}", "the migration ran a second time");
    }

    /// A blank taxonomy and an unset model fall through to the manifest's own
    /// defaults rather than being stored as empty — the old code's behaviour,
    /// which fell back to the default list when the field was blanked.
    #[tokio::test]
    async fn blank_fields_fall_through_to_the_schema_defaults() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        store::set_kv(
            &state.db,
            MAIL_KV_KEY,
            &serde_json::json!({ "model": "", "categories": " , ", "limit": 10 }).to_string(),
        )
        .await
        .unwrap();
        seed(&state).await;
        let row = store::get_agent(&state.db, MAIL_AGENT_ID)
            .await
            .unwrap()
            .unwrap();
        let config: Value = serde_json::from_str(&row.config).unwrap();
        assert!(config.get("model").is_none(), "{config}");
        assert!(config.get("categories").is_none(), "{config}");
        assert_eq!(config["limit"], 10);
    }

    // -----------------------------------------------------------------------
    // Upgrading a shipped built-in (container-runtime §5.1)
    // -----------------------------------------------------------------------

    /// The manifest an install seeded *before* hashes were recorded: the same
    /// document at `2.0.0`, with the retired `apply.turn` WP3 replaced.
    fn mail_2_0_0() -> String {
        let mut v: Value =
            serde_json::from_str(&shipped(MAIL_AGENT_ID).unwrap().to_json()).unwrap();
        v["version"] = Value::String("2.0.0".into());
        v["run"]["apply"] = serde_json::json!({ "turn": {
            "tools": ["gws__gmail_batchModify"],
            "prompt": "Label every row: {{rows}}",
            "output": { "type": "object",
                        "properties": { "applied": { "type": "integer" },
                                        "labels": { "type": "array",
                                                    "items": { "type": "string" } } },
                        "required": ["applied", "labels"] } } });
        manifest::load(&v.to_string()).unwrap().to_json()
    }

    async fn kv(state: &SharedState) -> Value {
        serde_json::from_str(
            &store::get_kv(&state.db, SEEDED_KEY)
                .await
                .unwrap()
                .unwrap_or_default(),
        )
        .unwrap_or(Value::Null)
    }

    async fn row(state: &SharedState) -> crate::store::AgentRow {
        store::get_agent(&state.db, MAIL_AGENT_ID)
            .await
            .unwrap()
            .expect("the mail agent is in the catalog")
    }

    /// Put the catalog back into the state an 0.1.58 install was in: the
    /// `2.0.0` row, a tuned config, and whichever KV form the caller wants.
    async fn install_2_0_0(state: &SharedState, seeded: Value) {
        seed(state).await;
        store::update_agent_manifest(&state.db, MAIL_AGENT_ID, &mail_2_0_0())
            .await
            .unwrap();
        store::set_agent_config(
            &state.db,
            MAIL_AGENT_ID,
            &serde_json::json!({ "model": "qwen3.8", "categories": ["Work", "Bills"] }).to_string(),
        )
        .await
        .unwrap();
        store::set_kv(&state.db, SEEDED_KEY, &seeded.to_string())
            .await
            .unwrap();
    }

    /// The hash matches what is stored, so the row was never edited: the new
    /// manifest replaces it and **the config is kept**, exactly as
    /// `agent_reset` keeps it.
    #[tokio::test]
    async fn an_untouched_builtin_is_moved_forward_and_keeps_its_config() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let recorded = serde_json::json!({ MAIL_AGENT_ID: manifest_hash(&mail_2_0_0()) });
        install_2_0_0(&state, recorded).await;

        let report = seed(&state).await;
        assert_eq!(report.upgraded, [MAIL_AGENT_ID], "{report:?}");
        assert!(report.left_alone.is_empty(), "{report:?}");

        let row = row(&state).await;
        let m = manifest::load(&row.manifest).unwrap();
        assert_eq!(m.version.as_deref(), Some("3.0.0"));
        assert!(m.apply_step().unwrap().is_script(), "still a turn: {row:?}");
        let config: Value = serde_json::from_str(&row.config).unwrap();
        assert_eq!(config["categories"], serde_json::json!(["Work", "Bills"]));
        assert_eq!(config["model"], "qwen3.8");
        // The hash moved with the row, so the next ship is not read as an edit.
        assert_eq!(
            kv(&state).await[MAIL_AGENT_ID],
            Value::String(manifest_hash(&row.manifest))
        );
        assert!(!update_available(&row));

        // Idempotent: a second start changes nothing and says nothing.
        let again = seed(&state).await;
        assert!(again.upgraded.is_empty(), "{again:?}");
        assert_eq!(
            store::get_agent(&state.db, MAIL_AGENT_ID)
                .await
                .unwrap()
                .unwrap()
                .manifest,
            row.manifest
        );
    }

    /// The hash differs, so the owner edited it. lmgw leaves the row exactly as
    /// it is and says a newer one ships — the notice the card renders beside
    /// **Reset to shipped**.
    #[tokio::test]
    async fn an_edited_builtin_is_left_alone_and_the_notice_says_so() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        // Recorded against something else entirely: the row has moved since.
        let recorded = serde_json::json!({ MAIL_AGENT_ID: manifest_hash("{}") });
        install_2_0_0(&state, recorded.clone()).await;

        let report = seed(&state).await;
        assert_eq!(report.left_alone, [MAIL_AGENT_ID], "{report:?}");
        assert!(report.upgraded.is_empty(), "{report:?}");
        let row = row(&state).await;
        assert_eq!(row.manifest, mail_2_0_0(), "an edited row was overwritten");
        assert!(
            update_available(&row),
            "the notice is what makes it fixable"
        );
        // And the mail agent's recorded hash is untouched, so the rule still
        // reads "edited" on the next start too.
        assert_eq!(kv(&state).await[MAIL_AGENT_ID], recorded[MAIL_AGENT_ID]);
    }

    /// The legacy array form — every install before this version. lmgw has no
    /// hash for the row and will not guess whether it was edited, so the row is
    /// **pinned**: the sweep rewrites the array into the map recording the
    /// [`PIN`] sentinel, and the row is never auto-replaced at any later start.
    #[tokio::test]
    async fn the_legacy_array_pins_the_row_and_becomes_the_map_form() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        install_2_0_0(&state, serde_json::json!([MAIL_AGENT_ID, "docs-librarian"])).await;

        let report = seed(&state).await;
        assert_eq!(report.left_alone, [MAIL_AGENT_ID], "{report:?}");
        let stored = row(&state).await;
        assert_eq!(stored.manifest, mail_2_0_0(), "the legacy row was replaced");
        assert!(update_available(&stored));

        // Rewritten to the map, pinned rather than vouched for. Recording the
        // stored text's real hash here would *claim* the row was never edited,
        // and the next start would act on that claim.
        let map = kv(&state).await;
        assert!(map.is_object(), "the array survived: {map}");
        assert_eq!(map[MAIL_AGENT_ID], Value::String(PIN.to_string()));
        assert!(map.get("docs-librarian").is_some(), "{map}");
    }

    /// The pin holds. Two more starts do not replace an edited legacy row, and
    /// the notice keeps offering the way out rather than taking it.
    #[tokio::test]
    async fn a_pinned_legacy_row_survives_every_later_start_until_reset() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        install_2_0_0(&state, serde_json::json!([MAIL_AGENT_ID])).await;
        seed(&state).await;
        for start in 2..=3 {
            let report = seed(&state).await;
            assert!(
                report.upgraded.is_empty(),
                "start {start} replaced the owner's manifest: {report:?}"
            );
            let stored = row(&state).await;
            assert_eq!(
                stored.manifest,
                mail_2_0_0(),
                "start {start} overwrote an edited legacy row"
            );
            assert!(update_available(&stored), "the notice went away at {start}");
            assert_eq!(kv(&state).await[MAIL_AGENT_ID], Value::String(PIN.into()));
        }

        // A legacy row that happens to hold exactly what ships is the one case
        // lmgw can vouch for, so it is hashed instead of pinned — nothing is
        // being replaced, so nothing is being guessed.
        store::update_agent_manifest(
            &state.db,
            MAIL_AGENT_ID,
            &shipped(MAIL_AGENT_ID).unwrap().to_json(),
        )
        .await
        .unwrap();
        store::set_kv(
            &state.db,
            SEEDED_KEY,
            &serde_json::json!([MAIL_AGENT_ID]).to_string(),
        )
        .await
        .unwrap();
        seed(&state).await;
        assert_eq!(
            kv(&state).await[MAIL_AGENT_ID],
            Value::String(manifest_hash(&shipped(MAIL_AGENT_ID).unwrap().to_json()))
        );
    }

    /// An upgrade keeps the config, minus anything the new schema no longer
    /// declares — a value whose field is gone is not "kept", it is a Start that
    /// fails validation on a manifest the owner never chose.
    #[tokio::test]
    async fn an_upgrade_drops_config_the_new_manifest_no_longer_declares() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let recorded = serde_json::json!({ MAIL_AGENT_ID: manifest_hash(&mail_2_0_0()) });
        install_2_0_0(&state, recorded).await;
        store::set_agent_config(
            &state.db,
            MAIL_AGENT_ID,
            &serde_json::json!({ "model": "qwen3.8", "retired_knob": 7 }).to_string(),
        )
        .await
        .unwrap();

        let report = seed(&state).await;
        assert_eq!(report.upgraded, [MAIL_AGENT_ID], "{report:?}");
        assert_eq!(
            report.dropped_config,
            vec![(MAIL_AGENT_ID.to_string(), vec!["retired_knob".to_string()])],
            "{report:?}"
        );
        let config: Value = serde_json::from_str(&row(&state).await.config).unwrap();
        assert_eq!(config["model"], "qwen3.8", "a live field was dropped too");
        assert!(config.get("retired_knob").is_none(), "{config}");
        // And what is left really validates against the schema that now ships.
        let m = shipped(MAIL_AGENT_ID).unwrap();
        let Value::Object(values) = config else {
            panic!("config is not an object")
        };
        manifest::validate_values(&m.fields().unwrap(), &values).unwrap();
    }

    /// A built-in the owner deleted stays deleted — an agent that comes back
    /// from the dead every morning is a bug, not a feature — and the upgrade
    /// rule does not resurrect it.
    #[tokio::test]
    async fn a_deleted_builtin_is_not_resurrected_by_the_upgrade_pass() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        install_2_0_0(&state, serde_json::json!([MAIL_AGENT_ID])).await;
        store::delete_agent(&state.db, MAIL_AGENT_ID).await.unwrap();

        seed(&state).await;
        assert!(store::get_agent(&state.db, MAIL_AGENT_ID)
            .await
            .unwrap()
            .is_none());
        // "Restore shipped agents" is the deliberate way back, and it records
        // the hash with the row so the next ship arrives on its own.
        restore(&state).await.unwrap();
        let row = row(&state).await;
        assert_eq!(
            manifest::load(&row.manifest).unwrap().version.as_deref(),
            Some("3.0.0")
        );
        assert_eq!(
            kv(&state).await[MAIL_AGENT_ID],
            Value::String(manifest_hash(&row.manifest))
        );
        assert!(!update_available(&row));
    }

    /// A row under a shipped id that the owner authored is not the built-in,
    /// and the upgrade rule never touches it.
    #[tokio::test]
    async fn an_authored_row_under_a_shipped_id_is_never_upgraded() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        install_2_0_0(&state, serde_json::json!([MAIL_AGENT_ID])).await;
        sqlx::query("UPDATE agents SET source = 'authored' WHERE id = ?1")
            .bind(MAIL_AGENT_ID)
            .execute(&state.db)
            .await
            .unwrap();
        seed(&state).await;
        seed(&state).await;
        let row = row(&state).await;
        assert_eq!(row.manifest, mail_2_0_0());
        assert!(!update_available(&row), "not a built-in, so no notice");
    }

    /// A value the agent's own schema refuses is not written onto it, and the
    /// key stays: the taxonomy is the thing worth keeping, so a migration that
    /// cannot land leaves the owner something to land later (§7.5).
    #[tokio::test]
    async fn values_the_agent_cannot_hold_keep_the_key() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        store::set_kv(
            &state.db,
            MAIL_KV_KEY,
            // `limit` is `minimum: 1`; a 0 only reaches the KV by hand, and
            // writing it would make every run fail on its own config.
            &serde_json::json!({ "model": "gemma4-e4b", "categories": "Work", "limit": 0 })
                .to_string(),
        )
        .await
        .unwrap();
        seed(&state).await;

        let row = store::get_agent(&state.db, MAIL_AGENT_ID)
            .await
            .unwrap()
            .unwrap();
        let config: Value = serde_json::from_str(&row.config).unwrap();
        assert!(
            config.get("limit").is_none() && config.get("model").is_none(),
            "an out-of-schema migration was written anyway: {config}"
        );
        assert!(
            store::get_kv(&state.db, MAIL_KV_KEY)
                .await
                .unwrap()
                .is_some(),
            "the key went even though nothing was migrated"
        );
    }
}
