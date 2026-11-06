//! The helper's stdout in the real process (§3.1, D100): every line goes
//! into an unbounded queue, and a thread of its own writes the queue to the
//! descriptor. A reader that stops reading (a frozen or hung app, whose scope
//! `apply` runs in; `apply` itself, frozen with it, for `run`) fills the pipe
//! and stalls that thread alone: the steps go on to their end, and the unit
//! ends and lets go of its name and the inhibitor.
//!
//! For a pipe or a terminal the thread writes through a descriptor of its
//! own that does not block: `/proc/self/fd/1` opened again with
//! `O_NONBLOCK`, so the description the helper shares with systemd-run and
//! the inhibitor keeps its flags. Where that open is refused, a copy of the
//! descriptor gets `O_NONBLOCK` on the shared description, and
//! [`Queued::settle`] puts the old flags back. A socket gets `MSG_DONTWAIT`
//! instead. The thread waits for room with `poll`. A regular file is written
//! through a copy of its descriptor (opened again it would lose its offset
//! and `O_APPEND`), and a file never waits for a reader.
//!
//! At the end [`Queued::settle`] waits until the queue is in the pipe or the
//! pipe is full, then the process exits without the thread: what a reader
//! that stopped never took is in the journal, which is the whole record.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};

/// How the thread reaches the reader.
enum Target {
    /// A descriptor that does not block: the original opened again, or a
    /// copy whose shared description got `O_NONBLOCK`. A pipe, a terminal.
    NonBlocking(OwnedFd),
    /// A regular file (a copy of the descriptor): written through to the
    /// end, since it takes every byte without a reader.
    File(OwnedFd),
    /// A socket (a copy of the descriptor): `send` with `MSG_DONTWAIT`.
    Socket(OwnedFd),
    /// No reader: a pipe whose reader is gone (`ENXIO` on the open), or no
    /// descriptor at all. Lines are dropped.
    Gone,
}

#[derive(Default)]
struct State {
    /// Chunks queued and not yet written or dropped.
    pending: usize,
    /// The thread waits for room in a full pipe.
    stalled: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut State)) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut st);
        self.changed.notify_all();
    }
}

/// The queue in front of a descriptor. [`Write`] never blocks and never
/// fails; [`Self::settle`] is the end.
pub struct Queued {
    tx: Option<mpsc::Sender<Vec<u8>>>,
    shared: Arc<Shared>,
    /// The descriptor and its flags before, when its shared description got
    /// `O_NONBLOCK`.
    restore: Option<(RawFd, libc::c_int)>,
}

impl Queued {
    /// The process's stdout.
    pub fn stdout() -> Self {
        Self::over(1)
    }

    /// The descriptor `fd`, which stays open; the thread writes through a
    /// descriptor of its own.
    pub fn over(fd: RawFd) -> Self {
        let (target, restore) = target(fd);
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let s = shared.clone();
        std::thread::spawn(move || writer(target, rx, &s));
        Self {
            tx: Some(tx),
            shared,
            restore: restore.map(|flags| (fd, flags)),
        }
    }

    /// Wait until every queued line is written, or the reader takes nothing
    /// now (the pipe is full). Never longer: the thread may still be waiting
    /// for room when this returns, and the process exits without it.
    pub fn settle(mut self) {
        self.tx.take();
        {
            let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            while st.pending > 0 && !st.stalled {
                st = self
                    .shared
                    .changed
                    .wait(st)
                    .unwrap_or_else(|e| e.into_inner());
            }
        }
        if let Some((fd, flags)) = self.restore {
            // SAFETY: F_SETFL on the descriptor this process was given.
            unsafe {
                libc::fcntl(fd, libc::F_SETFL, flags);
            }
        }
    }
}

