//! Which Chat folder a client's ongoing conversation lives in (kai §4,
//! kai-mobile P5): one rule over the folder list that every client runs
//! alike, so the phone and the desktop pick the same folder, whichever
//! paired first.
//!
//! **The lookup** ([`pick`]), by the conversation's folder name (names are
//! not unique) and the folder id the client remembers:
//! 1. the remembered folder when it is ready ([`Readiness::Ready`]),
//!    whatever its name now;
//! 2. else the ready folder of that name with the lowest id, the other
//!    namesakes logged; the client remembers it;
//! 3. else the remembered folder, not ready;
//! 4. else the plain folder of that name with the lowest id;
//! 5. else the client creates a plain folder of that name ([`Pick::Create`]),
//!    sending only the name: a client never chooses the conversation's
//!    model, marking a folder ongoing needs one, and a folder's retention is
//!    the owner's.
//!
//! A remembered id the list no longer has counts as none, and the client
//! forgets it. [`Pick::remember`] is the remembered id after the lookup,
//! both rules applied: the client writes it when it differs from the one it
//! passed in. **Racing creates converge** (Kai's review KAI-2 K2-22): a
//! lookup never replaces the client's own create on its way, and after a
//! create the client lists again and runs [`pick`] once more. A client
//! remembers only a ready folder, so the plain folders two creates made
//! (a reconnect, two devices' first runs) are namesakes of step 4, and every
//! client lands on the one with the lowest id; the other stays, plain.

use lmgw_api_types::chat::Folder;

/// Whether a folder can hold the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Ongoing, with a model: `current` answers.
    Ready,
    /// No model (a folder a client created, or the owner's plain one).
    NeedsSetup,
    /// A model, but not marked as an ongoing conversation.
    NotOngoing,
}

/// `folder`'s readiness: ongoing (`ongoing` set) and a model
/// (`defaults.model_alias`, not blank).
pub fn readiness(folder: &Folder) -> Readiness {
    let model = folder
        .defaults
        .model_alias
        .as_deref()
        .is_some_and(|m| !m.trim().is_empty());
    match (folder.ongoing.is_some(), model) {
        (true, true) => Readiness::Ready,
        (_, false) => Readiness::NeedsSetup,
        (false, true) => Readiness::NotOngoing,
    }
}

/// [`pick`]'s answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// Use this folder; `namesakes` are the ids of the other folders of the
    /// name, lowest first, for the log; `remember` as
    /// [`Pick::remember`] says.
    Use {
        folder: Box<Folder>,
        namesakes: Vec<i64>,
        remember: Option<i64>,
    },
    /// No folder of the name and none remembered (or the remembered one is
    /// gone): create a plain one, and remember none.
    Create,
}

impl Pick {
    /// The remembered folder id after this lookup, which the client writes
    /// when it differs from the one it passed in: the chosen folder when it
    /// is ready; else the remembered id while lmgw still lists it (then the
    /// chosen folder is that one, not ready); else none, which forgets an id
    /// lmgw no longer has. A folder that is not ready is used but never
    /// remembered anew, which is what makes racing creates converge (module
    /// doc).
    pub fn remember(&self) -> Option<i64> {
        match self {
            Pick::Use { remember, .. } => *remember,
            Pick::Create => None,
        }
    }
}

/// The lookup (module doc) over `folders` (`GET /chat/api/folders`), the
/// conversation's folder `name` (matched exactly, both sides trimmed) and
/// the `remembered` folder id.
pub fn pick(folders: &[Folder], name: &str, remembered: Option<i64>) -> Pick {
    let name = name.trim();
    let mut named: Vec<&Folder> = folders.iter().filter(|f| f.name.trim() == name).collect();
    named.sort_by_key(|f| f.id);
    let remembered = remembered.and_then(|id| folders.iter().find(|f| f.id == id));
    let ready = |f: &&Folder| readiness(f) == Readiness::Ready;
    let chosen = remembered
        .filter(ready)
        .or_else(|| named.iter().copied().find(ready))
        .or(remembered)
        .or_else(|| named.first().copied());
    match chosen {
        Some(f) => Pick::Use {
            folder: Box::new(f.clone()),
            namesakes: named
                .iter()
                .map(|n| n.id)
                .filter(|id| *id != f.id)
                .collect(),
            remember: if ready(&f) {
                Some(f.id)
            } else {
                remembered.map(|r| r.id)
            },
        },
        None => Pick::Create,
    }
}

#[cfg(test)]
mod tests {
    use lmgw_api_types::chat_folders::FolderOngoing;

    use super::*;

    fn folder(id: i64, name: &str) -> Folder {
        Folder {
            id,
            name: name.into(),
            ..Folder::default()
        }
    }

    /// A folder that is ready: a model, ongoing with `idle` minutes.
    fn ready(id: i64, name: &str, idle: i64) -> Folder {
        let mut f = folder(id, name);
        f.defaults.model_alias = Some("kai-chat".into());
        f.ongoing = Some(FolderOngoing {
            idle_minutes: idle,
            current_thread_id: None,
        });
        f
    }

