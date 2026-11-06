//! A tool call that ends itself on the run's cancel, and still writes its
//! rows (chat-media-tools review, finding 4).
//!
//! The loop races its tool calls against the run's [`Cancel`] and drops
//! what is in flight the moment it lands: an MCP call's connection closes,
//! and nothing else happens. A call that bills a model on the way — an
//! image or audio tool's — would leave no `request_logs` row then, and no
//! tool row either: a picture a cloud provider made and charged for would
//! be nowhere in Usage.
//!
//! So a call may **take the stop** ([`take_call_stop`]), as a model turn
//! does (`relay`): it gets a [`StopSignal`] that is raised when the cancel
//! lands, ends itself at its next await, writes its rows — status 200,
//! `canceled` — and is awaited to that end rather than dropped. A call that
//! took nothing is dropped as before.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::Cancel;
use crate::proxy::StopSignal;

/// The stop of one tool call, and whether its executor took it.
struct CallStop {
    stop: StopSignal,
    took: AtomicBool,
}

tokio::task_local! {
    /// The tool call running now, inside [`run`].
    static CALL_STOP: Arc<CallStop>;
}

/// The stop of the tool call the executor runs now, when the loop runs it
/// (module doc). Taking it is a promise: the call ends itself once the stop
/// is raised, and the loop waits for it. `None` outside a loop's call (an
/// `/mcp` call, a realtime session's).
pub fn take_call_stop() -> Option<StopSignal> {
    CALL_STOP
        .try_with(|c| {
            c.took.store(true, Ordering::Relaxed);
            c.stop.clone()
        })
        .ok()
}

/// Run `call` under `cancel` (module doc): `None` when the cancel landed and
/// the call, having taken no stop, was dropped.
pub(super) async fn run<T>(cancel: &Cancel, call: impl Future<Output = T>) -> Option<T> {
    let (stop, signal) = crate::proxy::stop_pair();
    let this = Arc::new(CallStop {
        stop: signal,
        took: AtomicBool::new(false),
    });
    let call = CALL_STOP.scope(this.clone(), call);
    // A cancel that spares calls lets this one finish: its stop is never
    // raised (`stop` lives until it has).
    if cancel.spares_calls() {
        let out = call.await;
        drop(stop);
        return Some(out);
    }
    tokio::pin!(call);
    tokio::select! {
        biased;
        () = cancel.raised() => {
            if !this.took.load(Ordering::Relaxed) {
                return None;
            }
            stop.stop();
            Some(call.await)
        }
        out = &mut call => Some(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_call_that_took_the_stop_ends_itself_and_is_awaited() {
        let (handle, signal) = crate::proxy::stop_pair();
        let cancel = Cancel::signal(signal);
        let run = tokio::spawn(async move {
            run(&cancel, async {
                let stop = take_call_stop().expect("inside a loop's call");
                stop.raised().await;
                "ended itself"
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.stop();
        assert_eq!(run.await.unwrap(), Some("ended itself"));
    }

    #[tokio::test]
    async fn a_cancel_that_spares_calls_lets_one_in_flight_finish() {
        let (handle, signal) = crate::proxy::stop_pair();
        let cancel = Cancel::signal(signal).sparing_calls();
        let (go, gate) = tokio::sync::oneshot::channel::<()>();
        let run = tokio::spawn(async move {
            run(&cancel, async {
                let stop = take_call_stop().expect("inside a loop's call");
                let _ = gate.await;
                assert!(!stop.is_raised(), "its stop is never raised");
                "finished"
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.stop();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!run.is_finished(), "not dropped at the cancel");
        go.send(()).unwrap();
        assert_eq!(run.await.unwrap(), Some("finished"));
    }

    #[tokio::test]
    async fn a_call_that_took_nothing_is_dropped() {
        let (handle, signal) = crate::proxy::stop_pair();
        let cancel = Cancel::signal(signal);
        let run = tokio::spawn(async move { run(&cancel, std::future::pending::<&str>()).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.stop();
        assert_eq!(run.await.unwrap(), None);
    }

    #[test]
    fn no_stop_outside_a_call() {
        assert!(take_call_stop().is_none());
    }
}
