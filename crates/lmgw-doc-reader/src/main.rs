//! lmgw's document reader (document-conversion design §5.2, §14 WP4 review
//! round): one request on the command line ([`Op`]), the file on stdin, one
//! frame on stdout ([`wire`]).
//!
//! It is started only by the gateway (`extract/convert/reader.rs`, through
//! the runner every document tool goes through), in a process group of its
//! own, under a timeout and an output bound, and with its address-space
//! limit as its first argument: before it reads a byte of the file it sets
//! that limit on itself and closes every descriptor beyond stdio
//! ([`confine`]), and it does not go on if it cannot. (Setting them here,
//! not between the gateway's fork and exec, lets the gateway start it
//! without a fork, whose cost grows with the gateway's own memory.) It
//! opens no file and no socket: its input is stdin, its output stdout. A file that makes a parser loop, recurse or ask
//! for memory without end costs this process, which the gateway stops and
//! names; the gateway itself only ever reads the bounded frame.
//!
//! What it writes to stderr is for a person running it by hand. The gateway
//! shows and logs none of it, and a panic says where, never what: a parser's
//! panic message may quote the document.

use std::io::{Cursor, Read, Write};

use lmgw_doc_reader::wire::{self, Answer, Failure, Op, Page, READER_STACK_BYTES, WIRE_VERSION};

mod crop;
mod email;
mod epub;
mod html;
mod image;
mod run;
mod xls;

/// The exit status of a command line that is no request.
const EXIT_USAGE: i32 = 2;
/// The exit status when stdin could not be read or stdout not written.
const EXIT_IO: i32 = 3;
/// The exit status when the reader could not confine itself as asked.
const EXIT_UNCONFINED: i32 = 4;

/// This process limited to `memory` bytes of address space, with no open
/// descriptor beyond stdin, stdout and stderr; `Err` says which step
/// failed.
#[cfg(unix)]
fn confine(memory: u64) -> Result<(), &'static str> {
    let limit = libc::rlimit {
        rlim_cur: memory as libc::rlim_t,
        rlim_max: memory as libc::rlim_t,
    };
    // SAFETY: plain system calls on this process, before any thread or
    // parser exists; `limit` is a valid rlimit for the call's duration.
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } != 0 {
        return Err("its memory limit could not be set");
    }
    // What the gateway's process had open without close-on-exec (a
    // library's descriptors) is closed here. `close_range` needs Linux
    // 5.9; elsewhere the descriptors are walked.
    #[cfg(target_os = "linux")]
    {
        // SAFETY: as above; no descriptor from 3 up is in use yet.
        let closed = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                0,
            )
        };
        if closed != 0 {
            return Err("its inherited descriptors could not be closed");
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: as above.
        let open_max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) }.clamp(3, 65_536) as i32;
        for fd in 3..open_max {
            // SAFETY: closing a descriptor this process does not use.
            unsafe { libc::close(fd) };
        }
    }
    Ok(())
}

/// No memory limit can be set on this system yet, so the gateway does not
/// start the reader here (`run.rs`), and a limit asked for is refused.
#[cfg(not(unix))]
fn confine(_memory: u64) -> Result<(), &'static str> {
    Err("a memory limit is not supported on this system yet")
}

/// stdin as a file read by position, when it is one: the gateway hands a
/// page run its spool file, of which one page's bytes are needed, and
/// several runs may share the file.
#[cfg(unix)]
mod positioned {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    use std::os::fd::AsFd;
    use std::os::unix::fs::FileExt;

    pub struct Positioned {
        file: File,
        pos: u64,
        len: u64,
    }

    /// stdin, when it is a regular file.
    pub fn stdin() -> Option<Positioned> {
        let fd = std::io::stdin().as_fd().try_clone_to_owned().ok()?;
        let file = File::from(fd);
        let meta = file.metadata().ok()?;
        meta.is_file().then(|| Positioned {
            file,
            pos: 0,
            len: meta.len(),
        })
    }

    impl Read for Positioned {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.file.read_at(buf, self.pos)?;
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl Seek for Positioned {
        fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
            let next = match to {
                SeekFrom::Start(n) => Some(n),
                SeekFrom::End(d) => self.len.checked_add_signed(d),
                SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            };
            self.pos = next.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "a seek before the start")
            })?;
            Ok(self.pos)
        }
    }
}

fn stdin_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Err(e) = std::io::stdin().lock().read_to_end(&mut bytes) {
        eprintln!("lmgw-doc-reader: stdin could not be read: {e}");
        std::process::exit(EXIT_IO);
    }
    bytes
}

