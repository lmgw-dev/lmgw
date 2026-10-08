//! Which deletes are the page's own, on plain values (reviews CL-16,
//! CL-22). A read of the open thread that answers 404 while the page's own
//! delete of it is under way is that delete, not one made elsewhere: the
//! page falls through as its delete does, and keeps no draft for a new
//! chat. A folder delete that takes its threads is the page's own delete of
//! the open thread when that is in the folder, marked before the folder's
//! POST goes out.

use super::super::chat::ChatThread;

/// The thread a delete of folder `folder` (with its threads when
/// `with_threads`) takes as the page's own: the open one, `open`, when it
/// is in that folder. Marked before the POST goes out.
pub(in crate::pages) fn own_folder_delete(
    open: Option<&ChatThread>,
    folder: i64,
    with_threads: bool,
) -> Option<i64> {
    open.filter(|t| with_threads && t.folder_id == Some(folder))
        .map(|t| t.id)
}

/// Whether a 404 for the open thread `id` says it was deleted elsewhere,
/// the page's own delete of `deleting` under way.
pub(super) fn gone_elsewhere(id: i64, deleting: Option<i64>) -> bool {
    deleting != Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(id: i64, folder: Option<i64>) -> ChatThread {
        ChatThread {
            id,
            folder_id: folder,
            ..Default::default()
        }
    }

    /// Review CL-16: the owner deletes the open thread's folder with its
    /// threads. The open thread is marked as the page's own, so a read of
    /// it that answers 404 before the folder's POST does is no delete
    /// elsewhere.
    #[test]
    fn the_page_s_own_folder_delete_is_no_delete_elsewhere() {
        let t = open(7, Some(3));
        let marked = own_folder_delete(Some(&t), 3, true);
        assert_eq!(marked, Some(7));
        assert!(!gone_elsewhere(7, marked));
        // Another writer's delete of it is one.
        assert!(gone_elsewhere(7, None));
        assert!(gone_elsewhere(7, Some(8)));
    }

    #[test]
    fn a_folder_delete_that_keeps_its_threads_or_holds_not_the_open_one_marks_nothing() {
        let t = open(7, Some(3));
        assert_eq!(own_folder_delete(Some(&t), 3, false), None, "threads kept");
        assert_eq!(own_folder_delete(Some(&t), 4, true), None, "another folder");
        assert_eq!(own_folder_delete(Some(&open(7, None)), 3, true), None);
        assert_eq!(own_folder_delete(None, 3, true), None, "nothing open");
    }
}
