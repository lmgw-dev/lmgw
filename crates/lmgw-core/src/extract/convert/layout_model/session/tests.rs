//! The gate over a stand-in runner: its slots, its unload, its file check.

use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::sync::Mutex as StdMutex;

use super::super::super::pool_size::Machine;
use super::*;

/// The five bytes `hello`.
const STAND_IN: ModelFile = ModelFile {
    id: "example",
    label: "Example model",
    adds: "Nothing.",
    repo: "example-owner/example-model",
    revision: "0000000000000000000000000000000000000000",
    file: "model.onnx",
    sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
    size_bytes: 5,
    licence: "CC0-1.0",
};

/// A runner that counts, says when it is in a load or a run, and can be
/// held there until released.
#[derive(Default)]
struct Fake {
    loads: AtomicUsize,
    /// The thread count of each load.
    threads: StdMutex<Vec<usize>>,
    /// The pool's running slots seen during each run.
    running: StdMutex<Vec<u32>>,
    pool: StdMutex<Option<ToolPool>>,
    entered: StdMutex<Option<mpsc::Sender<&'static str>>>,
    hold_load: StdMutex<Option<mpsc::Receiver<()>>>,
    hold_run: StdMutex<Option<mpsc::Receiver<()>>>,
}

impl Fake {
    fn tell(&self, what: &'static str) {
        if let Some(tx) = self.entered.lock().unwrap().as_ref() {
            let _ = tx.send(what);
        }
    }
}

impl Runner for Arc<Fake> {
    type Session = Vec<u8>;

    fn load(&self, bytes: &[u8], threads: usize) -> Result<Vec<u8>, String> {
        self.tell("load");
        if let Some(rx) = self.hold_load.lock().unwrap().take() {
            let _ = rx.recv();
        }
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.threads.lock().unwrap().push(threads);
        Ok(bytes.to_vec())
    }

    fn run(&self, session: &mut Vec<u8>, _input: Vec<u8>) -> Result<Vec<Region>, String> {
        assert_eq!(
            session.as_slice(),
            b"hello",
            "only the checked bytes are loaded"
        );
        self.tell("run");
        if let Some(p) = self.pool.lock().unwrap().as_ref() {
            self.running.lock().unwrap().push(p.facts().running);
        }
        if let Some(rx) = self.hold_run.lock().unwrap().take() {
            let _ = rx.recv();
        }
        Ok(Vec::new())
    }
}

fn pool_of(slots: u32) -> ToolPool {
    let p = ToolPool::new(Machine {
        cores: 64,
        memory: None,
    });
    p.set_by_hand(slots);
    p
}

struct Rig {
    fake: Arc<Fake>,
    gate: Arc<Gate<Arc<Fake>>>,
    pool: ToolPool,
    path: PathBuf,
    _dir: tempfile::TempDir,
}

fn rig(slots: u32, idle: Duration) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model.onnx");
    std::fs::write(&path, b"hello").unwrap();
    let pool = pool_of(slots);
    let fake = Arc::new(Fake::default());
    *fake.pool.lock().unwrap() = Some(pool.clone());
    let gate = Gate::new(fake.clone(), STAND_IN, pool.clone(), idle);
    Rig {
        fake,
        gate,
        pool,
        path,
        _dir: dir,
    }
}

