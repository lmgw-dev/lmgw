//! What VRAM admission gave a start that fits only partially on the card
//! (`vram::partial`, the owner's ruling of 2026-11-11): staged by admission
//! where it decides the start, taken by the entry that start creates, and
//! kept on it for as long as it lives. The start renders sd-server's
//! `--max-vram` from it and carries its warning through every launch of the
//! entry, `ready` included; the ledger charges the entry what it says.

use std::sync::MutexGuard;

use super::*;
use crate::runtime::Class;

/// A partial start's share of the card ([`Registry::stage_grant`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialGrant {
    /// What admission reserved for it on the card, and what the ledger
    /// charges the entry while it lives.
    pub bytes: u64,
    /// The budget sd-server is held to (`--max-vram`): `bytes` less the
    /// row's measured idle residency — sd-server's own context, which its
    /// budget does not count — or all of `bytes` where that is not measured.
    pub max_vram_bytes: u64,
    /// "It will work but only fits partially on the GPU, expect slowdowns:
    /// …", for the entry's warnings; empty where the start got all of it
    /// and only its placement is rendered.
    pub warning: String,
    /// A llama-server start's placement, which lmgw fitted where llama.cpp's
    /// `--fit` would have fitted it to the driver's free memory
    /// (`vram::partial::llama`): rendered as explicit values with `--fit
    /// off` in every launch of the entry. `None` for sd-server.
    pub llama: Option<LlamaPlacement>,
}

pub use crate::runtime::argv::LlamaPlacement;

/// What admission staged for the entry a start creates
/// ([`Registry::stage_grant`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Staged {
    /// Less than the whole, or a fitted placement: [`PartialGrant`].
    pub grant: Option<PartialGrant>,
    /// A warning a start that got all of its charge goes ahead with: the
    /// learned growth of other programs' use that was not kept free beside
    /// it (`vram::headroom`). Kept on the entry for its life, like a grant's.
    pub note: Option<String>,
}

impl Registry {
    /// Stage `staged` for the next entry `class`/`model_id` creates, under
    /// admission's reservation `id`. Replaces what an earlier reservation
    /// staged for the same model.
    pub fn stage_grant(&self, class: Class, model_id: &str, id: u64, staged: Staged) {
        self.staged()
            .insert((class, model_id.to_string()), (id, staged));
    }

    /// Drop what reservation `id` staged and no entry took: its start never
    /// created one (it joined a start already up, or never ran).
    pub fn unstage_grant(&self, class: Class, model_id: &str, id: u64) {
        let mut staged = self.staged();
        let key = (class, model_id.to_string());
        if staged
            .get(&key)
            .is_some_and(|(staged_id, _)| *staged_id == id)
        {
            staged.remove(&key);
        }
    }

    /// Take what was staged for `key`, for the entry being created. Called
    /// with the map lock held; this lock is only ever taken after it.
    pub(super) fn take_staged(&self, key: &Key) -> Staged {
        self.staged()
            .remove(key)
            .map(|(_, s)| s)
            .unwrap_or_default()
    }

    fn staged(&self) -> MutexGuard<'_, HashMap<Key, (u64, Staged)>> {
        self.staged.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The grant of the start `of` names, while the entry is still its own.
    pub(super) fn grant_of(&self, of: super::start_of::StartOf<'_>) -> Option<PartialGrant> {
        self.map()
            .get(of.key)
            .filter(|e| of.owns(e))
            .and_then(|e| e.grant.clone())
    }
}

/// The words every [`unheld_note`] ends its first clause with.
const UNHELD: &str = "VRAM admission gave this start";

/// Whether `warning` is an [`unheld_note`]: the entry is not held to its
/// grant, so nothing it does teaches the row's working budget.
pub fn is_unheld_note(warning: &str) -> bool {
    warning.contains(UNHELD)
}

