//! Where a violation goes: printed when found, and the process fails at its
//! exit.
//!
//! The check runs inside the gateway (an egress building a body, a route's
//! answer on its way out), often on a task of its own, where a panic would
//! only cost that request and a test that expected an error would still
//! pass. So a violation is printed at once, with the test thread's name,
//! and kept; when the test process exits, any kept violation turns its exit
//! status into a failure. nextest runs every test in a process of its own,
//! so that failure is the test's; under `cargo test` it is the binary's, and
//! the printed lines name the tests.

use std::cell::Cell;
use std::sync::Mutex;

static FOUND: Mutex<Vec<String>> = Mutex::new(Vec::new());

thread_local! {
    /// Set while a test checks bodies itself ([`quietly`]): it asserts on
    /// what it gets back, so nothing is kept for the exit.
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

/// `f` with the bodies it builds checked but not kept: for a test that
/// asserts on [`super::egress::check`] itself.
pub fn quietly<R>(f: impl FnOnce() -> R) -> R {
    /// Puts the flag back when `f` returns or panics.
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            QUIET.with(|q| q.set(self.0));
        }
    }
    let _reset = Reset(QUIET.with(|q| q.replace(true)));
    f()
}

/// Whether the calling thread is inside [`quietly`].
pub fn is_quiet() -> bool {
    QUIET.with(Cell::get)
}

/// One violation: printed now, failing the process at its exit.
pub fn record(report: String) {
    if QUIET.with(Cell::get) {
        return;
    }
    let thread = std::thread::current();
    let who = thread.name().unwrap_or("an unnamed thread");
    let line = format!("api-compat violation (in {who}): {report}");
    eprintln!("{line}");
    FOUND.lock().unwrap_or_else(|e| e.into_inner()).push(line);
}

/// A known violation (`known::KNOWN`) that matched: printed, never kept.
pub fn note(report: String) {
    if !QUIET.with(Cell::get) {
        eprintln!("api-compat known violation, not fixed yet: {report}");
    }
}

#[ctor::dtor]
unsafe fn fail_the_process_on_a_violation() {
    let found = FOUND.lock().unwrap_or_else(|e| e.into_inner());
    if found.is_empty() {
        return;
    }
    // No Rust stdio at exit (ctor's warning): the summary goes to fd 2 by hand.
    let text = format!(
        "\napi-compat: {} violation(s) of the providers' schemas; the test process fails:\n{}\n",
        found.len(),
        found.join("\n")
    );
    // The CRT's write takes a u32 count on Windows.
    #[cfg(unix)]
    let len = text.len();
    #[cfg(windows)]
    let len = u32::try_from(text.len()).unwrap_or(u32::MAX);
    // SAFETY: plain libc calls on a buffer that outlives them, at exit.
    unsafe {
        libc::write(2, text.as_ptr().cast(), len);
        libc::_exit(1);
    }
}
