//! The rule a Chat turn runs a label's tools under (client-apps design
//! §6.6): not only the entry the thread carries, but every rule the owner
//! set for the server it names — so the owner's floor holds at the turn as
//! well as at the write. A write checks labels as they resolve then; a
//! label that named nothing at that write (a server deleted, a device's
//! grant cleared) or that an owner's rename or re-prefix retargets later
//! resolves at the turn, and is gated by the floor then.
//!
//! Per target ([`label_target`]: a built-in toolset, or a registered
//! server by its tool prefix or its name), the strictest of:
//! - every entry of the thread that names it;
//! - every entry of the thread's own floor that names it, or, when that
//!   floor names it nowhere and the thread is in a folder, every entry of
//!   the folder's floor and defaults that does — the same baselines a
//!   device's write is compared with, so an owner who loosened a thread's
//!   entry against its folder's default keeps the looser rule.
//!
//! Tool names count in either spelling ([`LabelTarget::spellings`]).

use crate::config::Snapshot;
use crate::mcp::spec::ApprovalRule;
use crate::store::ThreadMcp;

use super::target::{label_target, label_target_in, LabelTarget};

/// What a turn's rules are measured with besides the thread's own entries
/// (module doc).
#[derive(Debug, Clone, Default)]
pub struct ApprovalFloor {
    /// The thread's floor (`ChatThread::approval_floor`).
    pub thread: Vec<ThreadMcp>,
    /// For a thread in a folder, the folder's floor and its defaults'
    /// `mcp_tools`; empty for one in none.
    pub folder: Vec<ThreadMcp>,
}

impl ApprovalFloor {
    /// The rule the tools `label` names run under in a turn of a thread
    /// carrying `entries` (module doc). `"never"`, the API's default, when
    /// nothing names its target.
    pub fn rule(
        &self,
        snap: &Snapshot,
        agg: &crate::mcp::Aggregate,
        label: &str,
        entries: &[ThreadMcp],
    ) -> ApprovalRule {
        let target = label_target_in(snap, agg, label);
        let of = |list: &[ThreadMcp]| -> Vec<ApprovalRule> {
            list.iter()
                .filter(|e| label_target(snap, &e.server_label).target == target.target)
                .map(ThreadMcp::approval_rule)
                .collect()
        };
        let mut rules = of(entries);
        let floor = of(&self.thread);
        rules.extend(if floor.is_empty() {
            of(&self.folder)
        } else {
            floor
        });
        let mut unique: Vec<ApprovalRule> = Vec::new();
        for r in rules {
            if !unique.contains(&r) {
                unique.push(r);
            }
        }
        if unique.len() <= 1 {
            // One rule: as written, its names matched as a turn always did.
            return unique.pop().unwrap_or_default();
        }
        unique
            .iter()
            .map(|r| both_spellings(&target, r))
            .reduce(|a, b| a.or(&b))
            .unwrap_or_default()
    }
}

/// `rule` with every listed name in each of its spellings for `target`, so
/// [`ApprovalRule::or`]'s exact comparison counts a tool whichever spelling
/// each rule named it in.
fn both_spellings(target: &LabelTarget, rule: &ApprovalRule) -> ApprovalRule {
    match rule {
        ApprovalRule::Filter { never, always } => {
            let all = |list: &[String]| -> Vec<String> {
                list.iter().flat_map(|n| target.spellings(n)).collect()
            };
            ApprovalRule::Filter {
                never: all(never),
                always: all(always),
            }
        }
        other => other.clone(),
    }
}
