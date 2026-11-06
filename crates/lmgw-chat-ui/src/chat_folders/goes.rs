//! Where a New chat goes, and the way back to an ongoing conversation
//! (chat-plane design §9.10).
//!
//! - **New chat follows the open thread's folder**: the list's New chat and
//!   the narrow head's start the chat in the folder of the thread that is
//!   open, as the folder menu's "New chat here" does; in an ongoing folder
//!   the new chat is the folder's current conversation, as its "New
//!   conversation" makes it ([`NewChatIn::Ongoing`]). With nothing open, an
//!   unfiled thread or a temporary one open, it stays unfiled. A temporary
//!   chat has no folder, and an Admin chat does not go into one yet.
//! - **Back to the ongoing conversation**: while the open thread is no
//!   ongoing folder's current thread, the head offers that current thread
//!   ([`back_to`]); with several ongoing folders, the one whose current
//!   thread was active last.

use super::{ChatThread, FolderInfo};

/// Where a New chat goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NewChatIn {
    /// No folder: the page's own New chat (`Opener::new_thread`).
    Unfiled,
    /// Folder `id`, from its defaults ("New chat here").
    Folder(i64),
    /// Ongoing folder `id`'s new current conversation ("New conversation").
    Ongoing(i64),
}

/// Where a New chat of `kind` ("chat", "admin", "temporary") goes while
/// thread `open` is open, the list knowing `folders`. A folder the list does
/// not know (deleted meanwhile) is no folder.
pub(crate) fn new_chat_goes(
    kind: &str,
    open: Option<&ChatThread>,
    folders: &[FolderInfo],
) -> NewChatIn {
    if kind != "chat" {
        return NewChatIn::Unfiled;
    }
    let Some(id) = open.filter(|t| !t.temporary).and_then(|t| t.folder_id) else {
        return NewChatIn::Unfiled;
    };
    match folders.iter().find(|f| f.id == id) {
        Some(f) if f.ongoing.is_some() => NewChatIn::Ongoing(id),
        Some(_) => NewChatIn::Folder(id),
        None => NewChatIn::Unfiled,
    }
}

/// The New chat's label, naming where it goes: "New chat", or "New chat in
/// <folder>".
pub(crate) fn new_chat_label(goes: NewChatIn, folders: &[FolderInfo]) -> String {
    let id = match goes {
        NewChatIn::Unfiled => return "New chat".to_string(),
        NewChatIn::Folder(id) | NewChatIn::Ongoing(id) => id,
    };
    folders
        .iter()
        .find(|f| f.id == id)
        .map(|f| format!("New chat in {}", f.name))
        .unwrap_or_else(|| "New chat".to_string())
}

/// The ongoing conversation the head goes back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackTo {
    pub folder: i64,
    pub name: String,
    /// Its current thread, which the button opens.
    pub thread: i64,
}