/// Wait up to two seconds for `cond`.
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let until = tokio::time::Instant::now() + Duration::from_secs(2);
    while !cond() {
        assert!(tokio::time::Instant::now() < until, "never: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The next thing the runner entered, waited for off the runtime.
async fn entered(rx: &Arc<StdMutex<mpsc::Receiver<&'static str>>>) -> &'static str {
    let rx = rx.clone();
    tokio::task::spawn_blocking(move || {
        rx.lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
    })
    .await
    .unwrap()
}

fn watch_entries(fake: &Fake) -> Arc<StdMutex<mpsc::Receiver<&'static str>>> {
    let (tx, rx) = mpsc::channel();
    *fake.entered.lock().unwrap() = Some(tx);
    Arc::new(StdMutex::new(rx))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_takes_as_many_slots_as_its_callers_share_up_to_four() {
    // One caller holds at most one slot less than the size (`pool::share`).
    for (slots, threads) in [(1u32, 1usize), (2, 1), (3, 2), (4, 3), (8, 4)] {
        let r = rig(slots, Duration::from_secs(60));
        r.gate.infer(&r.path, Vec::new()).await.unwrap();
        assert_eq!(
            *r.fake.threads.lock().unwrap(),
            vec![threads],
            "pool {slots}"
        );
        // The slots were held during the run, and given back after it.
        assert_eq!(
            *r.fake.running.lock().unwrap(),
            vec![threads as u32],
            "pool {slots}"
        );
        assert_eq!(r.pool.facts().running, 0);
        assert!(
            matches!(r.gate.now(), Now::Loaded { threads: t, .. } if t == threads),
            "{:?}",
            r.gate.now()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pool_set_smaller_while_a_page_waits_does_not_strand_it() {
    let r = rig(4, Duration::from_secs(60));
    // Held by another caller than the page's (`pool::share`).
    let other = || crate::extract::convert::pool::as_caller("other", r.pool.acquire());
    let held = vec![other().await, other().await, other().await];
    let (gate, path) = (r.gate.clone(), r.path.clone());
    let page = tokio::spawn(async move { gate.infer(&path, Vec::new()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // It holds nothing while it waits.
    assert_eq!(r.pool.facts().running, 3);
    r.pool.set_by_hand(3);
    drop(held);
    page.await.unwrap().unwrap();
    // Its share of three slots.
    assert_eq!(*r.fake.threads.lock().unwrap(), vec![2]);
    // Set larger again: the session is loaded anew for the new count.
    r.pool.set_by_hand(4);
    r.gate.infer(&r.path, Vec::new()).await.unwrap();
    assert_eq!(*r.fake.threads.lock().unwrap(), vec![2, 3]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_that_goes_away_mid_run_leaves_the_slots_until_the_run_ends() {
    // Three slots: the caller's share is two.
    let r = rig(3, Duration::from_secs(60));
    let rx = watch_entries(&r.fake);
    let (release, hold) = mpsc::channel();
    *r.fake.hold_run.lock().unwrap() = Some(hold);
    let (gate, path) = (r.gate.clone(), r.path.clone());
    let page = tokio::spawn(async move { gate.infer(&path, Vec::new()).await });
    assert_eq!(entered(&rx).await, "load");
    assert_eq!(entered(&rx).await, "run");
    assert_eq!(r.gate.now(), Now::Busy { threads: 2 });
    page.abort();
    let _ = page.await;
    // The run still goes on, on its slots.
    assert_eq!(r.pool.facts().running, 2);
    release.send(()).unwrap();
    let pool = r.pool.clone();
    eventually("slots back", move || pool.facts().running == 0).await;
    // The gate is free for the next page, on the loaded session.
    r.gate.infer(&r.path, Vec::new()).await.unwrap();
    assert_eq!(r.fake.loads.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_that_goes_away_during_the_first_load_still_has_it_unloaded() {
    let r = rig(2, Duration::from_millis(50));
    let rx = watch_entries(&r.fake);
    let (release, hold) = mpsc::channel();
    *r.fake.hold_load.lock().unwrap() = Some(hold);
    let (gate, path) = (r.gate.clone(), r.path.clone());
    let page = tokio::spawn(async move { gate.infer(&path, Vec::new()).await });
    assert_eq!(entered(&rx).await, "load");
    page.abort();
    let _ = page.await;
    release.send(()).unwrap();
    assert_eq!(entered(&rx).await, "run");
    let gate = r.gate.clone();
    eventually("unloaded after its idle time", move || {
        gate.now() == Now::NotLoaded && gate.cell.try_lock().is_ok_and(|g| g.is_none())
    })
    .await;
    assert_eq!(r.pool.facts().running, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pages_that_race_the_unload_always_leave_a_watcher() {
    let r = rig(2, Duration::from_millis(10));
    for i in 0..30u64 {
        r.gate.infer(&r.path, Vec::new()).await.unwrap();
        tokio::time::sleep(Duration::from_millis((i * 7) % 23)).await;
    }
    assert!(
        r.fake.loads.load(Ordering::SeqCst) >= 2,
        "it was unloaded between pages"
    );
    let gate = r.gate.clone();
    eventually("the last session unloaded", move || {
        gate.cell.try_lock().is_ok_and(|g| g.is_none())
    })
    .await;
    assert_eq!(r.gate.now(), Now::NotLoaded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_load_checks_the_bytes_it_loads() {
    let r = rig(1, Duration::from_millis(10));
    r.gate.infer(&r.path, Vec::new()).await.unwrap();
    let gate = r.gate.clone();
    eventually("unloaded", move || {
        gate.cell.try_lock().is_ok_and(|g| g.is_none())
    })
    .await;
    // Another file of the same size, with the first one's modification
    // time: loaded only if its bytes were the model's.
    let modified = std::fs::metadata(&r.path).unwrap().modified().unwrap();
    std::fs::write(&r.path, b"world").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&r.path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let e = r.gate.infer(&r.path, Vec::new()).await.unwrap_err();
    assert!(
        matches!(&e, InferError::Unusable(why) if why.contains("SHA-256")),
        "{e:?}"
    );
    assert_eq!(r.fake.loads.load(Ordering::SeqCst), 1);
    assert_eq!(r.gate.now(), Now::NotLoaded);
    // A file that grew is refused without being read whole.
    std::fs::write(&r.path, b"hello, longer").unwrap();
    let e = r.gate.infer(&r.path, Vec::new()).await.unwrap_err();
    assert!(
        matches!(&e, InferError::Unusable(why) if why.contains("or more")),
        "{e:?}"
    );
    assert_eq!(r.pool.facts().running, 0);
}

#[tokio::test(start_paused = true)]
async fn waiting_at_the_gate_does_not_count_on_the_file_s_clock() {
    let r = rig(1, Duration::from_secs(60));
    // Another page holds the gate for 30 s.
    let held = r.gate.cell.clone().lock_owned().await;
    let clock = pool::clock::FileClock::new(Duration::from_secs(10));
    let (gate, path, pool) = (r.gate.clone(), r.path.clone(), r.pool.clone());
    let page = tokio::spawn(pool::clock::scope(clock.clone(), async move {
        // The file's first step had its slot: the clock runs.
        drop(pool.acquire().await);
        let got = gate.infer(&path, Vec::new()).await;
        (got, pool::clock::current().map(|c| c.used()))
    }));
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(clock.used() < Duration::from_secs(1), "{:?}", clock.used());
    drop(held);
    let (got, used) = page.await.unwrap();
    got.unwrap();
    let used = used.unwrap();
    assert!(used < Duration::from_secs(1), "{used:?}");
}
