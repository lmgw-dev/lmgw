//! A device only tightens `require_approval` (the owner's decision,
//! 2026-10-09; client-apps design §6.6).
//!
//! **Targets, not labels.** Every entry is keyed by what its label names as
//! a turn resolves it ([`label_target`]): a built-in toolset, or a
//! registered server by its tool prefix or its name. Two entries of one
//! write that name the same target are refused for every writer
//! ([`duplicates`], `400`): a turn keeps one of them, and which one is not
//! what the list shows. Tool names are compared in the tool's own spelling
//! (`<prefix>__` taken off, [`LabelTarget::canonical`]), as a turn matches
//! either spelling.
//!
//! **The baselines.** A device's entry is compared with every rule it may
//! not go below for its target:
//! - the owner's **floor** (the thread's or folder's `approval_floor`): the
//!   rule the owner last wrote for that target. Only the owner's writes set
//!   it ([`owner_floor`]); a device removing an entry leaves it, so an entry
//!   added back is no looser than the owner's last rule;
//! - the **stored** rule, what the thread or folder carries now (a device's
//!   own tightening included);
//! - for a thread in a folder, when neither names the target, the folder's
//!   floor and defaults for it.
//!
//! Per baseline, the old and new rules are compared tool by tool over every
//! tool either names and over "any other tool" (the rest of the label):
//! `always` -> `never`, a tool dropped from an `always` list or added to a
//! `never` list, a dropped field, a filter that stops gating the label's
//! other tools: each is a loosening, refused `403 approval_loosen_refused`,
//! naming the label and the tool. Tightening passes, and so does a server
//! whose rules the thread or folder already carries unchanged (every entry
//! for it, none added). Removing an entry is no loosening: its tools go
//! with it, and its floor stays — and a turn applies the floor again
//! (`mcp::exec::ApprovalFloor`), for a label that names its server only
//! later.

use axum::http::StatusCode;
use axum::response::Response;

use super::super::chat::err_json;
use crate::config::Snapshot;
use crate::mcp::exec::{label_target, label_target_in, LabelTarget, Target};
use crate::mcp::spec::ApprovalRule;
use crate::state::SharedState;
use crate::store::ThreadMcp;

/// The code of every refusal here.
pub(in crate::web) const CODE: &str = "approval_loosen_refused";

/// Stands for every tool a rule does not name.
const OTHER: &str = "\u{0}other";

/// Where a baseline came from, for the refusal's words.
#[derive(Clone, Copy)]
enum From {
    Floor,
    Stored,
    Folder,
}

impl From {
    fn words(self) -> &'static str {
        match self {
            Self::Floor => "what the owner last set for it",
            Self::Stored => "what it required before",
            Self::Folder => "the folder's default for it",
        }
    }
}

/// `rule` with every listed name in the tool's own spelling for `target`.
fn canonical(target: &LabelTarget, rule: ApprovalRule) -> ApprovalRule {
    match rule {
        ApprovalRule::Filter { never, always } => {
            let names = |list: Vec<String>| -> Vec<String> {
                list.iter()
                    .map(|n| target.canonical(n).to_string())
                    .collect()
            };
            ApprovalRule::Filter {
                never: names(never),
                always: names(always),
            }
        }
        other => other,
    }
}

/// The first tool `new` stops gating that `old` gated, `OTHER` for the
/// label's remaining tools.
fn loosened(target: &LabelTarget, old: &ApprovalRule, new: &ApprovalRule) -> Option<String> {
    let (old, new) = (
        canonical(target, old.clone()),
        canonical(target, new.clone()),
    );
    let mut names: Vec<&str> = Vec::new();
    for rule in [&old, &new] {
        if let ApprovalRule::Filter { never, always } = rule {
            names.extend(never.iter().chain(always).map(String::as_str));
        }
    }
    names.push(OTHER);
    names
        .into_iter()
        .find(|t| old.requires(t, t) && !new.requires(t, t))
        .map(str::to_string)
}

/// Two entries of `written` that name one target (module doc), as the
/// `400`'s message — for every writer. Only where the write changed one
/// of the two (an entry `before` does not hold as written): two stored
/// before this rule, carried unchanged, do not block an unrelated write.
pub(in crate::web) fn duplicates(
    snap: &Snapshot,
    written: &[ThreadMcp],
    before: &[ThreadMcp],
) -> Result<(), String> {
    let mut seen: Vec<(Target, &str, bool)> = Vec::new();
    for entry in written {
        let label = entry.server_label.trim();
        let target = label_target(snap, label).target;
        let changed = !before.contains(entry);
        if let Some((_, first, _)) = seen
            .iter()
            .find(|(t, _, was)| *t == target && (changed || *was))
        {
            return Err(if *first == label {
                format!(
                    "mcp_tools: '{label}' is listed twice — one entry per server, with one \
                     require_approval"
                )
            } else {
                format!(
                    "mcp_tools: '{first}' and '{label}' name the same server (by its tool \
                     prefix and by its name) — one entry per server, with one \
                     require_approval"
                )
            });
        }
        seen.push((target, label, changed));
    }
    Ok(())
}