/// An image request on stdin: by position when stdin is a file, else whole.
fn image_op(op: Op) -> Result<(Answer, Vec<Vec<u8>>), Failure> {
    #[cfg(unix)]
    if let Some(file) = positioned::stdin() {
        return image_on(op, std::io::BufReader::new(file));
    }
    image_on(op, Cursor::new(stdin_bytes()))
}

fn image_on<R: std::io::BufRead + std::io::Seek>(
    op: Op,
    r: R,
) -> Result<(Answer, Vec<Vec<u8>>), Failure> {
    match op {
        Op::ImageInfo => image::info(r).map(|pages| (Answer::Images(pages), Vec::new())),
        Op::ImagePage { max_pixels, ifd } => {
            image::page(r, max_pixels, ifd).map(|(px, pgm)| (Answer::Pixels(px), vec![pgm]))
        }
        _ => unreachable!("image_op is given image requests"),
    }
}

/// The request's answer and its blobs.
fn answer(op: Op) -> (Answer, Vec<Vec<u8>>) {
    let done = match op {
        Op::Html => {
            let page = lmgw_doc_reader::charset::decode(&stdin_bytes());
            html::markdown(&page)
                .map(|md| {
                    let page = Page {
                        markdown: md.text,
                        too_deep: md.too_deep.map(|d| d as u64),
                    };
                    (Answer::Page(page), Vec::new())
                })
                .map_err(Failure::Unreadable)
        }
        Op::Epub { budget } => {
            epub::read(&stdin_bytes(), budget).map(|b| (Answer::Book(b), Vec::new()))
        }
        Op::Email => email::read(&stdin_bytes()).map(|(m, blobs)| (Answer::Mail(m), blobs)),
        Op::Xls => xls::read(&stdin_bytes()).map(|s| (Answer::Sheets(s), Vec::new())),
        Op::ImageInfo | Op::ImagePage { .. } => image_op(op),
        Op::Crop {
            width,
            height,
            budget,
            boxes,
        } => crop::crops(&stdin_bytes(), width, height, budget, &boxes)
            .map(|(c, blobs)| (Answer::Crops(c), blobs)),
    };
    done.unwrap_or_else(|f| (Answer::Failed(f), Vec::new()))
}

fn main() {
    // Where, never what (module doc).
    std::panic::set_hook(Box::new(|info| {
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "an unknown place".to_string());
        eprintln!("lmgw-doc-reader stopped at {at}");
    }));
    // `run`: another program started confined (`run.rs`), before anything
    // of the reader's own; its paths need not be UTF-8.
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if raw
        .first()
        .is_some_and(|a| a == lmgw_doc_reader::sandbox::RUN)
    {
        run::main(&raw[1..]);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        println!(
            "lmgw-doc-reader {} (wire {WIRE_VERSION})",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    let Some((memory, op)) = Op::parse_limited(&args) else {
        eprintln!(
            "lmgw-doc-reader is started by lmgw: [--memory-limit <bytes>] html | epub <budget> \
             | email | xls | image-info | image-page <max pixels> [<directory offset>] | crop \
             <width> <height> <budget> [<x,y,w,h> …], the file on stdin"
        );
        std::process::exit(EXIT_USAGE);
    };
    if let Some(memory) = memory {
        if let Err(why) = confine(memory) {
            eprintln!("lmgw-doc-reader: {why}, so it reads nothing");
            std::process::exit(EXIT_UNCONFINED);
        }
        // It opens no file and no socket from here on: a Landlock ruleset
        // that allows nothing, where the kernel has Landlock (performance
        // round, 2026-11-19); elsewhere it reads as before.
        if let Err(why) = run::nothing() {
            eprintln!("lmgw-doc-reader: {why}, so it reads nothing");
            std::process::exit(EXIT_UNCONFINED);
        }
    }
    // htmd writes a page's tree recursively: on a stack of a known size,
    // whatever the system's default is.
    let worker = std::thread::Builder::new()
        .stack_size(READER_STACK_BYTES)
        .spawn(move || answer(op));
    let (answer, blobs) = match worker.map(|w| w.join()) {
        Ok(Ok(done)) => done,
        // The hook has said where.
        Ok(Err(_)) => std::process::exit(101),
        Err(e) => {
            eprintln!("lmgw-doc-reader: no thread to read on: {e}");
            std::process::exit(EXIT_IO);
        }
    };
    let blobs: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::io::stdout().lock());
    if wire::write(&mut out, &answer, &blobs)
        .and_then(|()| out.flush())
        .is_err()
    {
        // The gateway stopped reading (its output bound, or it went away).
        std::process::exit(EXIT_IO);
    }
}
