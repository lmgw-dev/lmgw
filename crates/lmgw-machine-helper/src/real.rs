//! This machine as the helper acts on it: programs started from an empty
//! environment with only `ROOT_ENV`, by absolute path, with no descriptor
//! beyond the standard three; files written as [`crate::files`] says; the
//! journal through its native socket.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};

use lmgw_machine::facts::ROOT_ENV;
use lmgw_machine::helper::{Stream, JOURNAL_ID};
use lmgw_machine::plan::steps::FileWrite;

use crate::files::{create_new, replace_own};
use crate::host::{Ended, Host};

/// journald's socket for the native protocol.
pub const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";

/// The child inherits no descriptor beyond stdin, stdout and stderr: every
/// descriptor from 3 up is made close-on-exec between the fork and the exec
/// (as `lmgw_core::child::stdio_only` does for lmgw's own children). pkexec
/// hands the helper whatever lmgw had open, and none of it is dnf's business.
/// Close-on-exec rather than closed, because std reports a failed exec back
/// through a close-on-exec pipe of its own. `close_range(2)` with
/// `CLOSE_RANGE_CLOEXEC` needs Linux 5.11; on an older kernel the call fails
/// and the spawn goes on as it would without it.
pub fn stdio_only(cmd: &mut Command) -> &mut Command {
    // SAFETY: the closure runs in the forked child before the exec, where
    // only async-signal-safe work is allowed. It makes one raw system call:
    // no allocation, no lock, no memory of the parent's touched.
    unsafe {
        cmd.pre_exec(|| {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            );
            Ok(())
        })
    }
}

/// `argv` as [`Host::run`] starts it, not yet spawned: the environment
/// cleared to [`ROOT_ENV`], no input, both outputs piped, `/` as its working
/// directory, no inherited descriptor.
pub fn command(argv: &[String]) -> io::Result<Command> {
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an empty command",
        ));
    };
    if !Path::new(program).is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{program} is not an absolute path, and the helper never searches PATH"),
        ));
    }
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(ROOT_ENV)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    stdio_only(&mut cmd);
    Ok(cmd)
}

/// The real machine.
pub struct RealHost;