/// The floor after the owner wrote `written` over `before` (module doc):
/// each target the owner wrote takes the owner's rule, a target the owner
/// removed has none, and a target the write did not touch (one a device
/// removed since, say) keeps what it had.
pub(in crate::web) fn owner_floor(
    snap: &Snapshot,
    written: &[ThreadMcp],
    before: &[ThreadMcp],
    floor: &[ThreadMcp],
) -> Vec<ThreadMcp> {
    let touched: Vec<Target> = written
        .iter()
        .chain(before)
        .map(|e| label_target(snap, &e.server_label).target)
        .collect();
    floor
        .iter()
        .filter(|e| !touched.contains(&label_target(snap, &e.server_label).target))
        .cloned()
        .chain(written.iter().map(|e| ThreadMcp {
            server_label: e.server_label.trim().to_string(),
            allowed_tools: None,
            require_approval: e.require_approval.clone(),
        }))
        .collect()
}

/// Whether `written` holds the same rules for `target` as `before` (by
/// label and `require_approval`, whatever their order).
fn carried(snap: &Snapshot, target: &Target, written: &[ThreadMcp], before: &[ThreadMcp]) -> bool {
    let rules = |list: &[ThreadMcp]| {
        let mut out: Vec<(String, Option<String>)> = list
            .iter()
            .filter(|e| label_target(snap, &e.server_label).target == *target)
            .map(|e| {
                (
                    e.server_label.trim().to_string(),
                    e.require_approval.as_ref().map(|v| v.to_string()),
                )
            })
            .collect();
        out.sort();
        out
    };
    rules(written) == rules(before)
}

/// Refuse the first entry of `written` that loosens one of its baselines
/// (module doc): `before` is what the thread or folder stores, `floor` its
/// owner's floor, `folder` the folder of a thread (its floor and defaults
/// count for a target the thread has neither for).
pub(in crate::web) async fn check(
    state: &SharedState,
    written: &[ThreadMcp],
    before: &[ThreadMcp],
    floor: &[ThreadMcp],
    folder: Option<i64>,
) -> Result<(), Response> {
    let snap = state.snapshot();
    // Which tools a collision moved, for the rules' spellings.
    let agg = state.mcp.aggregate(&snap).await;
    let mut defaults: Option<Vec<ThreadMcp>> = None;
    for entry in written {
        let target = label_target_in(&snap, &agg, &entry.server_label);
        // Carried unchanged: nothing written. Only when the write carries
        // every rule `before` holds for the server, and adds none: dropping
        // one of two stored entries for it (written before two were
        // refused) leaves the other's rule to compare with.
        if carried(&snap, &target.target, written, before) {
            continue;
        }
        let of = |list: &[ThreadMcp], from: From| -> Vec<(ThreadMcp, From)> {
            list.iter()
                .filter(|b| label_target(&snap, &b.server_label).target == target.target)
                .map(|b| (b.clone(), from))
                .collect()
        };
        let mut base = of(floor, From::Floor);
        base.extend(of(before, From::Stored));
        if base.is_empty() {
            if let Some(id) = folder {
                if defaults.is_none() {
                    let f = crate::store::get_chat_folder(&state.db, id)
                        .await
                        .map_err(|e| {
                            err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
                        })?;
                    defaults = Some(
                        f.map(|f| {
                            let mut all = f.approval_floor;
                            all.extend(f.defaults.mcp_tools.unwrap_or_default());
                            all
                        })
                        .unwrap_or_default(),
                    );
                }
                base = of(defaults.as_deref().unwrap_or_default(), From::Folder);
            }
        }
        let new = entry.approval_rule();
        for (old, from) in base {
            if let Some(tool) = loosened(&target, &old.approval_rule(), &new) {
                let label = entry.server_label.trim();
                let what = if tool == OTHER {
                    "its other tools".to_string()
                } else {
                    format!("'{tool}'")
                };
                return Err(err_json(
                    StatusCode::FORBIDDEN,
                    CODE,
                    format!(
                        "'{label}': {what} would need approval for fewer calls than {} — a \
                         device may only tighten require_approval; the owner may loosen it. \
                         Nothing was changed",
                        from.words()
                    ),
                ));
            }
        }
    }
    Ok(())
}
