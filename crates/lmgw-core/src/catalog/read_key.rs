//! What a catalog read and its cached answer are for (openai egress design
//! §14.2): an upstream row's id together with the configuration the read
//! went out with. A request can hold a snapshot from before a write changed
//! the row's address, key, kind or protocol (it queued across the write),
//! and a read it starts then is that old configuration's list: keyed by the
//! id alone, it would be cached as the live row's after `invalidate` had
//! already run. Keyed by both, it answers only requests holding that same
//! configuration, and a request with the live one reads its own.

use crate::config::Upstream;

/// The id and the configuration a catalog read depends on: what
/// `fetch_models` sends with (the URL, the key, the protocol's path and
/// shape, the kind), and the row's extra headers, which a models read does
/// not send yet but a write that changes them keys a fresh read all the
/// same.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ReadKey {
    pub(super) id: i64,
    protocol: &'static str,
    kind: &'static str,
    base_url: String,
    api_key: Option<String>,
    extra_headers: Vec<(String, String)>,
}

impl ReadKey {
    pub(super) fn of(u: &Upstream) -> Self {
        Self {
            id: u.id,
            protocol: u.protocol.as_str(),
            kind: u.kind.as_str(),
            base_url: u.base_url.clone(),
            api_key: u.api_key.clone(),
            extra_headers: u.extra_headers.clone(),
        }
    }
}

/// Held by a read's task: when the task ends however it ends (its answer
/// cached, a panic unwinding through it, the runtime dropping it), the read
/// leaves its place in flight, if it still holds it, so the next request
/// starts another rather than awaiting one nobody runs.
pub(super) struct Leave {
    pub(super) state: crate::state::SharedState,
    pub(super) key: ReadKey,
    pub(super) n: u64,
}

impl Drop for Leave {
    fn drop(&mut self) {
        let mut reading = self
            .state
            .catalog
            .reading
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if reading.get(&self.key).is_some_and(|(m, _)| *m == self.n) {
            reading.remove(&self.key);
        }
    }
}