/// What holds sd-server in a start given `g` ([`held_by`]).
pub(super) enum HeldBy {
    /// The grant's `--max-vram`, as admission rendered it.
    Grant,
    /// The row's own `max_vram`, which admission gave it as the grant's
    /// budget (`vram::partial`, the row's own budget).
    OwnAsGranted,
    /// The row's own `max_vram` (key, value as written), which is not what
    /// admission gave: the row changed after the decision.
    Own(String, String),
    /// Nothing: this sd-server build has no `--max-vram`.
    NoFlag,
}

/// What holds sd-server in a start of `img` given `g`: the row's own
/// `max_vram` wins at render, and a build without the flag cannot be told.
pub(super) fn held_by(img: &crate::runtime::argv::ImageArgs, g: &PartialGrant) -> HeldBy {
    if let Some((k, v)) = img
        .args
        .iter()
        .find(|(k, _)| img.caps.resolve_key(k) == "max_vram")
    {
        if crate::vram::partial::own_budget(&img.args) == Some(Some(g.max_vram_bytes)) {
            return HeldBy::OwnAsGranted;
        }
        let v = v.as_str().map_or_else(|| v.to_string(), str::to_string);
        return HeldBy::Own(k.clone(), v);
    }
    match img.caps.get("max_vram") {
        Some(_) => HeldBy::Grant,
        None => HeldBy::NoFlag,
    }
}

/// Why sd-server is not held to `g` in this start, when it is not: the row's
/// own `max_vram` differs from what admission gave (an edit after the
/// decision), or a build without the flag cannot be told. A row whose own
/// `max_vram` admission gave as the budget is held to it as given.
pub(super) fn unheld_note(
    img: &crate::runtime::argv::ImageArgs,
    g: &PartialGrant,
) -> Option<String> {
    let given = crate::hf::fmt_bytes(g.bytes);
    match held_by(img, g) {
        HeldBy::Grant | HeldBy::OwnAsGranted => None,
        HeldBy::Own(k, v) => Some(format!(
            "{k} {v} in its args is what sd-server is held to, not the {given} on the card \
             {UNHELD}"
        )),
        HeldBy::NoFlag => Some(format!(
            "this sd-server build has no --max-vram, so it is not held to the {given} on the \
             card {UNHELD}: it sizes itself to the driver's free memory"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdcpp_caps::SdcppCaps;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn img(args: serde_json::Value, caps: Arc<SdcppCaps>) -> crate::runtime::argv::ImageArgs {
        crate::runtime::argv::ImageArgs {
            files: Default::default(),
            args: args.as_object().cloned().unwrap_or_default(),
            caps,
            held: None,
        }
    }

    fn grant(max_vram_bytes: u64) -> PartialGrant {
        PartialGrant {
            bytes: 3 * GIB,
            max_vram_bytes,
            warning: String::new(),
            llama: None,
        }
    }

    /// What holds sd-server, and the note where it is not the grant: the
    /// grant's flag; the row's own `max_vram` as admission gave it (no
    /// note); the row's own one that is not (an edit after the decision);
    /// a build without the flag.
    #[test]
    fn what_holds_sd_server() {
        let embedded = SdcppCaps::embedded();
        let a = img(
            serde_json::json!({"offload_to_cpu": true}),
            embedded.clone(),
        );
        assert!(matches!(held_by(&a, &grant(2 * GIB)), HeldBy::Grant));
        assert_eq!(unheld_note(&a, &grant(2 * GIB)), None);

        let a = img(serde_json::json!({"max-vram": "2"}), embedded.clone());
        assert!(matches!(held_by(&a, &grant(2 * GIB)), HeldBy::OwnAsGranted));
        assert_eq!(unheld_note(&a, &grant(2 * GIB)), None);
        let note = unheld_note(&a, &grant(5 * GIB / 2)).unwrap();
        assert!(
            is_unheld_note(&note) && note.starts_with("max-vram 2 in its args"),
            "{note}"
        );

        let a = img(serde_json::json!({}), Arc::new(SdcppCaps::parse("")));
        assert!(matches!(held_by(&a, &grant(2 * GIB)), HeldBy::NoFlag));
        let note = unheld_note(&a, &grant(2 * GIB)).unwrap();
        assert!(
            is_unheld_note(&note) && note.contains("has no --max-vram"),
            "{note}"
        );
    }
}
