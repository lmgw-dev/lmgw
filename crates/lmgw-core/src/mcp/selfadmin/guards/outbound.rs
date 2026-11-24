//! The outbound guard on what the three model settings name (security
//! round B, L6, and the review of the round). `OUTBOUND_SETTINGS` keeps a
//! model's call from choosing where callers' document pages, pictures and
//! audio go; the same effect is open through the tools that change what the
//! chosen name answers with: another upstream or upstream model for that
//! alias (`lmgw__model_set`), other candidates (`lmgw__candidate_alias_set`),
//! another fallback, container or row behind a local model's name
//! (`lmgw__local_model_set`, `lmgw__aux_model_set`,
//! `lmgw__audio_model_set`), another address for the upstream it routes to
//! (`lmgw__upstream_set`). The models the settings reach are the chosen one
//! and every hop of its fallback chain, a candidate alias's candidates
//! among them, as the snapshot has them when the call is made, with the
//! upstreams their routes go to. A call is refused when:
//!
//! - it changes such a row as the row is now (looked up by its `id`), or
//!   as it would be (the name it gives): a rename away from a protected
//!   name, and a new row under one, are both refused, so a rename followed
//!   by a create cannot put another row behind the setting's name;
//! - it would make an upstream answer a protected name by its passthrough
//!   prefix (a new or changed upstream that exposes its catalog under the
//!   prefix the name starts with, or under none while a protected name is a
//!   bare passthrough one or answers to nothing);
//! - for the upstream a protected model routes to, it changes anything but
//!   the key and `timeout_ms`, or disables it while a protected name is one
//!   of its passthrough models (another upstream could then answer that
//!   name).
//!
//! A local, aux or audio row, an alias and a candidate alias stay locked
//! whole: a row's container image, its run arguments and its server
//! arguments can each send what it reads elsewhere, and disabling a row or
//! an alias lets another model of the same spelling answer its name.
//!
//! A preview writes nothing and passes. Each refusal names the setting and
//! the model it protects, and says the owner changes it in the dashboard.
//!
//! Not yet covered: an upstream whose catalog moves (not a change lmgw
//! makes), and the fallback chain as it will be after the call (only as it
//! is now; a fallback added to a protected row is refused with the row).

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::config::{AuxKind, HopKey, Snapshot};
use crate::runtime::Class;

/// Which setting a reached model or upstream serves, and through what.
#[derive(Debug, Clone)]
struct Why {
    /// The setting, as Settings › Documents names it.
    setting: &'static str,
    /// The model it names.
    model: String,
    /// The hop this is, when it is not the model itself: "its fallback
    /// 'x'", "its upstream 'y'".
    via: Option<String>,
}

/// What the outbound settings reach now.
#[derive(Debug, Default)]
struct Reached {
    /// Every name of the chains, lowercased.
    names: HashMap<String, Why>,
    /// The names as the settings and chains spell them, with whether each
    /// resolves to a passthrough model or to nothing (what a bare
    /// passthrough upstream would answer).
    spelled: Vec<(String, bool)>,
    keys: Vec<(HopKey, Why)>,
    upstreams: HashMap<i64, Why>,
    /// Upstreams a protected passthrough name routes to.
    passthrough_on: HashSet<i64>,
}

const SETTINGS: [&str; 3] = [
    "the page-reading model",
    "the picture-description model",
    "the speech-to-text model",
];

