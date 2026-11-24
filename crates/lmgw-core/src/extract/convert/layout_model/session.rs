//! The one session of the layout model, and the gate in front of it.
//!
//! **One session, shared.** ONNX Runtime keeps the model's weights per
//! session, so the gateway loads it once, at the first page that needs it:
//! the gateway's resident memory grew by 485 MiB over the corpus' 115
//! pages ([`LAYOUT_MEMORY_BYTES`]; the pinned file, 4 threads, measured
//! 2026-11-19; a bare program with only the session held 364 MiB after
//! the load and 427 MiB after the pages). It is dropped when no page has
//! used it for [`LAYOUT_IDLE_UNLOAD`], and loaded again by the next page
//! (about half a second). Every load reads the file once, checks those
//! bytes against the pinned SHA-256 and loads the session from them
//! ([`files::read_checked`]); the bytes (163 MiB) are held only while the
//! load runs.
//!
//! **One inference at a time, and its threads are slots.** `Session::run`
//! needs the session alone, so pages queue at the gate, first come first
//! served, each file's pages taking their turn among the other files'. The
//! page at the gate asks the tool pool for [`LAYOUT_THREADS`] slots at once
//! ([`ToolPool::acquire_many`]: fewer when the pool is smaller, all of them
//! taken together at its turn, none held while it waits), then runs on a
//! blocking thread with as many threads as it got; the thread holds the
//! gate and the slots until the run has ended. So the cores the model uses
//! are cores the pool handed out, never more, and where the pool is 4 or
//! smaller (a 16 GiB machine's is 2) one inference has the whole pool
//! while it runs, about 0.4 s a page. Waiting at the gate and for the
//! slots holds no slot and does not count on the file's clock
//! (`pool::clock`).
//!
//! **Threads.** [`LAYOUT_THREADS`], or the pool's size when that is
//! smaller; a session loaded for another count (the pool was set by hand
//! since) is loaded anew. Measured on the pinned file on a 32-core machine
//! under other load: 1.25 s a page with one thread, 0.37 s with four, no
//! faster with eight or sixteen. The threads do not spin between runs, so
//! they use nothing once their slots are given back.
//!
//! The gate is generic over its [`Runner`] (ONNX Runtime, or a test's), so
//! its slots, its unload and its file check are tested without the model.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::super::pool::{self, clock, ToolPool};
use super::decode::{decode, Region};
use super::files::{self, ModelFile, HERON};
use super::prep::SIDE;

/// The most threads one inference uses (module doc: no faster with more).
pub const LAYOUT_THREADS: usize = 4;

/// How long the session stays loaded after its last page.
pub const LAYOUT_IDLE_UNLOAD: Duration = Duration::from_secs(300);

/// About what the loaded session holds in memory (module doc): 485 MiB.
pub const LAYOUT_MEMORY_BYTES: u64 = 485 << 20;

/// Why a page got no regions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferError {
    /// The file or the session: no page can be read until it is fixed.
    Unusable(String),
    /// This page's run.
    Page(String),
}

/// Loading a session and running it: ONNX Runtime ([`Ort`]), or a test's.
pub(super) trait Runner: Send + Sync + 'static {
    type Session: Send + 'static;
    /// A session from the model's `bytes`, with `threads` threads.
    fn load(&self, bytes: &[u8], threads: usize) -> Result<Self::Session, String>;
    /// One run: `input` is [`super::prep::input`]'s planes.
    fn run(&self, session: &mut Self::Session, input: Vec<u8>) -> Result<Vec<Region>, String>;
}

/// ONNX Runtime through the pinned `ort`.
pub(super) struct Ort;

// `ort`'s errors are not `Send`: flattened to text.
fn ort_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

impl Runner for Ort {
    type Session = Session;

    fn load(&self, bytes: &[u8], threads: usize) -> Result<Session, String> {
        Session::builder()
            .map_err(ort_err)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(ort_err)?
            .with_intra_threads(threads)
            .map_err(ort_err)?
            .with_intra_op_spinning(false)
            .map_err(ort_err)?
            .commit_from_memory(bytes)
            .map_err(ort_err)
    }

