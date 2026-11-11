//! A native child's log: the last [`LOG_RING_LINES`] lines of its stdout and
//! stderr, in the order they arrived, in memory.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt};

/// How many lines of one engine's output the native launcher keeps. An
/// engine prints a few hundred lines while it loads and one or two per
/// request, so this holds a start and the requests after it; once it is
/// full, each new line drops the oldest, and lmgw's log says so the first
/// time it happens for a child.
pub const LOG_RING_LINES: usize = 4000;

/// How long one kept line may be. A longer run of output without a line
/// break (a progress bar, a dumped prompt) is kept as several lines of this
/// size, so nothing is cut, only split.
pub const LOG_LINE_BYTES: usize = 8 * 1024;

/// One child's log ring.
#[derive(Debug)]
pub(super) struct Ring {
    name: String,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    lines: VecDeque<String>,
    dropped: u64,
}

impl Ring {
    pub(super) fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            inner: Mutex::new(Inner::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn push(&self, line: String) {
        let mut inner = self.lock();
        if inner.lines.len() >= LOG_RING_LINES {
            inner.lines.pop_front();
            inner.dropped += 1;
            if inner.dropped == 1 {
                tracing::info!(
                    container = %self.name,
                    "the in-memory log of the engine process {} is full: it keeps the last \
                     {LOG_RING_LINES} lines of its stdout and stderr, and drops the oldest from \
                     here on",
                    self.name
                );
            }
        }
        inner.lines.push_back(line);
    }

    /// The last `n` lines, each ending in a line break.
    pub(super) fn tail(&self, n: usize) -> String {
        let inner = self.lock();
        let skip = inner.lines.len().saturating_sub(n);
        let mut out = String::new();
        for line in inner.lines.iter().skip(skip) {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// Read `from` to its end into this ring, a line at a time.
    pub(super) async fn pump(self: Arc<Self>, mut from: impl AsyncRead + Unpin) {
        let mut pending: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; LOG_LINE_BYTES];
        loop {
            let n = match from.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            pending.extend_from_slice(&chunk[..n]);
            while let Some(at) = pending.iter().position(|b| *b == b'\n') {
                let rest = pending.split_off(at + 1);
                self.push_bytes(&pending[..at]);
                pending = rest;
            }
            // A run without a line break is not held back past the bound.
            while pending.len() >= LOG_LINE_BYTES {
                let rest = pending.split_off(LOG_LINE_BYTES);
                self.push_bytes(&pending);
                pending = rest;
            }
        }
        if !pending.is_empty() {
            self.push_bytes(&pending);
        }
    }

    /// One line of output, as lines of at most [`LOG_LINE_BYTES`] (a
    /// character cut at a split shows as U+FFFD).
    fn push_bytes(&self, bytes: &[u8]) {
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        if bytes.is_empty() {
            self.push(String::new());
        }
        for piece in bytes.chunks(LOG_LINE_BYTES) {
            self.push(String::from_utf8_lossy(piece).into_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lines_are_split_kept_in_order_and_bounded() {
        let ring = Ring::new("t");
        let long = "x".repeat(LOG_LINE_BYTES + 10);
        let input = format!("one\r\ntwo\n{long}\nlast without a break");
        ring.clone().pump(input.as_bytes()).await;
        let tail = ring.tail(10);
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines[0], "one");
        assert_eq!(lines[1], "two");
        assert_eq!(lines[2].len(), LOG_LINE_BYTES);
        assert_eq!(lines[3], "x".repeat(10));
        assert_eq!(lines[4], "last without a break");
        assert_eq!(ring.tail(1), "last without a break\n");

        for i in 0..LOG_RING_LINES {
            ring.push(format!("l{i}"));
        }
        let all = ring.tail(usize::MAX);
        assert_eq!(all.lines().count(), LOG_RING_LINES);
        assert!(all.starts_with("l0\n"), "the oldest lines went first");
    }
}