fn reached(snap: &Snapshot) -> Reached {
    let d = &snap.settings.documents;
    let mut r = Reached::default();
    for (setting, start) in
        SETTINGS
            .into_iter()
            .zip([&d.page_reader, &d.picture_alias, &d.asr_alias])
    {
        let model = start.trim().to_string();
        let why = |via: Option<String>| Why {
            setting,
            model: model.clone(),
            via,
        };
        let mut name = model.clone();
        let mut via = None;
        // Each hop once: the chain is as long as the rows that exist.
        while !name.is_empty() && !r.names.contains_key(&name.to_lowercase()) {
            let here = why(via.clone());
            r.names.insert(name.to_lowercase(), here.clone());
            let key = snap.hop_key(&name).or_else(|| snap.disabled_key(&name));
            r.spelled.push((
                name.clone(),
                matches!(key, None | Some(HopKey::Passthrough(..))),
            ));
            if let Ok((_, route)) = snap.hop(&name) {
                let u = &route.upstream;
                r.upstreams
                    .entry(u.id)
                    .or_insert_with(|| why(Some(format!("its upstream '{}'", u.name))));
            }
            let Some(key) = key else {
                break;
            };
            if let Some(n) = snap.hop_name(&key) {
                r.names.entry(n.to_lowercase()).or_insert(here.clone());
            }
            if let HopKey::Passthrough(up, _) = &key {
                r.passthrough_on.insert(*up);
                r.upstreams
                    .entry(*up)
                    .or_insert_with(|| why(Some(format!("its upstream {up}"))));
            }
            // A candidate alias's candidates are local chat rows behind its
            // name (the review: its alternates).
            if let HopKey::Candidate(id) = &key {
                if let Some(ca) = snap.candidate_aliases.values().find(|c| c.id == *id) {
                    for m in &ca.candidates {
                        let cand = why(Some(format!("its candidate '{m}'")));
                        r.names
                            .entry(snap.local_public_name(m).to_lowercase())
                            .or_insert(cand.clone());
                        r.keys.push((HopKey::Local(Class::Chat, m.clone()), cand));
                    }
                }
            }
            let next = snap.fallback_of(&key).map(str::to_string);
            r.keys.push((key, here));
            match next {
                Some(n) => {
                    via = Some(format!("its fallback '{n}'"));
                    name = n;
                }
                None => break,
            }
        }
    }
    r
}

fn arg_str<'a>(args: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    args.get(k).and_then(Value::as_str)
}

fn arg_bool(args: &Map<String, Value>, k: &str) -> Option<bool> {
    args.get(k).and_then(|v| {
        v.as_bool().or_else(|| match v.as_str() {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        })
    })
}

fn arg_id(args: &Map<String, Value>) -> Option<i64> {
    args.get("id").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

/// The `model_id` of the local row of `class` with `id`, as it is now.
fn row_model_id(snap: &Snapshot, class: Class, id: i64) -> Option<String> {
    match class {
        Class::Chat => snap
            .local_models
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.model_id.clone()),
        Class::Aux => snap
            .aux_models
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.model_id.clone()),
        Class::Audio => snap
            .audio_models
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.model_id.clone()),
        Class::Image => snap
            .image_models
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.model_id.clone()),
    }
}

/// The names a row of `class` called `model_id` would answer to.
fn public_names(snap: &Snapshot, class: Class, model_id: &str) -> Vec<String> {
    match class {
        Class::Chat => vec![snap.local_public_name(model_id)],
        Class::Aux => [AuxKind::Embed, AuxKind::Rerank, AuxKind::Decision]
            .into_iter()
            .map(|k| snap.aux_name_for(k, model_id))
            .collect(),
        Class::Audio => vec![snap.audio_public_name(model_id)],
        Class::Image => vec![snap.image_public_name(model_id)],
    }
}

impl Reached {
    fn key(&self, key: &HopKey) -> Option<&Why> {
        self.keys.iter().find(|(k, _)| k == key).map(|(_, w)| w)
    }

    fn name(&self, name: &str) -> Option<&Why> {
        self.names.get(&name.trim().to_lowercase())
    }

    /// A local row of `class`, as it is (`id`) and as it would be
    /// (`model_id`, also the lookup key without an `id`).
    fn local(&self, snap: &Snapshot, class: Class, args: &Map<String, Value>) -> Option<&Why> {
        let now = arg_id(args)
            .and_then(|id| row_model_id(snap, class, id))
            .and_then(|m| self.key(&HopKey::Local(class, m)));
        let then = arg_str(args, "model_id").and_then(|m| {
            let m = m.trim();
            self.key(&HopKey::Local(class, m.to_string())).or_else(|| {
                public_names(snap, class, m)
                    .iter()
                    .find_map(|n| self.name(n))
            })
        });
        now.or(then)
    }

