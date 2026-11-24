//! A step on a blocking thread told to stop when the future that waits for
//! it is dropped (security round A, M1): its file's clock ran out
//! (`document_timeout`), the client went away, or its job was cancelled. A
//! blocking thread cannot be cancelled from outside, so the in-gateway
//! readers that can run long on a hostile file (the office readers' XML
//! walk and their entry reads, `extract::office`) ask [`stopped`] as they
//! go and end with their own error; the step's slot is then given back at
//! once instead of after the whole read. The review's `sizelie.docx` (1.1
//! MiB, 400 MiB of XML) held its slot 84 s after its request had gone.
//!
//! Outside a pool step (Chat's own reads) [`stopped`] is always false.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

thread_local! {
    static STOP: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Whether the step running on this thread was told to stop.
pub fn stopped() -> bool {
    STOP.with(|s| {
        s.borrow()
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
    })
}

/// Held by the future that waits for the step: dropped before the step has
/// ended, it tells the step to stop.
pub(super) struct Armed(Arc<AtomicBool>);

impl Drop for Armed {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A new step's flag: the waiting side's [`Armed`] and the thread's half.
pub(super) fn arm() -> (Armed, Arc<AtomicBool>) {
    let flag = Arc::new(AtomicBool::new(false));
    (Armed(flag.clone()), flag)
}

/// Clears the thread's flag when the step ends, a panic included (the
/// blocking thread runs other steps after).
struct Clear;

impl Drop for Clear {
    fn drop(&mut self) {
        STOP.with(|s| *s.borrow_mut() = None);
    }
}

/// Run `f` on this (blocking) thread with `flag` as its stop.
pub(super) fn run<T>(flag: Arc<AtomicBool>, f: impl FnOnce() -> T) -> T {
    STOP.with(|s| *s.borrow_mut() = Some(flag));
    let _clear = Clear;
    f()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::super::{blocking_in, test_pool};

    /// A step that loops until told to stop gives its slot back as soon as
    /// the future waiting for it is dropped.
    #[tokio::test]
    async fn a_dropped_step_stops_and_gives_its_slot_back() {
        let pool = test_pool::small(1, 1);
        let step = tokio::spawn(blocking_in(pool, || {
            let started = Instant::now();
            while !super::stopped() {
                assert!(started.elapsed() < Duration::from_secs(60), "never told");
                std::thread::sleep(Duration::from_millis(5));
            }
        }));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pool.facts().running, 1);
        step.abort();
        let freed = Instant::now();
        let _again = tokio::time::timeout(Duration::from_secs(10), pool.acquire())
            .await
            .expect("the slot came back");
        assert!(freed.elapsed() < Duration::from_secs(5));
        // Outside a step: never stopped.
        assert!(!super::stopped());
    }

    /// The office readers ask as they go: a read told to stop ends with its
    /// own error, at its next entry or its next 4,096 XML events.
    #[test]
    fn an_office_read_told_to_stop_ends_with_its_own_error() {
        use crate::extract::office::{extract, OfficeError, OfficeFormat};
        let paras: Vec<String> = (0..20_000).map(|i| format!("paragraph {i}")).collect();
        let refs: Vec<&str> = paras.iter().map(String::as_str).collect();
        let doc = crate::extract::test_files::docx(&refs);
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let r = super::run(flag, || extract(OfficeFormat::Docx, &doc));
        assert_eq!(r.err(), Some(OfficeError::Stopped));
        // Not told: read whole.
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let r = super::run(flag, || extract(OfficeFormat::Docx, &doc)).unwrap();
        assert!(r.markdown().contains("paragraph 19999"));
    }
}
