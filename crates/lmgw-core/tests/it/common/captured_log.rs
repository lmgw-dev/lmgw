//! What lmgw logs while a test runs, read back as text: a `tracing`
//! subscriber of the test's own, the default on this thread for as long as
//! its guard lives. `#[tokio::test]` runs on one thread, so the tasks it
//! spawns log here too.

use std::sync::{Arc, Mutex};

/// The text the subscriber wrote, at INFO and above.
#[derive(Clone, Default)]
pub struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl CapturedLog {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = CapturedLog;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A second dispatcher for the whole process, never used to log.
///
/// tracing-core caches each callsite's interest. While a single dispatcher
/// is registered (`has_just_one`), a callsite first hit on another thread is
/// judged by *that* thread's default — none, in a parallel test — and cached
/// as never, so a capture on this thread misses every line of it. With a
/// second dispatcher registered, every callsite asks the thread it fires on.
static SECOND: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();

/// Start capturing; the capture ends when the guard drops.
pub fn capture_log() -> (CapturedLog, tracing::subscriber::DefaultGuard) {
    SECOND.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
    let log = CapturedLog::default();
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish(),
    );
    // Callsites judged before this capture existed are judged again.
    tracing::callsite::rebuild_interest_cache();
    (log, guard)
}