    /// An upstream call (module doc): the reached row beyond its key,
    /// timeout and disable; any row that would answer a protected name by
    /// its prefix.
    fn upstream(&self, snap: &Snapshot, args: &Map<String, Value>) -> Option<&Why> {
        let action = arg_str(args, "action").unwrap_or("");
        let id = arg_id(args);
        let current = id.and_then(|i| snap.upstreams.get(&i));
        if let Some(why) = id.and_then(|i| self.upstreams.get(&i)) {
            let disables = action == "disable" || arg_bool(args, "enabled") == Some(false);
            let enables = action == "enable" || arg_bool(args, "enabled") == Some(true);
            let only_free = args.keys().all(|k| {
                matches!(
                    k.as_str(),
                    "action" | "id" | "api_key" | "timeout_ms" | "enabled"
                )
            });
            let free = matches!(action, "update" | "disable")
                && only_free
                && !enables
                && !(disables && id.is_some_and(|i| self.passthrough_on.contains(&i)));
            return (!free).then_some(why);
        }
        // The row as it would be: does its catalog's prefix catch a
        // protected name?
        let enabled = match action {
            "enable" => true,
            "disable" => false,
            _ => arg_bool(args, "enabled")
                .or(current.map(|u| u.enabled))
                .unwrap_or(true),
        };
        let exposes = arg_bool(args, "expose_all")
            .or(current.map(|u| u.expose_all))
            .unwrap_or(false);
        if !enabled || !exposes || action == "delete" {
            return None;
        }
        let prefix = arg_str(args, "expose_prefix")
            .map(str::to_string)
            .or(current.map(|u| u.expose_prefix.clone()))
            .unwrap_or_default();
        let prefix = prefix.trim_matches('/').to_lowercase();
        self.spelled
            .iter()
            .find(|(n, bare)| {
                if prefix.is_empty() {
                    *bare
                } else {
                    n.to_lowercase()
                        .strip_prefix(&prefix)
                        .and_then(|r| r.strip_prefix('/'))
                        .is_some_and(|r| !r.is_empty())
                }
            })
            .and_then(|(n, _)| self.name(n))
    }
}

/// Why `name(args)` may not run: it changes what an outbound setting's
/// model answers with (module doc).
pub(in crate::mcp::selfadmin) fn refusal(
    snap: &Snapshot,
    name: &str,
    args: Option<&Map<String, Value>>,
) -> Option<String> {
    let empty = Map::new();
    let args = args.unwrap_or(&empty);
    if arg_str(args, "action") == Some("preview") {
        return None;
    }
    let r = reached(snap);
    if r.names.is_empty() {
        return None;
    }
    let named = |k: &str| arg_str(args, k).and_then(|a| r.name(a));
    let id = arg_id(args);
    let hit = match name {
        "lmgw__model_set" => named("alias").or_else(|| id.and_then(|i| r.key(&HopKey::Alias(i)))),
        "lmgw__candidate_alias_set" => {
            named("alias").or_else(|| id.and_then(|i| r.key(&HopKey::Candidate(i))))
        }
        "lmgw__local_model_set" => r.local(snap, Class::Chat, args),
        "lmgw__aux_model_set" => r.local(snap, Class::Aux, args),
        "lmgw__audio_model_set" => r.local(snap, Class::Audio, args),
        "lmgw__upstream_set" => r.upstream(snap, args),
        _ => None,
    }?;
    let via = hit
        .via
        .as_deref()
        .map(|v| format!(", through {v}"))
        .unwrap_or_default();
    let free = if name == "lmgw__upstream_set" && id.is_some_and(|i| r.upstreams.contains_key(&i)) {
        " (that upstream's key and timeout_ms can still be changed here, and it can be \
         disabled unless a protected name is one of its passthrough models)"
    } else {
        ""
    };
    Some(format!(
        "{name} cannot change this through lmgw's admin tools: it would change where {} \
         '{}', chosen on Settings › Documents, sends callers' documents{via}{free}. The owner \
         changes it in the dashboard. Nothing was changed",
        hit.setting, hit.model
    ))
}

#[cfg(test)]
mod tests;
