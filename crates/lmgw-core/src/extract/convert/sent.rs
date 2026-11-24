//! The model calls sent inside a part of a conversion that may be cut off
//! before it answers (security round B, a carried bug): an e-mail's
//! attachment that its message's deadline ends while it is read
//! (`file/more/late.rs`). Such an attachment returns nothing, so the calls
//! it had sent (paid for, whether answered or not) would be missing from
//! the message's `stats.models`. Every call site records its call here when
//! it sends it ([`record`]: the page and picture models' `call::ask`, the
//! speech-to-text call), into each [`Ledger`] the call is inside, so an
//! attachment nested in an attached message is counted by both; the cut
//! part's ledger is what the message adds ([`Ledger::take`]).

use std::sync::{Arc, Mutex};

use lmgw_api_types::documents::model_help::ModelCalls;

tokio::task_local! {
    static LEDGER: Arc<Ledger>;
}

/// The calls sent inside one scope ([`scope`]).
#[derive(Debug, Default)]
pub struct Ledger {
    /// The scope this one is inside, which counts its calls too.
    parent: Option<Arc<Ledger>>,
    calls: Mutex<Vec<ModelCalls>>,
}

impl Ledger {
    /// A ledger inside the current one, if any.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            parent: LEDGER.try_with(Arc::clone).ok(),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn add(&self, call: &ModelCalls) {
        let mut g = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        match g
            .iter_mut()
            .find(|e| (&e.alias, &e.answered_for) == (&call.alias, &call.answered_for))
        {
            Some(e) => e.calls = e.calls.saturating_add(call.calls),
            None => g.push(call.clone()),
        }
        drop(g);
        if let Some(p) = &self.parent {
            p.add(call);
        }
    }

    /// The calls sent inside, one entry per model, and how many in all.
    pub fn take(&self) -> (u32, Vec<ModelCalls>) {
        let calls = std::mem::take(&mut *self.calls.lock().unwrap_or_else(|e| e.into_inner()));
        let n = calls
            .iter()
            .map(|c| c.calls)
            .fold(0u32, u32::saturating_add);
        (n, calls)
    }
}

/// `f` with `ledger` counting the calls sent inside it.
pub async fn scope<F: std::future::Future>(ledger: Arc<Ledger>, f: F) -> F::Output {
    LEDGER.scope(ledger, f).await
}

/// One call is sent now: `call` (its `calls` is the count, 1; nothing is
/// read by it yet) goes to every ledger it is inside. Outside any, nothing.
pub fn record(call: ModelCalls) {
    let _ = LEDGER.try_with(|l| l.add(&call));
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::documents::model_help::BoundFrom;

    fn call(alias: &str) -> ModelCalls {
        ModelCalls {
            alias: alias.into(),
            answered_for: None,
            calls: 1,
            parts_read: 0,
            pictures_described: 0,
            files_transcribed: 0,
            in_flight_bound: 1,
            bound_from: BoundFrom::OneCall,
        }
    }

    #[tokio::test]
    async fn a_call_counts_in_every_scope_it_is_inside() {
        record(call("example-outside"));
        let outer = Ledger::new();
        let inner_seen = scope(outer.clone(), async {
            record(call("example-page"));
            let inner = Ledger::new();
            scope(inner.clone(), async {
                record(call("example-page"));
                record(call("example-stt"));
            })
            .await;
            inner.take()
        })
        .await;
        assert_eq!(inner_seen.0, 2);
        let (n, by) = outer.take();
        assert_eq!(n, 3);
        let page = by.iter().find(|c| c.alias == "example-page").unwrap();
        assert_eq!(page.calls, 2);
        assert!(by.iter().all(|c| c.alias != "example-outside"));
        assert_eq!(outer.take().0, 0);
    }
}
