//! What the progress knows of a get between the jobs frames (find-a-model
//! design D92, D93, revised in WP6's fix round): each file's state from its
//! transfer on the feed, its download row and the get's stage, and when to
//! read the get's status. Pure, so the rules are unit-tested without a
//! browser.

/// Where one file of the get is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileState {
    /// Its transfer is on the feed.
    Moving,
    /// It came down whole.
    Done,
    /// It was on the disk before the get.
    Here,
    /// It stopped short and waits: the get is paused, or names it waiting.
    Paused,
    /// Its download failed.
    Failed,
    /// It hasn't started yet.
    Waiting,
}

/// One file's line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FileLine {
    pub file: String,
    pub done: u64,
    pub total: Option<u64>,
    pub state: FileState,
}

/// What is known of one file now.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Known<'a> {
    /// Its download row's status, the newest read: `queued`, `downloading`,
    /// `done`, `failed` or `update_available`.
    pub status: &'a str,
    /// Its transfer's bytes on the feed now.
    pub live: Option<(u64, Option<u64>)>,
    /// Its transfer's last bytes seen on the feed.
    pub last: Option<(u64, Option<u64>)>,
    /// The get's stage is `paused`.
    pub paused: bool,
    /// The get's `waiting` names this file.
    pub named: bool,
}

/// One file's line from what is known of it. A transfer that left the feed
/// is done only when its last bytes reached its total; a row that says
/// `done` with no transfer seen was on the disk before the get.
pub(crate) fn file_line(file: &str, k: Known<'_>) -> FileLine {
    let line = |state, (done, total): (u64, Option<u64>)| FileLine {
        file: file.to_string(),
        done,
        total,
        state,
    };
    let last = k.last.unwrap_or((0, None));
    if let Some(l) = k.live {
        return line(FileState::Moving, l);
    }
    match k.status {
        "failed" => return line(FileState::Failed, last),
        "done" | "update_available" => {
            return match k.last {
                Some((d, t)) => line(FileState::Done, (t.unwrap_or(d), t)),
                None => line(FileState::Here, (0, None)),
            }
        }
        _ => {}
    }
    match k.last {
        Some((d, Some(t))) if d >= t => line(FileState::Done, (t, Some(t))),
        Some(_) if k.paused || k.named => line(FileState::Paused, last),
        None if k.paused => line(FileState::Paused, last),
        _ => line(FileState::Waiting, last),
    }
}

/// The reads of the get's status (D93): one at a time, and none lost. A
/// read wanted while one is out goes out when that one is back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EndRead {
    reading: bool,
    pending: bool,
}

/// What follows a status read that came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    /// The get ended, or its job is on the feed: the feed says when to
    /// read again.
    Nothing,
    /// A read was wanted while this one was out: again now.
    Now,
    /// It still runs and its job isn't on the feed (not listed yet, or the
    /// frame that dropped it came before this read was answered): again in
    /// a moment.
    Soon,
}

impl EndRead {
    /// A read is wanted: `true` when it goes out now, else it waits for the
    /// one that is out.
    pub fn want(&mut self) -> bool {
        if self.reading {
            self.pending = true;
            false
        } else {
            self.reading = true;
            true
        }
    }

    /// A read came back: `running` when it said the get runs, `on_feed`
    /// when the get's job is on the feed now.
    pub fn back(&mut self, running: bool, on_feed: bool) -> Next {
        self.reading = false;
        let pending = std::mem::take(&mut self.pending);
        match (running, pending, on_feed) {
            (false, _, _) => Next::Nothing,
            (true, true, _) => Next::Now,
            (true, false, false) => Next::Soon,
            (true, false, true) => Next::Nothing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1_000_000_000;

    fn k(status: &str) -> Known<'_> {
        Known {
            status,
            ..Known::default()
        }
    }

    #[test]
    fn a_file_is_done_only_when_it_reached_its_total() {
        let moving = Known {
            live: Some((G, Some(5 * G))),
            ..k("downloading")
        };
        assert_eq!(file_line("w.gguf", moving).state, FileState::Moving);
        let whole = Known {
            last: Some((5 * G, Some(5 * G))),
            ..k("downloading")
        };
        let l = file_line("w.gguf", whole);
        assert_eq!((l.state, l.done), (FileState::Done, 5 * G));
        // Left the feed short of its total, nothing says why yet.
        let short = Known {
            last: Some((2 * G, Some(5 * G))),
            ..k("downloading")
        };
        assert_eq!(file_line("w.gguf", short).state, FileState::Waiting);
    }

    #[test]
    fn a_short_file_is_paused_or_failed_as_the_get_and_its_row_say() {
        let last = Some((2 * G, Some(5 * G)));
        let paused = Known {
            last,
            paused: true,
            ..k("queued")
        };
        let l = file_line("w.gguf", paused);
        assert_eq!((l.state, l.done), (FileState::Paused, 2 * G));
        let named = Known {
            last,
            named: true,
            ..k("queued")
        };
        assert_eq!(file_line("w.gguf", named).state, FileState::Paused);
        // Named while it never moved: it waits for its start.
        assert_eq!(
            file_line(
                "w.gguf",
                Known {
                    named: true,
                    ..k("queued")
                }
            )
            .state,
            FileState::Waiting
        );
        let failed = Known {
            last,
            ..k("failed")
        };
        assert_eq!(file_line("w.gguf", failed).state, FileState::Failed);
    }

    #[test]
    fn a_file_already_on_the_disk_says_so() {
        let l = file_line("mmproj.gguf", k("done"));
        assert_eq!(l.state, FileState::Here);
        assert_eq!((l.done, l.total), (0, None));
        assert_eq!(file_line("w.gguf", k("queued")).state, FileState::Waiting);
    }

    #[test]
    fn a_read_wanted_while_one_is_out_is_not_lost() {
        let mut r = EndRead::default();
        assert!(r.want());
        // The frame that drops the job comes while the read is out.
        assert!(!r.want());
        assert_eq!(r.back(true, false), Next::Now);
        assert!(r.want());
        assert_eq!(r.back(false, false), Next::Nothing);
    }

    #[test]
    fn a_running_answer_without_the_job_on_the_feed_reads_again_soon() {
        let mut r = EndRead::default();
        assert!(r.want());
        assert_eq!(r.back(true, false), Next::Soon);
        assert!(r.want());
        assert_eq!(r.back(true, true), Next::Nothing);
        assert!(r.want(), "nothing left out");
    }
}