    fn used(p: &Pick) -> (i64, Vec<i64>) {
        match p {
            Pick::Use {
                folder, namesakes, ..
            } => (folder.id, namesakes.clone()),
            Pick::Create => panic!("expected a folder, got Create"),
        }
    }

    #[test]
    fn of_repeated_names_the_ready_folder_with_the_lowest_id_wins() {
        let folders = [
            folder(2, "Kai"),
            ready(9, "Kai", 120),
            ready(5, "Kai", 30),
            ready(1, "Work", 0),
        ];
        let p = pick(&folders, "Kai", None);
        assert_eq!(
            used(&p),
            (5, vec![2, 9]),
            "a plain folder with a lower id does not count; the others are logged"
        );
        assert_eq!(p.remember(), Some(5));
    }

    #[test]
    fn the_remembered_folder_wins_while_it_is_ready_whatever_its_name() {
        let folders = [ready(3, "Kai", 0), ready(7, "Assistant", 120)];
        let p = pick(&folders, "Kai", Some(7));
        assert_eq!(used(&p), (7, vec![3]));
        assert_eq!(p.remember(), Some(7));
    }

    #[test]
    fn a_remembered_folder_that_is_not_ready_yields_to_a_ready_namesake() {
        let mut plain = folder(4, "Kai");
        plain.defaults.model_alias = Some("kai-chat".into());
        let folders = [folder(1, "Kai"), plain.clone(), ready(8, "Kai", 120)];
        let p = pick(&folders, "Kai", Some(4));
        assert_eq!(used(&p).0, 8);
        assert_eq!(p.remember(), Some(8), "the ready one replaces it");
        // With no ready namesake it stays the remembered one, not the lower
        // plain one, and stays remembered.
        let folders = [folder(1, "Kai"), plain];
        let p = pick(&folders, "Kai", Some(4));
        assert_eq!(used(&p).0, 4);
        assert_eq!(p.remember(), Some(4));
    }

    #[test]
    fn a_remembered_id_lmgw_no_longer_has_is_forgotten() {
        let folders = [folder(6, "Kai"), folder(2, "Kai")];
        let p = pick(&folders, "Kai", Some(9));
        assert_eq!(used(&p).0, 2, "the lowest plain namesake");
        assert_eq!(p.remember(), None, "9 is forgotten, 2 not remembered");
        let p = pick(&[ready(3, "Kai", 0)], "Kai", Some(9));
        assert_eq!(p.remember(), Some(3), "a ready namesake replaces it");
        assert_eq!(pick(&[], "Kai", Some(9)).remember(), None);
    }

    #[test]
    fn with_no_ready_folder_the_plain_one_with_the_lowest_id_and_with_none_a_new_one() {
        let folders = [folder(6, "Kai"), folder(2, " Kai "), folder(1, "kai")];
        assert_eq!(used(&pick(&folders, "Kai", None)), (2, vec![6]));
        assert_eq!(
            pick(&[folder(1, "kai"), ready(2, "Work", 0)], "Kai", None),
            Pick::Create,
            "names match exactly (trimmed)"
        );
        assert_eq!(
            pick(&[], "Kai", Some(3)),
            Pick::Create,
            "a remembered id lmgw no longer has"
        );
    }

    /// K2-22: two creates raced (a reconnect, or the phone's and the
    /// desktop's first runs). Each client lists again after its create and
    /// lands on the lower id, whichever folder it made itself.
    #[test]
    fn racing_creates_converge_on_the_lowest_namesake() {
        let after = [folder(4, "Kai"), folder(5, "Kai")];
        for made in [4, 5] {
            let first = pick(&[folder(made, "Kai")], "Kai", None);
            assert_eq!(first.remember(), None, "a plain folder is not remembered");
            let p = pick(&after, "Kai", first.remember());
            assert_eq!(used(&p), (4, vec![5]), "created {made}");
        }
        // Once the owner readies the other one, both move to it.
        let readied = [folder(4, "Kai"), ready(5, "Kai", 120)];
        assert_eq!(used(&pick(&readied, "Kai", None)).0, 5);
    }

    #[test]
    fn readiness_needs_a_model_and_ongoing() {
        assert_eq!(readiness(&folder(1, "Kai")), Readiness::NeedsSetup);
        let mut f = folder(1, "Kai");
        f.defaults.model_alias = Some("m".into());
        assert_eq!(readiness(&f), Readiness::NotOngoing);
        assert_eq!(readiness(&ready(1, "Kai", 0)), Readiness::Ready);
        let mut blank = ready(1, "Kai", 0);
        blank.defaults.model_alias = Some("  ".into());
        assert_eq!(readiness(&blank), Readiness::NeedsSetup, "a blank model");
    }
}