/// What the head's "Back to" opens while thread `open` is open: `None`
/// when `open` is an ongoing folder's current thread, or no ongoing folder
/// has one. With several, the one whose current thread was active last
/// (its `updated_at` in `threads`, the list as read); a current thread the
/// list does not carry (the archived view is shown) counts as the oldest,
/// and a tie goes to the folder listed first.
pub(crate) fn back_to(
    folders: &[FolderInfo],
    threads: &[ChatThread],
    open: Option<i64>,
) -> Option<BackTo> {
    let currents = folders
        .iter()
        .filter_map(|f| f.current_thread().map(|t| (f, t)));
    if open.is_some_and(|o| currents.clone().any(|(_, t)| t == o)) {
        return None;
    }
    let active = |id: i64| {
        threads
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.updated_at.as_str())
    };
    let mut best: Option<(&FolderInfo, i64, Option<&str>)> = None;
    for (f, t) in currents {
        let at = active(t);
        if best.is_none_or(|(_, _, b)| at > b) {
            best = Some((f, t, at));
        }
    }
    best.map(|(f, thread, _)| BackTo {
        folder: f.id,
        name: f.name.clone(),
        thread,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_folders::{is_current, set_current};
    use lmgw_api_types::chat_folders::FolderOngoing;

    fn folder(id: i64, name: &str, current: Option<Option<i64>>) -> FolderInfo {
        FolderInfo {
            id,
            name: name.into(),
            ongoing: current.map(|c| FolderOngoing {
                idle_minutes: 0,
                current_thread_id: c,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_roll_over_makes_the_new_thread_the_folders_current_one() {
        let mut fs = vec![
            folder(1, "Plain", None),
            folder(2, "Daily", Some(Some(10))),
            folder(3, "Empty", Some(None)),
        ];
        set_current(&mut fs, 2, 11);
        set_current(&mut fs, 3, 12);
        set_current(&mut fs, 1, 13);
        set_current(&mut fs, 99, 14);
        assert_eq!(fs[0].ongoing, None);
        assert!(is_current(&fs, 11) && !is_current(&fs, 10));
        assert!(is_current(&fs, 12));
        assert!(!is_current(&fs, 13) && !is_current(&fs, 14));
    }

    fn thread(id: i64, folder: Option<i64>, at: &str) -> ChatThread {
        ChatThread {
            id,
            folder_id: folder,
            updated_at: at.into(),
            ..Default::default()
        }
    }

    fn folders() -> Vec<FolderInfo> {
        vec![folder(1, "Work", None), folder(2, "Kai", Some(Some(20)))]
    }

    #[test]
    fn a_new_chat_goes_into_the_open_threads_folder() {
        let f = folders();
        let in_work = thread(10, Some(1), "");
        assert_eq!(
            new_chat_goes("chat", Some(&in_work), &f),
            NewChatIn::Folder(1)
        );
        assert_eq!(new_chat_label(NewChatIn::Folder(1), &f), "New chat in Work");
    }

    #[test]
    fn in_an_ongoing_folder_it_is_the_new_current_conversation() {
        let f = folders();
        // Any thread of the folder, its current one or an older one.
        for id in [20, 21] {
            let t = thread(id, Some(2), "");
            assert_eq!(new_chat_goes("chat", Some(&t), &f), NewChatIn::Ongoing(2));
        }
        assert_eq!(new_chat_label(NewChatIn::Ongoing(2), &f), "New chat in Kai");
    }

    #[test]
    fn unfiled_none_open_or_an_unknown_folder_stay_unfiled() {
        let f = folders();
        assert_eq!(new_chat_goes("chat", None, &f), NewChatIn::Unfiled);
        let loose = thread(30, None, "");
        assert_eq!(new_chat_goes("chat", Some(&loose), &f), NewChatIn::Unfiled);
        let gone = thread(31, Some(99), "");
        assert_eq!(new_chat_goes("chat", Some(&gone), &f), NewChatIn::Unfiled);
        assert_eq!(new_chat_label(NewChatIn::Unfiled, &f), "New chat");
    }

    #[test]
    fn temporary_and_admin_chats_stay_unfiled() {
        let f = folders();
        let in_work = thread(10, Some(1), "");
        assert_eq!(
            new_chat_goes("temporary", Some(&in_work), &f),
            NewChatIn::Unfiled
        );
        assert_eq!(
            new_chat_goes("admin", Some(&in_work), &f),
            NewChatIn::Unfiled
        );
        // A temporary chat open: it has no folder, whatever it says.
        let temp = ChatThread {
            temporary: true,
            ..thread(-1, Some(1), "")
        };
        assert_eq!(new_chat_goes("chat", Some(&temp), &f), NewChatIn::Unfiled);
    }

    #[test]
    fn back_to_offers_the_current_thread_from_anywhere_else() {
        let f = folders();
        let threads = [thread(20, Some(2), "2026-11-04 09:00:00")];
        let back = BackTo {
            folder: 2,
            name: "Kai".into(),
            thread: 20,
        };
        assert_eq!(back_to(&f, &threads, Some(10)), Some(back.clone()));
        assert_eq!(back_to(&f, &threads, None), Some(back));
        // In it already: no way back to show.
        assert_eq!(back_to(&f, &threads, Some(20)), None);
    }

    #[test]
    fn back_to_needs_an_ongoing_folder_with_a_current_thread() {
        let f = vec![folder(1, "Work", None), folder(2, "Kai", Some(None))];
        assert_eq!(back_to(&f, &[], Some(10)), None);
    }

    #[test]
    fn with_several_the_last_active_current_thread_wins() {
        let f = vec![
            folder(1, "Kai", Some(Some(10))),
            folder(2, "Car", Some(Some(20))),
            folder(3, "Home", Some(Some(30))),
        ];
        let threads = [
            thread(10, Some(1), "2026-11-04 08:00:00"),
            thread(20, Some(2), "2026-11-04 09:30:00"),
            // 30 is not in the list read (the archived view): the oldest.
        ];
        assert_eq!(back_to(&f, &threads, Some(5)).map(|b| b.folder), Some(2));
        // Neither listed: the first folder.
        assert_eq!(back_to(&f, &[], Some(5)).map(|b| b.folder), Some(1));
        // In one of them: none, though the others have current threads.
        assert_eq!(back_to(&f, &threads, Some(10)), None);
    }
}