    fn run(&self, session: &mut Session, input: Vec<u8>) -> Result<Vec<Region>, String> {
        let side = SIDE as usize;
        let images = Tensor::from_array(([1usize, 3, side, side], input)).map_err(ort_err)?;
        // The boxes come back scaled to this size (width, height).
        let sizes = Tensor::from_array(([1usize, 2], vec![i64::from(SIDE), i64::from(SIDE)]))
            .map_err(ort_err)?;
        let outputs = session
            .run(ort::inputs!["images" => images, "orig_target_sizes" => sizes])
            .map_err(ort_err)?;
        let get = |name: &str| {
            outputs
                .get(name)
                .ok_or_else(|| format!("the model gave no output `{name}`"))
        };
        let (_, labels) = get("labels")?
            .try_extract_tensor::<i64>()
            .map_err(ort_err)?;
        let (_, boxes) = get("boxes")?.try_extract_tensor::<f32>().map_err(ort_err)?;
        let (_, scores) = get("scores")?
            .try_extract_tensor::<f32>()
            .map_err(ort_err)?;
        Ok(decode(labels, boxes, scores, SIDE as f32))
    }
}

struct Loaded<S> {
    session: S,
    threads: usize,
    path: PathBuf,
    last_used: Instant,
}

/// What Settings › Documents says of the session now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Now {
    /// Not in memory (a load going on, or one that failed, included).
    NotLoaded,
    /// Loaded with this many threads, last used this long ago.
    Loaded { threads: usize, idle: Duration },
    /// A page is being read with this many threads.
    Busy { threads: usize },
}

#[derive(Debug, Clone, Copy)]
enum Said {
    NotLoaded,
    Loaded { threads: usize, at: Instant },
    Busy { threads: usize },
}

type Cell<S> = Arc<Mutex<Option<Loaded<S>>>>;

/// The session behind its gate, with what it runs on.
pub(super) struct Gate<R: Runner> {
    runner: R,
    /// The file the bytes must be.
    model: ModelFile,
    pool: ToolPool,
    idle_unload: Duration,
    cell: Cell<R::Session>,
    /// Whether the task that drops an idle session is running.
    watching: AtomicBool,
    said: std::sync::Mutex<Said>,
}

/// The gateway's gate: ONNX Runtime, the pinned file, the tool pool.
fn global() -> &'static Arc<Gate<Ort>> {
    static GATE: OnceLock<Arc<Gate<Ort>>> = OnceLock::new();
    GATE.get_or_init(|| Gate::new(Ort, HERON, pool::global().clone(), LAYOUT_IDLE_UNLOAD))
}

/// What the gateway's session is doing now.
pub fn now() -> Now {
    global().now()
}

/// The regions of one page: `input` through the session at `path`, behind
/// the gate (module doc). The caller holds no slot.
pub async fn infer(path: &Path, input: Vec<u8>) -> Result<Vec<Region>, InferError> {
    global().infer(path, input).await
}

