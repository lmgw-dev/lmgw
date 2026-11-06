//! The real process's stdout (§3.1, D100): a queue in front of the pipe, so
//! a reader that stops reading never stops the helper, and the end waits
//! only for what the pipe takes. Real pipes of the test's own; nothing runs.

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::time::Duration;

use lmgw_machine::helper::{Event, Exit, Stream};
use lmgw_machine_helper::stdout::Queued;
use lmgw_machine_helper::Ended;

use super::support::{helper_into, in_the_unit, FakeHost, Machine};

/// A pipe: (read end, write end).
fn pipe() -> (std::fs::File, OwnedFd) {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe2 writes two descriptors into `fds`.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: pipe2 just returned them, and nothing else owns them.
    unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            OwnedFd::from_raw_fd(fds[1]),
        )
    }
}

/// `f` on a thread, which must end within `limit`: a hang fails the test
/// instead of stalling the suite.
fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit)
        .expect("it waited on a reader that reads nothing")
}

/// A reader that never reads: 4 MB of lines go in at once, and the end
/// returns with the pipe full.
#[test]
fn a_reader_that_never_reads_stalls_nothing() {
    let (mut r, w) = pipe();
    let fd = std::os::fd::AsRawFd::as_raw_fd(&w);
    let line = format!("{}\n", "x".repeat(1023));
    within(Duration::from_secs(60), move || {
        let mut q = Queued::over(fd);
        for _ in 0..4096 {
            q.write_all(line.as_bytes()).unwrap();
            q.flush().unwrap();
        }
        q.settle();
    });
    // What the pipe took, and not the rest: the reader gets the start.
    let mut first = vec![0u8; 1024];
    r.read_exact(&mut first).unwrap();
    assert_eq!(&first[..1023], "x".repeat(1023).as_bytes());
    drop(r);
    drop(w);
}

/// A pipe with room for all of it: every byte, in order, before the end
/// returns.
#[test]
fn what_the_pipe_takes_is_written_whole_before_the_end() {
    let (mut r, w) = pipe();
    let fd = std::os::fd::AsRawFd::as_raw_fd(&w);
    let mut want = Vec::new();
    for i in 0..300 {
        want.extend_from_slice(format!("line {i} {}\n", "y".repeat(80)).as_bytes());
    }
    let sent = want.clone();
    within(Duration::from_secs(60), move || {
        let mut q = Queued::over(fd);
        for l in sent.split_inclusive(|&b| b == b'\n') {
            q.write_all(l).unwrap();
        }
        q.settle();
    });
    // Under the pipe's 64 KiB, so all of it is there without a reader.
    assert!(want.len() < 64 * 1024);
    let mut got = vec![0u8; want.len()];
    r.read_exact(&mut got).unwrap();
    assert_eq!(got, want);
    drop(w);
}

/// A reader that is gone: nothing blocks, nothing fails.
#[test]
fn a_reader_that_is_gone_takes_nothing_and_stops_nothing() {
    let (r, w) = pipe();
    drop(r);
    let fd = std::os::fd::AsRawFd::as_raw_fd(&w);
    within(Duration::from_secs(60), move || {
        let mut q = Queued::over(fd);
        q.write_all(b"nobody reads this\n").unwrap();
        q.settle();
    });
    drop(w);
}

/// `run` whose stdout nobody reads: the install printing 2 MB, far past the
/// pipe's room, runs to its end after the key's import, and `run` finishes,
/// so the plan never stops halfway and the unit lets go of its name and the
/// inhibitor.
#[test]
fn a_run_whose_reader_stopped_runs_every_step() {
    let (r, w) = pipe();
    let fd = std::os::fd::AsRawFd::as_raw_fd(&w);
    let (exit, runs) = within(Duration::from_secs(120), move || {
        let m = Machine::fedora();
        let plan = m.plan(false);
        let host = FakeHost::new();
        host.answer(|argv| {
            if argv[0] == "/usr/bin/dnf5" {
                let lines = (0..10_000)
                    .map(|i| (Stream::Stdout, format!("dnf line {i} {}", "z".repeat(200))))
                    .collect();
                (lines, Ok(Ended::exited(0)))
            } else {
                (Vec::new(), Ok(Ended::exited(0)))
            }
        });
        let mut q = Queued::over(fd);
        let o = helper_into(&["run", &plan.hash()], &in_the_unit(), &m, &host, &mut q);
        q.settle();
        assert!(matches!(o.events.last(), Some(Event::Finished { exit: 0 })));
        (o.exit, host.runs().len())
    });
    assert_eq!(exit, Exit::Done);
    assert_eq!(runs, 2, "rpmkeys --import and dnf5 install both ran");
    drop(r);
    drop(w);
}
