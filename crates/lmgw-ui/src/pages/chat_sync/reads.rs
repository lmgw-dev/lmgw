//! What a `chat` frame has the follower read, on plain values (reviews
//! CL-11, CL-22): the whole list, or only the rows of the threads whose
//! messages moved, and the open thread when the frame names it.

use lmgw_api_types::ChatChanged;

/// What one frame asks to be read.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Asked {
    /// The whole list: the frame names a thread or a folder, or says
    /// resync.
    pub list: bool,
    /// Only these threads' rows: the frame names only messages, which moved
    /// their threads' times and nothing else the list shows (`rows`).
    pub rows: Vec<i64>,
    /// The open thread, when the frame names it. A temporary chat (a
    /// negative id) is the gateway's, in memory: never read.
    pub open: Option<i64>,
}

/// What frame `c` asks to be read, thread `open` open.
pub(super) fn asked(c: &ChatChanged, open: Option<i64>) -> Asked {
    let rows_only = c.threads.is_empty() && c.folders.is_empty() && !c.resync;
    Asked {
        list: !rows_only,
        rows: if rows_only {
            c.messages.clone()
        } else {
            Vec::new()
        },
        open: open.filter(|id| *id > 0 && c.names_thread(*id)),
    }
}

/// Whether named rows are read by themselves, `whole_out` reads of the
/// whole list being out: only when none is, since the rows read's ticket
/// would drop that read's answer and what it brings beyond the rows.
pub(super) fn rows_alone(whole_out: u32) -> bool {
    whole_out == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(threads: &[i64], folders: &[i64], messages: &[i64], resync: bool) -> ChatChanged {
        ChatChanged {
            threads: threads.to_vec(),
            folders: folders.to_vec(),
            messages: messages.to_vec(),
            resync,
        }
    }

    /// Review CL-11: a device's turn names only messages. The page reads
    /// those threads' rows, not the whole list, and not the open thread
    /// unless it is named.
    #[test]
    fn a_frame_of_messages_alone_reads_only_their_rows() {
        let a = asked(&frame(&[], &[], &[3, 7], false), Some(5));
        assert_eq!(
            a,
            Asked {
                list: false,
                rows: vec![3, 7],
                open: None
            }
        );
        let a = asked(&frame(&[], &[], &[3, 5], false), Some(5));
        assert_eq!(a.rows, vec![3, 5]);
        assert_eq!(a.open, Some(5), "the open thread is named: read too");
        assert!(!a.list);
    }

    #[test]
    fn a_thread_a_folder_or_a_resync_reads_the_whole_list() {
        for f in [
            frame(&[3], &[], &[3], false),
            frame(&[], &[2], &[], false),
            frame(&[], &[], &[], true),
            frame(&[], &[2], &[7], false),
        ] {
            let a = asked(&f, None);
            assert!(a.list && a.rows.is_empty(), "{f:?}");
        }
        assert_eq!(
            asked(&ChatChanged::resync(), Some(5)).open,
            Some(5),
            "a resync reads the open thread"
        );
        assert_eq!(asked(&frame(&[5], &[], &[], false), Some(5)).open, Some(5));
    }

    #[test]
    fn a_temporary_chat_is_never_read() {
        assert_eq!(asked(&ChatChanged::resync(), Some(-2)).open, None);
        assert_eq!(asked(&frame(&[], &[], &[-2], false), Some(-2)).open, None);
    }

    /// Review CL-11's ticket: rows are read alone only while no read of the
    /// whole list is out; else the whole list is read again.
    #[test]
    fn rows_are_read_alone_only_while_no_whole_list_read_is_out() {
        assert!(rows_alone(0));
        assert!(!rows_alone(1));
        assert!(!rows_alone(2));
    }
}
