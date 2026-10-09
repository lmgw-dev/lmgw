//! What the panel's editor shows, decided without a view: the pick, the
//! list updated with a stored row before the editor mounts on it, when a
//! switch asks first (the profiles review's fix 2), and where a new
//! profile's Discard goes back to.

use lmgw_api_types::chat_profiles::{Profile, ProfileList};

/// What the editor pane shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pick {
    Nothing,
    New,
    Id(i64),
}

/// The list with `p` in place of the row with its id, or added: in the
/// gateway's order (the name, ASCII case folded as SQLite's `NOCASE`, then
/// the id), so the list reads as the next fetch will answer it.
pub(super) fn upsert(list: &mut ProfileList, p: Profile) {
    list.profiles.retain(|x| x.id != p.id);
    let key = |x: &Profile| (x.name.to_ascii_lowercase(), x.id);
    let at = list.profiles.partition_point(|x| key(x) < key(&p));
    list.profiles.insert(at, p);
}

/// The pick a stored profile `id` (a create, a save, a reset) moves the
/// pane to: `None` when the pane shows it already — its editor has taken
/// the stored row itself, and mounting it again from a list would show
/// what the list held. A create's pick (`New`) moves to the new row's id.
pub(super) fn after_stored(pick: Pick, id: i64) -> Option<Pick> {
    (pick != Pick::Id(id)).then_some(Pick::Id(id))
}

/// Whether going from `from` to `to` asks first: only to another pick, and
/// only while the editor has unsaved edits.
pub(super) fn asks(from: Pick, to: Pick, dirty: bool) -> bool {
    dirty && from != to
}

/// Where Discard on a new profile's draft takes the pane: back to `before`,
/// the pick "New profile" left, while the list still holds it; else nothing.
pub(super) fn back_from_new(before: Pick, list: &ProfileList) -> Pick {
    match before {
        Pick::Id(id) if list.profiles.iter().any(|p| p.id == id) => before,
        _ => Pick::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: i64, name: &str) -> Profile {
        Profile {
            id,
            name: name.into(),
            ..Profile::default()
        }
    }

    fn names(l: &ProfileList) -> Vec<(i64, &str)> {
        l.profiles.iter().map(|x| (x.id, x.name.as_str())).collect()
    }

    #[test]
    fn a_stored_row_replaces_its_old_one_in_the_gateway_s_order() {
        let mut l = ProfileList {
            profiles: vec![p(1, "Concise"), p(3, "pirate")],
            default_profile_id: None,
        };
        // A create: added where the next fetch puts it.
        upsert(&mut l, p(7, "Butler"));
        assert_eq!(names(&l), [(7, "Butler"), (1, "Concise"), (3, "pirate")]);
        // A save that renames: the old row goes, the new one is in order.
        let mut renamed = p(1, "Zen");
        renamed.persona = "Calm.".into();
        upsert(&mut l, renamed);
        assert_eq!(names(&l), [(7, "Butler"), (3, "pirate"), (1, "Zen")]);
        assert_eq!(
            l.profiles[2].persona, "Calm.",
            "the stored row, not the old one"
        );
        // Same name in another case: the id decides.
        upsert(&mut l, p(2, "PIRATE"));
        assert_eq!(names(&l)[1..3], [(2, "PIRATE"), (3, "pirate")]);
    }

    #[test]
    fn the_editor_mounts_again_only_on_another_pick() {
        // A save or a reset of the shown row: no remount.
        assert_eq!(after_stored(Pick::Id(4), 4), None);
        // A create: the pane moves to the new row.
        assert_eq!(after_stored(Pick::New, 9), Some(Pick::Id(9)));
        // A built-in added back while another row was shown.
        assert_eq!(after_stored(Pick::Id(4), 1), Some(Pick::Id(1)));
    }

    #[test]
    fn a_new_profile_s_discard_goes_back_to_the_pick_before_it() {
        let l = ProfileList {
            profiles: vec![p(1, "Concise"), p(3, "pirate")],
            default_profile_id: None,
        };
        assert_eq!(back_from_new(Pick::Id(3), &l), Pick::Id(3));
        assert_eq!(back_from_new(Pick::Nothing, &l), Pick::Nothing);
        // Gone meanwhile (deleted elsewhere): nothing, not an empty pane on a dead id.
        assert_eq!(back_from_new(Pick::Id(9), &l), Pick::Nothing);
        // Never back into a draft.
        assert_eq!(back_from_new(Pick::New, &l), Pick::Nothing);
    }

    #[test]
    fn a_switch_asks_only_with_unsaved_edits_and_another_pick() {
        assert!(asks(Pick::Id(1), Pick::Id(2), true));
        assert!(asks(Pick::Id(1), Pick::New, true));
        assert!(asks(Pick::New, Pick::Id(2), true));
        assert!(!asks(Pick::Id(1), Pick::Id(1), true));
        assert!(!asks(Pick::Id(1), Pick::Id(2), false));
    }
}
