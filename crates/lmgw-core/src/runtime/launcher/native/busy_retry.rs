//! A spawn that waits out Linux's ETXTBSY.
//!
//! lmgw writes an engine executable (the asset unpack) and spawns it soon
//! after, in a process with many threads. While the write fd is open, any
//! other thread's fork copies it into its child until that child's exec, and
//! the kernel refuses to exec a file some process holds open for writing:
//! `ETXTBSY`, "Text file busy" (golang/go#22315). The window is the other
//! thread's fork-to-exec, microseconds to a few milliseconds, so the spawn
//! is tried again after a short, growing pause.

use std::future::Future;
use std::io::{Error, ErrorKind};
use std::time::Duration;

/// First pause; each retry doubles it.
const FIRST_PAUSE: Duration = Duration::from_millis(10);
/// Retries stop once the pauses have added up to this.
const TOTAL: Duration = Duration::from_secs(1);

fn is_busy(e: &Error) -> bool {
    cfg!(unix) && (e.kind() == ErrorKind::ExecutableFileBusy || e.raw_os_error() == Some(26))
}

/// The pauses between tries: 10, 20, 40 ... ms, the last one cut so they sum
/// to `TOTAL`.
fn pauses() -> Vec<Duration> {
    let mut out = Vec::new();
    let (mut next, mut spent) = (FIRST_PAUSE, Duration::ZERO);
    while spent < TOTAL {
        let p = next.min(TOTAL - spent);
        out.push(p);
        spent += p;
        next *= 2;
    }
    out
}

/// Run `attempt(state)` until it stops answering ETXTBSY or the pauses are
/// spent. The attempt hands its state (the command to spawn) back with its
/// answer, so the next try can use it again.
pub(super) async fn spawn_retrying<S, T, F, Fut>(state: S, attempt: F) -> std::io::Result<T>
where
    F: FnMut(S) -> Fut,
    Fut: Future<Output = (S, std::io::Result<T>)>,
{
    retry_with(state, attempt, pauses()).await
}

async fn retry_with<S, T, F, Fut>(
    mut state: S,
    mut attempt: F,
    pauses: Vec<Duration>,
) -> std::io::Result<T>
where
    F: FnMut(S) -> Fut,
    Fut: Future<Output = (S, std::io::Result<T>)>,
{
    let mut pauses = pauses.into_iter();
    let mut waited = Duration::ZERO;
    loop {
        let (back, answer) = attempt(state).await;
        state = back;
        let e = match answer {
            Err(e) if is_busy(&e) => e,
            other => return other,
        };
        let Some(pause) = pauses.next() else {
            return Err(Error::new(
                e.kind(),
                format!("{e} (it stayed busy for {waited:?} of retries)"),
            ));
        };
        tracing::debug!("the executable is busy (ETXTBSY), trying the start again in {pause:?}");
        tokio::time::sleep(pause).await;
        waited += pause;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn busy() -> Error {
        Error::from(ErrorKind::ExecutableFileBusy)
    }

    #[test]
    fn the_pauses_double_and_sum_to_one_second() {
        let p = pauses();
        assert_eq!(&p[..4], [10, 20, 40, 80].map(Duration::from_millis));
        assert_eq!(p.iter().sum::<Duration>(), TOTAL);
    }

    #[tokio::test]
    async fn two_busy_answers_then_a_start_succeeds() {
        let calls = Cell::new(0);
        let got = retry_with(
            (),
            |s| {
                calls.set(calls.get() + 1);
                let n = calls.get();
                async move { (s, if n <= 2 { Err(busy()) } else { Ok(n) }) }
            },
            vec![Duration::ZERO; 5],
        )
        .await;
        assert_eq!(got.unwrap(), 3);
    }

    #[tokio::test]
    async fn an_always_busy_file_fails_with_the_note() {
        let e = retry_with::<_, (), _, _>(
            (),
            |s| async move { (s, Err(busy())) },
            vec![Duration::from_millis(1); 3],
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::ExecutableFileBusy);
        assert!(e.to_string().contains("stayed busy for 3ms"), "{e}");
    }

    #[tokio::test]
    async fn another_error_is_not_retried() {
        let calls = Cell::new(0);
        let e = retry_with::<_, (), _, _>(
            (),
            |s| {
                calls.set(calls.get() + 1);
                async move { (s, Err(Error::from(ErrorKind::NotFound))) }
            },
            vec![Duration::ZERO; 5],
        )
        .await
        .unwrap_err();
        assert_eq!((e.kind(), calls.get()), (ErrorKind::NotFound, 1));
    }
}