impl<R: Runner> Gate<R> {
    pub(super) fn new(
        runner: R,
        model: ModelFile,
        pool: ToolPool,
        idle_unload: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            runner,
            model,
            pool,
            idle_unload,
            cell: Arc::new(Mutex::new(None)),
            watching: AtomicBool::new(false),
            said: std::sync::Mutex::new(Said::NotLoaded),
        })
    }

    fn say(&self, s: Said) {
        *self.said.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }

    pub(super) fn now(&self) -> Now {
        match *self.said.lock().unwrap_or_else(|e| e.into_inner()) {
            Said::NotLoaded => Now::NotLoaded,
            Said::Loaded { threads, at } => Now::Loaded {
                threads,
                idle: at.elapsed(),
            },
            Said::Busy { threads } => Now::Busy { threads },
        }
    }

    /// The gate's work on its blocking thread: the session loaded (for
    /// `path` and `threads`), then one run.
    fn at_gate(
        &self,
        slot: &mut Option<Loaded<R::Session>>,
        path: &Path,
        threads: usize,
        input: Vec<u8>,
    ) -> Result<Vec<Region>, InferError> {
        let stale = slot
            .as_ref()
            .is_some_and(|l| l.threads != threads || l.path != path);
        if stale {
            *slot = None;
            self.say(Said::NotLoaded);
        }
        if slot.is_none() {
            let bytes = files::read_checked(&self.model, path).map_err(|why| {
                InferError::Unusable(format!("the file at its place is not the model: {why}"))
            })?;
            let session = self.runner.load(&bytes, threads).map_err(|e| {
                InferError::Unusable(format!("ONNX Runtime could not load the model: {e}"))
            })?;
            drop(bytes);
            tracing::info!(threads, "the layout model is loaded");
            *slot = Some(Loaded {
                session,
                threads,
                path: path.to_path_buf(),
                last_used: Instant::now(),
            });
        }
        let loaded = slot.as_mut().expect("loaded above");
        self.say(Said::Busy { threads });
        let out = self.runner.run(&mut loaded.session, input);
        loaded.last_used = Instant::now();
        self.say(Said::Loaded {
            threads,
            at: loaded.last_used,
        });
        out.map_err(|e| InferError::Page(format!("the model's run failed: {e}")))
    }

    /// Drop the session once it has been idle for `idle_unload`. The flag
    /// is cleared under the gate, so a page that loads a session after the
    /// last look always finds it cleared and starts another watcher.
    async fn watch(self: Arc<Self>) {
        loop {
            let wait = {
                let mut g = self.cell.lock().await;
                let idle = g.as_ref().map(|l| l.last_used.elapsed());
                match idle {
                    None => {
                        self.watching.store(false, Ordering::Release);
                        return;
                    }
                    Some(idle) if idle >= self.idle_unload => {
                        let gone = g.take();
                        self.say(Said::NotLoaded);
                        self.watching.store(false, Ordering::Release);
                        drop(g);
                        // Freeing the weights is work: off the runtime's threads.
                        tokio::task::spawn_blocking(move || drop(gone));
                        tracing::info!("the layout model is unloaded (idle)");
                        return;
                    }
                    Some(idle) => self.idle_unload - idle,
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// See [`infer`].
    pub(super) async fn infer(
        self: &Arc<Self>,
        path: &Path,
        input: Vec<u8>,
    ) -> Result<Vec<Region>, InferError> {
        let (guard, slots) = {
            // Waiting at the gate is not the file's own time.
            let _paused = clock::paused();
            let guard: OwnedMutexGuard<Option<Loaded<R::Session>>> =
                self.cell.clone().lock_owned().await;
            let slots = self
                .pool
                .acquire_charged(LAYOUT_THREADS, LAYOUT_MEMORY_BYTES)
                .await;
            (guard, slots)
        };
        // Armed before anything is awaited: a caller that goes away during
        // the load still leaves a session that is dropped when idle.
        if !self.watching.swap(true, Ordering::AcqRel) {
            tokio::spawn(self.clone().watch());
        }
        let threads = slots.count();
        let path = path.to_path_buf();
        let me = self.clone();
        let done = tokio::task::spawn_blocking(move || {
            // Held until the run has ended, whoever still waits for it.
            let (_slots, mut guard) = (slots, guard);
            let out = me.at_gate(&mut guard, &path, threads, input);
            if guard.is_none() {
                me.say(Said::NotLoaded);
            }
            out
        })
        .await;
        match done {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("the layout model's run stopped: {e}");
                // A run that panicked may have left the session half-way:
                // it is loaded anew by the next page.
                let gone = self.cell.lock().await.take();
                self.say(Said::NotLoaded);
                tokio::task::spawn_blocking(move || drop(gone));
                Err(InferError::Page(
                    "the model's run stopped unexpectedly".into(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests;
