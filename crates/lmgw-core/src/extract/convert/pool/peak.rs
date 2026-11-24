//! The heap one thread holds, counted for tests that hold an in-process
//! read to its charge (`charge::READ_PER_OUTPUT_BYTE`, [`super::tests_read`]).
//! The test binary's allocator is the system's; it counts only on a thread
//! inside [`peak_of`], each block rounded up as glibc's malloc rounds it
//! (16-byte steps with its 8-byte header, 32 bytes at the least), so many
//! small blocks count what they really cost.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

#[global_allocator]
static COUNTING: Counting = Counting;

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    /// Bytes held now above the start of [`peak_of`] (negative when it
    /// frees blocks from before).
    static NOW: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

/// A block of `n` bytes as malloc holds it.
fn chunk(n: usize) -> i64 {
    let n = i64::try_from(n).unwrap_or(i64::MAX);
    (n.saturating_add(8 + 15) & !15).max(32)
}

fn count(by: i64) {
    if !ON.try_with(Cell::get).unwrap_or(false) {
        return;
    }
    let now = NOW.with(|c| {
        let v = c.get().saturating_add(by);
        c.set(v);
        v
    });
    PEAK.with(|p| p.set(p.get().max(now)));
}

// SAFETY: every call goes to the system allocator unchanged; the counting
// only touches const-initialised thread-locals without destructors, which
// never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            count(chunk(l.size()));
        }
        p
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            count(chunk(l.size()));
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(p, l) };
        count(-chunk(l.size()));
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() {
            count(chunk(n) - chunk(l.size()));
        }
        q
    }
}

/// `f`'s result and the most heap this thread held above its start while
/// `f` ran, in bytes as malloc holds them.
pub fn peak_of<T>(f: impl FnOnce() -> T) -> (T, u64) {
    NOW.with(|c| c.set(0));
    PEAK.with(|c| c.set(0));
    ON.with(|c| c.set(true));
    let out = f();
    ON.with(|c| c.set(false));
    let peak = PEAK.with(Cell::get);
    (out, u64::try_from(peak).unwrap_or(0))
}
