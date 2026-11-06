//! A rename never breaks a chain (openai egress design §14.2, decision 27):
//! renaming a plain alias, a candidate alias or a local row, and changing a
//! class prefix, rewrite every other row's and alias's fallback that names
//! what was renamed, in the rename's own write transaction.
//!
//! "Names it" is by identity: a stored fallback is rewritten when it
//! resolves, in the snapshot before the write, to a [`HopKey`] the write
//! renames. A name that resolves to something else (an alias that shadows a
//! local name) or to nothing is left as it is.

use std::collections::HashSet;

use crate::config::{HopKey, Snapshot};
use crate::runtime::Class;
use crate::state::SharedState;
use crate::store::FallbackRewrite;

/// What a rename rewrites, and the sentences its answer says it with.
#[derive(Debug, Clone, Default)]
pub(crate) struct Rewrites {
    pub(crate) rows: Vec<FallbackRewrite>,
    pub(crate) notes: Vec<String>,
}

/// The rewrites renaming each `(key, new name)` makes. `except` is the row
/// the rename itself writes (with its own fallback as the patch says), so
/// its stored fallback is not rewritten underneath it.
pub(crate) fn rename_rewrites(
    snap: &Snapshot,
    renamed: &[(HopKey, String)],
    except: Option<&HopKey>,
) -> Rewrites {
    let mut out = Rewrites::default();
    if renamed.is_empty() {
        return out;
    }
    for o in snap.fallback_owners() {
        if except == Some(&o.key) {
            continue;
        }
        let Some(target) = snap.any_hop_key(&o.fallback) else {
            continue;
        };
        let Some((_, new)) = renamed.iter().find(|(k, _)| *k == target) else {
            continue;
        };
        out.push(&o, new);
    }
    out
}

impl Rewrites {
    /// `o`'s fallback names `new` from now on (nothing when it does already).
    fn push(&mut self, o: &crate::config::FallbackOwner, new: &str) {
        if new == o.fallback {
            return;
        }
        self.notes.push(format!(
            "fallback rewritten: {} now names '{new}' (was '{}')",
            o.label, o.fallback
        ));
        self.rows.push(FallbackRewrite {
            table: o.table,
            id: o.id,
            was: o.fallback.clone(),
            fallback: new.to_string(),
        });
    }
}

/// The rewrites an upstream's new `expose_prefix` makes: every passthrough
/// id of it is renamed, as a class prefix renames its local names, so a
/// fallback that names one (`kilo/openai/x`) follows it. `new_prefix` is
/// the prefix as it will be saved.
///
/// A name under a prefix is that upstream's by its spelling. A **bare**
/// upstream (no prefix) answers every name nothing else does, so a
/// dangling fallback meant for something else resolves to it too: only a
/// name its catalog lists is one of its models, and is rewritten. Its
/// catalog unreadable, a name it would only answer is left as it is, and
/// the answer says so.
pub(crate) async fn upstream_prefix_rewrites(
    state: &SharedState,
    snap: &Snapshot,
    upstream_id: i64,
    new_prefix: &str,
) -> Rewrites {
    use lmgw_api_types::public_names::prefixed;
    let mut out = Rewrites::default();
    let u = match snap.upstreams.get(&upstream_id) {
        Some(u) if u.prefix() != new_prefix.trim_matches('/') => u,
        _ => return out,
    };
    let mut listed: Option<Result<HashSet<String>, String>> = None;
    for o in snap.fallback_owners() {
        let Some(HopKey::Passthrough(up, model)) = snap.any_hop_key(&o.fallback) else {
            continue;
        };
        if up != upstream_id {
            continue;
        }
        if u.prefix().is_empty() {
            if listed.is_none() {
                listed = Some(
                    crate::catalog::upstream_models(state, u)
                        .await
                        .map(|ms| ms.into_iter().map(|m| m.id).collect()),
                );
            }
            match listed.as_ref() {
                Some(Ok(ids)) if ids.contains(&model) => {}
                Some(Ok(_)) => continue,
                Some(Err(why)) => {
                    out.notes.push(format!(
                        "fallback not rewritten: {} names '{}', which '{}' answers as it answers \
                         any name nothing else does, and its catalog could not be read to tell \
                         whether that is one of its models ({why})",
                        o.label, o.fallback, u.name
                    ));
                    continue;
                }
                None => continue,
            }
        }
        out.push(&o, &prefixed(new_prefix, &model));
    }
    out
}

/// The rewrites a settings save makes when it changes a class prefix: every
/// local row of the class gets a new public name. `after` is the settings
/// as they will be saved.
pub(crate) fn prefix_rewrites(snap: &Snapshot, after: &crate::config::Settings) -> Rewrites {
    use lmgw_api_types::public_names::{aux_prefix, prefixed};
    let mut renamed: Vec<(HopKey, String)> = Vec::new();
    if after.router.public_prefix != snap.settings.router.public_prefix {
        for m in &snap.local_models {
            renamed.push((
                HopKey::Local(Class::Chat, m.model_id.clone()),
                prefixed(&after.router.public_prefix, &m.model_id),
            ));
        }
    }
    let aux = &after.aux_router;
    for m in &snap.aux_models {
        let new = prefixed(
            aux_prefix(m.kind.as_str(), &aux.public_prefix, &aux.decision_prefix),
            &m.model_id,
        );
        if new != snap.aux_public_name(m) {
            renamed.push((HopKey::Local(Class::Aux, m.model_id.clone()), new));
        }
    }
    if after.audio.public_prefix != snap.settings.audio.public_prefix {
        for m in &snap.audio_models {
            renamed.push((
                HopKey::Local(Class::Audio, m.model_id.clone()),
                prefixed(&after.audio.public_prefix, &m.model_id),
            ));
        }
    }
    if after.image.public_prefix != snap.settings.image.public_prefix {
        for m in &snap.image_models {
            renamed.push((
                HopKey::Local(Class::Image, m.model_id.clone()),
                prefixed(&after.image.public_prefix, &m.model_id),
            ));
        }
    }
    rename_rewrites(snap, &renamed, None)
}