impl Write for Queued {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(tx) = &self.tx {
            self.shared.update(|st| st.pending += 1);
            if tx.send(buf.to_vec()).is_err() {
                self.shared.update(|st| st.pending -= 1);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// How the thread writes to `fd`, and the flags to put back when the shared
/// description got `O_NONBLOCK`.
fn target(fd: RawFd) -> (Target, Option<libc::c_int>) {
    // SAFETY: fstat writes only into the struct it is given; a descriptor
    // that is not open is an error, not undefined behaviour.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return (Target::Gone, None);
    }
    match st.st_mode & libc::S_IFMT {
        libc::S_IFSOCK => return (dup(fd).map_or(Target::Gone, Target::Socket), None),
        libc::S_IFREG => return (dup(fd).map_or(Target::Gone, Target::File), None),
        _ => {}
    }
    let path = std::ffi::CString::new(format!("/proc/self/fd/{fd}")).expect("no NUL in it");
    // SAFETY: a NUL-terminated path that outlives the call; open returns a
    // new descriptor or -1.
    let new = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY,
        )
    };
    if new >= 0 {
        // SAFETY: open just returned it, and nothing else owns it.
        return (
            Target::NonBlocking(unsafe { OwnedFd::from_raw_fd(new) }),
            None,
        );
    }
    if io::Error::last_os_error().raw_os_error() == Some(libc::ENXIO) {
        // A pipe (or FIFO) with no reader left.
        return (Target::Gone, None);
    }
    // The open was refused (an SELinux rule, no /proc): the shared
    // description itself, without blocking, until settle.
    let Some(copy) = dup(fd) else {
        return (Target::Gone, None);
    };
    // SAFETY: F_GETFL and F_SETFL on a descriptor this process holds.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0 {
        (Target::NonBlocking(copy), Some(flags))
    } else {
        // Blocking after all: a stall then looks like a write in progress,
        // and settle waits for it.
        (Target::File(copy), None)
    }
}

fn dup(fd: RawFd) -> Option<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor or -1.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    // SAFETY: fcntl just returned it, and nothing else owns it.
    (new >= 0).then(|| unsafe { OwnedFd::from_raw_fd(new) })
}

/// The thread: each chunk whole, in order; after a write error (the reader
/// gone: `EPIPE`) every later chunk is dropped, so the queue still empties.
fn writer(target: Target, rx: mpsc::Receiver<Vec<u8>>, shared: &Shared) {
    let mut broken = matches!(target, Target::Gone);
    for chunk in rx {
        if !broken && write_all(&target, &chunk, shared).is_err() {
            broken = true;
        }
        shared.update(|st| st.pending -= 1);
    }
}

fn write_all(target: &Target, mut buf: &[u8], shared: &Shared) -> io::Result<()> {
    while !buf.is_empty() {
        let (fd, n) = match target {
            Target::NonBlocking(fd) | Target::File(fd) => (
                fd.as_raw_fd(),
                // SAFETY: `buf` is valid for its length; write reads only it.
                unsafe { libc::write(fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) },
            ),
            Target::Socket(fd) => (
                fd.as_raw_fd(),
                // SAFETY: as for write; MSG_NOSIGNAL keeps a closed peer
                // from raising SIGPIPE.
                unsafe {
                    libc::send(
                        fd.as_raw_fd(),
                        buf.as_ptr().cast(),
                        buf.len(),
                        libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                    )
                },
            ),
            Target::Gone => return Err(io::Error::from(io::ErrorKind::BrokenPipe)),
        };
        if n >= 0 {
            buf = &buf[n as usize..];
            continue;
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => {
                shared.update(|st| st.stalled = true);
                wait_for_room(fd);
                shared.update(|st| st.stalled = false);
            }
            _ => return Err(e),
        }
    }
    Ok(())
}

/// Until `fd` takes bytes again, or its reader is gone (the write after
/// this one then fails). No bound: only the main thread's end bounds it.
fn wait_for_room(fd: RawFd) {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        // SAFETY: one pollfd, valid for the call.
        let rc = unsafe { libc::poll(&mut p, 1, -1) };
        if rc >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}
