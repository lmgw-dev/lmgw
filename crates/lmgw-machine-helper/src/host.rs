//! The seam every effect of the helper goes through: the programs it starts,
//! the files it writes, the journal. [`crate::real::RealHost`] is this
//! machine; the tests state a fake, so no test runs dnf, rpmkeys, systemctl
//! or systemd-run, or writes under `/etc`.
//!
//! Reads go through `lmgw_machine::Facts` (root's through
//! `SystemFacts::for_root`), as the check's do.

use std::io;

use lmgw_machine::helper::Stream;
use lmgw_machine::plan::steps::FileWrite;

/// How a program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    /// Its exit code; `None` when a signal ended it.
    pub code: Option<i32>,
    /// The signal that ended it.
    pub signal: Option<i32>,
}

impl Ended {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    pub fn exited(code: i32) -> Self {
        Self {
            code: Some(code),
            signal: None,
        }
    }
}

/// The journal's priorities the helper uses (syslog's numbers).
pub mod priority {
    pub const ERR: u8 = 3;
    pub const WARNING: u8 = 4;
    pub const NOTICE: u8 = 5;
    pub const INFO: u8 = 6;
}

pub trait Host {
    /// Run `argv` (its first element an absolute path, never looked up in
    /// `PATH`) from an empty environment with only
    /// `lmgw_machine::facts::ROOT_ENV`, no input, `/` as its working
    /// directory and no descriptor beyond the three standard ones; hand
    /// `line` every line of its output as it comes, whole, and wait for it
    /// to end: for the program, not for its pipes, which a process it left
    /// behind may hold open (D101).
    fn run(&self, argv: &[String], line: &mut dyn FnMut(Stream, &str)) -> io::Result<Ended>;

    /// Write `w` as §3.2 says: a new file never over one that is there, or
    /// lmgw's own file replaced, either way through a temporary file in the
    /// same directory, so no half-written file is ever at the path.
    fn write_file(&self, w: &FileWrite) -> io::Result<()>;

    /// One journal entry: `message` with `priority`, the event's JSON line
    /// as a field of its own.
    fn journal(&self, priority: u8, message: &str, event: &str) -> io::Result<()>;
}
