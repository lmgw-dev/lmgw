//! The tools each server listed when it was last connected ([`super::names`]:
//! a server not connected now still claims its tools' names, so a name does
//! not change when a server is reaped, sleeps or goes offline). Held in
//! memory, read from `mcp_known_tools` once, and written back whenever a
//! connected server's listing differs from what is held.

use std::collections::HashMap;
use std::sync::Mutex;

use rmcp::model::Tool;

use super::McpManager;

/// The last listing of each server, by id.
#[derive(Default)]
pub(super) struct Known {
    /// `None` until read from the database (or, without one, until first
    /// used).
    names: Mutex<Option<HashMap<i64, Held>>>,
    /// Orders the writes, so the last one stores the newest listing.
    write: tokio::sync::Mutex<()>,
}

/// One server's last listing.
struct Held {
    /// Its upstream tool names, sorted.
    names: Vec<String>,
    /// [`fingerprint`] of the listing these were read from; `None` for one
    /// read from the database. A listing with the same one is not read
    /// again.
    print: Option<u64>,
}

/// A listing's names, as [`Known`] holds them.
fn names_of(tools: &[Tool]) -> Vec<String> {
    let mut v: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    v.sort();
    v.dedup();
    v
}

/// The listing's names in its own order, hashed: what tells an unchanged
/// listing apart without sorting it on every aggregate. Two listings that
/// differ and hash alike would keep the older names held; a 64-bit hash
/// makes that a matter of chance no one meets.
pub(super) fn fingerprint(tools: &[Tool]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for t in tools {
        t.name.as_ref().hash(&mut h);
    }
    h.finish()
}

impl McpManager {
    /// What each server in `want` last listed, `listed` (the servers
    /// connected and listed now: id, [`fingerprint`], tools) taken in
    /// first: a listing that differs from the one held is stored.
    pub(super) async fn known_tools(
        &self,
        listed: &[(i64, u64, &[Tool])],
        want: impl Fn(i64) -> bool,
    ) -> HashMap<i64, Vec<String>> {
        let loaded = self.known.names.lock().map(|g| g.is_some()).unwrap_or(true);
        if !loaded {
            let stored = match self.app() {
                Some(app) => crate::store::known_tools(&app.db)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!("MCP: reading the servers' last tool lists failed: {e}");
                        HashMap::new()
                    }),
                None => HashMap::new(),
            };
            if let Ok(mut g) = self.known.names.lock() {
                g.get_or_insert(
                    stored
                        .into_iter()
                        .map(|(id, names)| (id, Held { names, print: None }))
                        .collect(),
                );
            }
        }
        let (changed, out) = {
            let Ok(mut g) = self.known.names.lock() else {
                return HashMap::new();
            };
            let map = g.get_or_insert_with(HashMap::new);
            let mut changed = Vec::new();
            for &(id, print, tools) in listed {
                let held = map.get(&id);
                if held.is_some_and(|h| h.print == Some(print)) {
                    continue;
                }
                let names = names_of(tools);
                if held.map(|h| &h.names) != Some(&names) {
                    changed.push(id);
                }
                map.insert(
                    id,
                    Held {
                        names,
                        print: Some(print),
                    },
                );
            }
            let out = map
                .iter()
                .filter(|(id, _)| want(**id))
                .map(|(id, h)| (*id, h.names.clone()))
                .collect();
            (changed, out)
        };
        if !changed.is_empty() {
            self.store_known(changed);
        }
        out
    }

    /// Drop the listings of servers that no longer exist (`exists`): the
    /// database's go with their rows.
    pub(super) fn forget_known(&self, exists: impl Fn(i64) -> bool) {
        if let Ok(mut g) = self.known.names.lock() {
            if let Some(map) = g.as_mut() {
                map.retain(|id, _| exists(*id));
            }
        }
    }

    /// Write the listings of `ids` as held when the write runs.
    fn store_known(&self, ids: Vec<i64>) {
        let (Some(app), Ok(rt)) = (self.app(), tokio::runtime::Handle::try_current()) else {
            return;
        };
        rt.spawn(async move {
            let mcp = &app.mcp;
            let _order = mcp.known.write.lock().await;
            for id in ids {
                let names = mcp
                    .known
                    .names
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().and_then(|m| m.get(&id).map(|h| h.names.clone())));
                let Some(names) = names else { continue };
                if let Err(e) = crate::store::set_known_tools(&app.db, id, &names).await {
                    tracing::debug!("MCP server {id}: storing its tool list failed: {e}");
                }
            }
        });
    }
}
