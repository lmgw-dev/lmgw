//! Podman's API as a script, for the download before a model's first start
//! (machine-check design §5): a [`PullApi`] that streams `before` at once,
//! holds the rest of its answer until the test lets go, and then calls
//! `stored` — the fake store now has the image. No service, no registry.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lmgw_core::backends::pull::{ApiPull, PullApi};
use lmgw_core::erased::Erased;
use tokio::sync::{mpsc, watch};

/// What the image's arrival does to the test's world.
pub type Stored = Arc<dyn Fn() + Send + Sync>;

pub struct ScriptedPull {
    before: Vec<String>,
    after: Vec<String>,
    release: watch::Receiver<bool>,
    refuse: Option<String>,
    stored: Stored,
    /// The references asked for, in order.
    pub calls: Mutex<Vec<String>>,
    /// The reader went away before the answer ended.
    pub hung_up: Arc<AtomicBool>,
}

impl ScriptedPull {
    /// The script, and the sender that lets its held part go. `refuse`:
    /// the API refuses every pull with this message instead.
    pub fn new(
        before: Vec<String>,
        after: Vec<String>,
        refuse: Option<&str>,
        stored: Stored,
    ) -> (Arc<Self>, watch::Sender<bool>) {
        let (tx, rx) = watch::channel(false);
        let api = Arc::new(Self {
            before,
            after,
            release: rx,
            refuse: refuse.map(str::to_string),
            stored,
            calls: Mutex::new(Vec::new()),
            hung_up: Arc::default(),
        });
        (api, tx)
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn hung_up(&self) -> bool {
        self.hung_up.load(Ordering::SeqCst)
    }
}

impl PullApi for ScriptedPull {
    fn pull<'a>(&'a self, reference: &'a str) -> Erased<'a, ApiPull> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(reference.to_string());
            if let Some(r) = &self.refuse {
                return ApiPull::Refused(r.clone());
            }
            let (tx, rx) = mpsc::channel(64);
            let (before, after) = (self.before.clone(), self.after.clone());
            let mut release = self.release.clone();
            let (stored, hung_up) = (self.stored.clone(), self.hung_up.clone());
            tokio::spawn(async move {
                for l in before {
                    if tx.send(Ok(l)).await.is_err() {
                        hung_up.store(true, Ordering::SeqCst);
                        return;
                    }
                }
                tokio::select! {
                    _ = release.wait_for(|r| *r) => {}
                    _ = tx.closed() => {
                        hung_up.store(true, Ordering::SeqCst);
                        return;
                    }
                }
                for l in after {
                    if tx.send(Ok(l)).await.is_err() {
                        hung_up.store(true, Ordering::SeqCst);
                        return;
                    }
                }
                stored();
            });
            ApiPull::Streaming(rx)
        })
    }
}