impl Host for RealHost {
    /// Both pipes are read in this thread as their lines come, until the
    /// program has ended; then what it left in them is taken, and they are
    /// dropped (D101). A process it left behind (a scriptlet's daemon) may
    /// hold a pipe open for as long as it runs, and waiting for the pipes to
    /// close would wait for it: what it writes later is not the step's, and
    /// systemd stops it when the unit ends.
    fn run(&self, argv: &[String], line: &mut dyn FnMut(Stream, &str)) -> io::Result<Ended> {
        let mut child = command(argv)?.spawn()?;
        let mut pipes = [
            Pipe::new(child.stdout.take().map(OwnedFd::from), Stream::Stdout)?,
            Pipe::new(child.stderr.take().map(OwnedFd::from), Stream::Stderr)?,
        ];
        // The program's end, said by a thread that waits for it: it drops the
        // write end of this pipe once the program is reaped.
        let (ended_r, ended_w) = pipe()?;
        let waiter = std::thread::spawn(move || {
            let status = child.wait();
            drop(ended_w);
            status
        });
        loop {
            let mut fds = vec![libc::pollfd {
                fd: ended_r.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            let open: Vec<usize> = (0..pipes.len()).filter(|&i| pipes[i].open()).collect();
            for &i in &open {
                fds.push(libc::pollfd {
                    fd: pipes[i].raw(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            // SAFETY: `fds` is valid for its length during the call.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            for (k, &i) in open.iter().enumerate() {
                if fds[k + 1].revents != 0 {
                    pipes[i].take(None, line);
                }
            }
            if fds[0].revents != 0 {
                break;
            }
        }
        // The program has ended, so everything it wrote is in the pipes now:
        // that much, not what a process it left behind writes after it.
        for p in &mut pipes {
            let there = p.readable();
            p.take(Some(there), line);
            p.finish(line);
        }
        drop(pipes);
        let status = waiter
            .join()
            .map_err(|_| io::Error::other("the thread waiting for the program panicked"))??;
        Ok(Ended {
            code: status.code(),
            signal: status.signal(),
        })
    }

    fn write_file(&self, w: &FileWrite) -> io::Result<()> {
        let path = Path::new(w.path);
        if w.replace {
            replace_own(path, w.content.as_bytes(), w.mode)
        } else {
            create_new(path, w.content.as_bytes(), w.mode)
        }
    }

    fn journal(&self, priority: u8, message: &str, event: &str) -> io::Result<()> {
        let datagram = journal_datagram(&[
            ("PRIORITY", &priority.to_string()),
            ("SYSLOG_IDENTIFIER", JOURNAL_ID),
            ("MESSAGE", message),
            ("LMGW_EVENT", event),
        ]);
        UnixDatagram::unbound()?.send_to(&datagram, JOURNAL_SOCKET)?;
        Ok(())
    }
}

/// One of a program's outputs, read without blocking, cut into lines (its
/// newline and a carriage return before it taken off, decoded lossily, never
/// cut short).
struct Pipe {
    fd: Option<OwnedFd>,
    stream: Stream,
    /// The bytes after the last newline.
    rest: Vec<u8>,
}

impl Pipe {
    fn new(fd: Option<OwnedFd>, stream: Stream) -> io::Result<Self> {
        if let Some(fd) = &fd {
            set_nonblocking(fd.as_raw_fd())?;
        }
        Ok(Self {
            fd,
            stream,
            rest: Vec::new(),
        })
    }

    fn open(&self) -> bool {
        self.fd.is_some()
    }

    fn raw(&self) -> RawFd {
        self.fd.as_ref().map_or(-1, AsRawFd::as_raw_fd)
    }

    /// How many bytes are in the pipe now (`FIONREAD`).
    fn readable(&self) -> usize {
        let Some(fd) = &self.fd else { return 0 };
        let mut n: libc::c_int = 0;
        // SAFETY: FIONREAD writes one int into `n`.
        if unsafe { libc::ioctl(fd.as_raw_fd(), libc::FIONREAD, &mut n) } < 0 {
            return 0;
        }
        usize::try_from(n).unwrap_or(0)
    }

    /// Read what is there (at most `limit` bytes), handing on every whole
    /// line; the end of the pipe or a read error closes it.
    fn take(&mut self, limit: Option<usize>, line: &mut dyn FnMut(Stream, &str)) {
        let mut left = limit.unwrap_or(usize::MAX);
        let mut buf = [0u8; 64 * 1024];
        while left > 0 {
            let Some(fd) = &self.fd else { return };
            let want = buf.len().min(left);
            // SAFETY: `buf` is valid for `want` bytes; read writes only there.
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), want) };
            if n > 0 {
                let n = n as usize;
                left -= n;
                self.rest.extend_from_slice(&buf[..n]);
                self.lines(line);
                continue;
            }
            if n == 0 {
                self.finish(line);
                self.fd = None;
                return;
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock => return,
                _ => {
                    self.finish(line);
                    line(
                        self.stream,
                        &format!("(the helper could not read further output here: {e})"),
                    );
                    self.fd = None;
                    return;
                }
            }
        }
    }

    fn lines(&mut self, line: &mut dyn FnMut(Stream, &str)) {
        while let Some(at) = self.rest.iter().position(|&b| b == b'\n') {
            let mut whole: Vec<u8> = self.rest.drain(..=at).collect();
            whole.pop();
            if whole.last() == Some(&b'\r') {
                whole.pop();
            }
            line(self.stream, &String::from_utf8_lossy(&whole));
        }
    }

    /// The last line, when it has no newline.
    fn finish(&mut self, line: &mut dyn FnMut(Stream, &str)) {
        self.lines(line);
        if !self.rest.is_empty() {
            let last = std::mem::take(&mut self.rest);
            line(self.stream, &String::from_utf8_lossy(&last));
        }
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFL and F_SETFL on a descriptor this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A pipe of this process's own, close-on-exec: (read end, write end).
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe2 writes two descriptors into `fds`.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 just returned them, and nothing else owns them.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// One entry in journald's native protocol: `KEY=value` lines, and for a
/// value with a newline the binary form (the key, a newline, the length as
/// 64-bit little-endian, the bytes, a newline).
pub fn journal_datagram(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, value) in fields {
        out.extend_from_slice(key.as_bytes());
        if value.contains('\n') {
            out.push(b'\n');
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
            out.extend_from_slice(value.as_bytes());
        } else {
            out.push(b'=');
            out.extend_from_slice(value.as_bytes());
        }
        out.push(b'\n');
    }
    out
}
