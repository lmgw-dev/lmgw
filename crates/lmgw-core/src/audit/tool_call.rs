//! A writing `lmgw__*` call's entry (§5.8): the self-admin dispatcher's
//! half of the log, whose route (`/mcp/admin`, the Chat's executor) is one
//! for reads and writes. Begun before the dispatcher's own gate, so a call
//! that gate refuses is an entry too.

use std::sync::Arc;

use serde_json::{Map, Value};

use super::{redact, Actor};
use crate::config::Snapshot;
use crate::state::SharedState;

/// The outcome of a call the self-admin mode, the device's level or an
/// access guard refused before it ran.
pub const REFUSED: u16 = 403;

/// A writing call's entry, begun before the call runs (what the settings
/// were) and written once it answered or was refused.
pub struct ToolCall {
    actor: Actor,
    before: Arc<Snapshot>,
    name: String,
    target: Map<String, Value>,
    /// The settings patch it carries ([`redact::PATCH_OPS`]): its fields
    /// are what it asked for, not the snapshots' difference.
    patch: bool,
}

impl ToolCall {
    /// `Some` for a call of a writing tool by `actor` while accounts are on.
    pub fn begin(
        state: &SharedState,
        actor: Option<&Actor>,
        name: &str,
        writes: bool,
        args: Option<&Map<String, Value>>,
    ) -> Option<Self> {
        let before = state.snapshot();
        (writes && before.accounts_on).then_some(())?;
        let patch = redact::PATCH_OPS.contains(&name);
        let mut target = Map::new();
        if let Some(args) = args {
            if patch {
                let fields = redact::patch_fields(args);
                if !fields.is_empty() {
                    target.insert("fields".into(), Value::Object(fields));
                }
            } else {
                target = redact::identity(args);
            }
        }
        Some(Self {
            actor: actor?.clone(),
            before,
            name: name.to_string(),
            target,
            patch,
        })
    }

    /// The call was refused before it ran, with `outcome`
    /// ([`REFUSED`], or 400 for an argument the tool does not take).
    pub async fn refused(self, state: &SharedState, outcome: u16) {
        let Self {
            actor,
            name,
            target,
            ..
        } = self;
        super::record(state, actor.entry(name, target, outcome)).await;
    }

    /// Write the entry: 200 when the tool did it, 400 when it answered an
    /// error.
    pub async fn done(mut self, state: &SharedState, ok: bool) {
        let after = state.snapshot();
        if !self.patch && !Arc::ptr_eq(&self.before, &after) {
            let fields = redact::settings_fields(&self.before.settings, &after.settings);
            if !fields.is_empty() {
                self.target.insert("fields".into(), Value::Object(fields));
            }
        }
        let outcome = if ok { 200 } else { 400 };
        super::record(state, self.actor.entry(self.name, self.target, outcome)).await;
    }
}
