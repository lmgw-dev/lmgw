//! Registry ownership, the join-only claim and draining for the owner
//! (candidate-aliases design §4.4–§4.5, §12 entries 46–48) — the registry
//! half, against a fake `podman` and wiremock `/health` containers, like
//! `runtime_registry.rs`. The VRAM half (what a background start may evict,
//! when it is blocked) is `background_admission.rs`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::config::LlamaParams;
use lmgw_core::runtime::argv::LlamaArgs;
use lmgw_core::runtime::descriptor::{ModelRuntime, RungPos};
use lmgw_core::runtime::registry::{
    AcquireSpec, ClimbStart, CmdOutput, CommandRunner, Marked, Origin, Registry, RuntimeError,
    RuntimeState, RuntimeView, StartSpec,
};
use lmgw_core::runtime::Class;
use tokio::sync::watch;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A podman that records every call, can hold a `run` or a `wait` open, and
/// can fail the next `run`.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    run_gate: Mutex<Option<watch::Receiver<bool>>>,
    wait_gate: Mutex<Option<watch::Receiver<bool>>>,
    fail_next_run: Mutex<bool>,
}

async fn parked(gate: Option<watch::Receiver<bool>>) {
    if let Some(mut rx) = gate {
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
}

fn ok() -> CmdOutput {
    CmdOutput {
        status: 0,
        stdout: "c0ffee\n".into(),
        stderr: String::new(),
    }
}

#[async_trait::async_trait]
impl CommandRunner for Fake {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "podman");
        self.calls.lock().unwrap().push(args.to_vec());
        match args[0].as_str() {
            "run" => {
                let gate = self.run_gate.lock().unwrap().clone();
                parked(gate).await;
                if std::mem::take(&mut *self.fail_next_run.lock().unwrap()) {
                    return Ok(CmdOutput {
                        status: 125,
                        stdout: String::new(),
                        stderr: "Error: the weights are not there\n".into(),
                    });
                }
                Ok(ok())
            }
            "wait" => {
                let gate = self.wait_gate.lock().unwrap().clone();
                parked(gate).await;
                Ok(ok())
            }
            _ => Ok(ok()),
        }
    }
}

impl Fake {
    fn runs(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == "run")
            .count()
    }

    fn stops(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == "stop")
            .count()
    }

    /// Hold every `run` until the returned sender says go.
    fn hold_runs(&self) -> watch::Sender<bool> {
        let (tx, rx) = watch::channel(false);
        *self.run_gate.lock().unwrap() = Some(rx);
        tx
    }

    /// Hold every `podman wait` — a stop in progress — until the sender says go.
    fn hold_waits(&self) -> watch::Sender<bool> {
        let (tx, rx) = watch::channel(false);
        *self.wait_gate.lock().unwrap() = Some(rx);
        tx
    }
}

async fn healthy() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
        .mount(&server)
        .await;
    server
}

fn registry(runner: Arc<Fake>, ports: Vec<u16>) -> Arc<Registry> {
    let queue = Arc::new(Mutex::new(VecDeque::from(ports)));
    Arc::new(Registry::with_ports(
        runner,
        reqwest::Client::new(),
        Arc::new(move || {
            queue.lock().unwrap().pop_front().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    "test allocator ran out of ports — an unexpected extra start",
                )
            })
        }),
    ))
}

fn runtime(model_id: &str) -> ModelRuntime {
    ModelRuntime {
        class: Class::Chat,
        model_id: model_id.into(),
        image: "localhost/llama-server-cuda:official-latest".into(),
        extra_run_args: vec![],
        idle_seconds: 0,
        warm_start: false,
        enabled: true,
        llama: Some(LlamaArgs::Chat {
            gguf_path: format!("{model_id}.gguf"),
            params: Box::default(),
            args: vec![],
        }),
        audio: None,
        audio_settings: None,
        image_model: None,
        sdcpp_caps: None,
        rung: None,
    }
}

fn spec(rt: &ModelRuntime) -> AcquireSpec<'_> {
    AcquireSpec {
        runtime: rt,
        container_prefix: "lmgw",
        models_dir: "/srv/models",
        data_dir: Path::new("/nonexistent/lmgw-test-data-dir"),
        may_write_models_dir: true,
        load_timeout: Duration::from_secs(5),
        stop_timeout: Duration::from_secs(5),
    }
}

fn view(reg: &Registry, model_id: &str) -> Option<RuntimeView> {
    reg.list().into_iter().find(|v| v.model_id == model_id)
}

fn owner(reg: &Registry, model_id: &str) -> Origin {
    view(reg, model_id)
        .expect("the model is in the registry")
        .owner
}

/// Poll `cond` until it holds; the registry has no event to await for a park
/// or a state change the test did not cause itself.
async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

// ---------------------------------------------------------------------------
// Ownership (§4.4, entry 48)
// ---------------------------------------------------------------------------

/// A background start creates a `Background` entry, published as such; the
/// owner's first claim makes it theirs, and it stays theirs after they let go. The
/// owner's frame carries no `owner` key at all.
#[tokio::test]
async fn a_background_start_is_the_guests_until_the_owner_claims_it() {
    let c = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![c.address().port()]);
    let rt = runtime("m");

    let guest = reg
        .acquire_as(&spec(&rt), Origin::Background)
        .await
        .unwrap();
    assert_eq!(owner(&reg, "m"), Origin::Background);
    let frame = serde_json::to_value(view(&reg, "m").unwrap()).unwrap();
    assert_eq!(frame["owner"], "background");

    // A second background claim changes nothing.
    let again = reg
        .acquire_as(&spec(&rt), Origin::Background)
        .await
        .unwrap();
    assert_eq!(owner(&reg, "m"), Origin::Background);
    drop(again);

    let mine = reg.acquire(&spec(&rt)).await.unwrap();
    assert_eq!(owner(&reg, "m"), Origin::Owner);
    drop(mine);
    drop(guest);
    assert_eq!(
        owner(&reg, "m"),
        Origin::Owner,
        "the owner's until it stops"
    );
    let frame = serde_json::to_value(view(&reg, "m").unwrap()).unwrap();
    assert!(
        frame.get("owner").is_none(),
        "absent for the owner: {frame}"
    );
    assert!(frame.get("draining_for_owner").is_none(), "{frame}");
    assert_eq!(fake.runs(), 1, "one container throughout");
}

/// Ownership lives and dies with the container: after a stop, the next start
/// records whoever starts it — the owner's earlier claim does not carry
/// over, and a background claim on the owner's model leaves it the owner's.
#[tokio::test]
async fn a_stop_forgets_the_owner_and_the_next_start_records_its_own() {
    let (a, b) = (healthy().await, healthy().await);
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![a.address().port(), b.address().port()]);
    let rt = runtime("m");

    let mine = reg.acquire(&spec(&rt)).await.unwrap();
    let guest = reg
        .join(Class::Chat, "m", Origin::Background)
        .await
        .unwrap()
        .expect("a loaded model is joined");
    assert_eq!(
        owner(&reg, "m"),
        Origin::Owner,
        "a guest never takes it over"
    );
    drop((mine, guest));

    reg.stop(Class::Chat, "m", false).await.unwrap();
    assert!(view(&reg, "m").is_none());
    let guest = reg
        .acquire_as(&spec(&rt), Origin::Background)
        .await
        .unwrap();
    assert_eq!(owner(&reg, "m"), Origin::Background, "a new container");
    drop(guest);
    assert_eq!(fake.runs(), 2);
}

/// The owner's arrival at a background start in flight is a claim on it
/// (joining a start): the entry is the owner's from the moment they park, before it is
/// ready, and both requests share the one start.
#[tokio::test]
async fn an_owner_parked_on_a_background_start_owns_it_before_it_is_ready() {
    let c = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![c.address().port()]);
    let rt = runtime("m");
    let go = fake.hold_runs();

    let guest = tokio::spawn({
        let (reg, rt) = (reg.clone(), rt.clone());
        async move { reg.acquire_as(&spec(&rt), Origin::Background).await }
    });
    until("the background start is in flight", || fake.runs() == 1).await;
    assert_eq!(owner(&reg, "m"), Origin::Background);
    assert_eq!(view(&reg, "m").unwrap().state, RuntimeState::Starting);

    let mine = tokio::spawn({
        let (reg, rt) = (reg.clone(), rt.clone());
        async move { reg.acquire(&spec(&rt)).await }
    });
    until("the owner's park made it theirs", || {
        owner(&reg, "m") == Origin::Owner
    })
    .await;
    assert_eq!(view(&reg, "m").unwrap().state, RuntimeState::Starting);

    go.send_replace(true);
    let (guest, mine) = (guest.await.unwrap().unwrap(), mine.await.unwrap().unwrap());
    assert_eq!(owner(&reg, "m"), Origin::Owner);
    assert_eq!(fake.runs(), 1, "one start served both");
    drop((guest, mine));
}

/// A climb replaces the container under the same entry, so the entry keeps
/// its owner (entry 48): a guest's ladder stays the guest's across its climb.
#[tokio::test]
async fn a_climb_keeps_the_entrys_owner() {
    let (old, new) = (healthy().await, healthy().await);
    let fake = Arc::new(Fake::default());
    let reg = registry(
        fake.clone(),
        vec![old.address().port(), new.address().port()],
    );
    let ladder = |index: usize| {
        let mut rt = runtime("lad");
        rt.llama = Some(LlamaArgs::Chat {
            gguf_path: format!("lad-{index}.gguf"),
            params: Box::new(LlamaParams {
                ctx_size: Some(64 << index),
                parallel: Some(1),
                n_predict: Some(16),
                ..Default::default()
            }),
            args: vec![],
        });
        rt.rung = Some(RungPos { index, of: 2 });
        rt
    };
    let (base, top) = (ladder(0), ladder(1));

    let claim = reg
        .acquire_as(&spec(&base), Origin::Background)
        .await
        .unwrap();
    let mut climb = match reg.mark_climb(&claim, RungPos { index: 1, of: 2 }, "why") {
        Marked::Ticket(t) => t,
        other => panic!("expected the ticket, got {other:?}"),
    };
    climb.drain(None).await.unwrap();
    let run = climb.start(StartSpec::of(&spec(&top))).expect("still ours");
    run.finish().await.unwrap();
    let v = view(&reg, "lad").unwrap();
    assert_eq!(v.rung.as_ref().map(|r| r.rung), Some(2), "climbed");
    assert_eq!(v.owner, Origin::Background, "the climb kept the owner");
    drop(claim);
}

// ---------------------------------------------------------------------------
// The join-only claim (entry 46)
// ---------------------------------------------------------------------------

/// A join never starts anything: an absent model is `None` with no podman
/// call, and so is one on its way out — a stop in progress.
#[tokio::test]
async fn a_join_never_starts_a_model_that_is_absent_or_stopping() {
    let c = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![c.address().port()]);
    let rt = runtime("m");

    for origin in [Origin::Owner, Origin::Background] {
        assert!(reg.join(Class::Chat, "m", origin).await.unwrap().is_none());
    }
    assert_eq!(fake.runs(), 0, "nothing was started");

    drop(reg.acquire(&spec(&rt)).await.unwrap());
    let go = fake.hold_waits();
    let stopping = tokio::spawn({
        let reg = reg.clone();
        async move { reg.stop(Class::Chat, "m", false).await }
    });
    until("the stop has taken the entry", || {
        view(&reg, "m").is_some_and(|v| v.state == RuntimeState::Stopping)
    })
    .await;
    assert!(
        reg.join(Class::Chat, "m", Origin::Owner)
            .await
            .unwrap()
            .is_none(),
        "a model being stopped is not loaded"
    );
    go.send_replace(true);
    stopping.await.unwrap().unwrap();
    assert_eq!(fake.runs(), 1, "the join started nothing");
}

/// A join on a start in flight waits for it and shares it — also when the
/// request that started it went away meanwhile: the start is the registry's
/// (`owned.rs`), so it lands regardless, and the join holds the one claim on
/// it. A join never starts anything itself.
#[tokio::test]
async fn a_join_shares_a_start_in_flight_but_never_takes_one_over() {
    let c = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![c.address().port(), c.address().port()]);
    let rt = runtime("m");
    let go = fake.hold_runs();

    // Abandoned: the starting request is dropped while the join waits on it.
    let starter = tokio::spawn({
        let (reg, rt) = (reg.clone(), rt.clone());
        async move { reg.acquire(&spec(&rt)).await }
    });
    until("the start is in flight", || fake.runs() == 1).await;
    let joiner = tokio::spawn({
        let reg = reg.clone();
        async move { reg.join(Class::Chat, "m", Origin::Background).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!joiner.is_finished(), "it waits on the start");
    starter.abort();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !joiner.is_finished(),
        "the start goes on without its requester, and the join with it"
    );
    go.send_replace(true);
    let joined = joiner.await.unwrap().unwrap().expect("the start it joined");
    assert_eq!(
        view(&reg, "m").unwrap().in_flight,
        1,
        "the join's claim, and none for the requester that went away"
    );
    assert_eq!(fake.runs(), 1);
    drop(joined);
    reg.stop(Class::Chat, "m", false).await.unwrap();
    go.send_replace(false);

    // Shared: the start lands, and the join holds a claim on it.
    let starter = tokio::spawn({
        let (reg, rt) = (reg.clone(), rt.clone());
        async move { reg.acquire(&spec(&rt)).await }
    });
    until("the second start is in flight", || fake.runs() == 2).await;
    let joiner = tokio::spawn({
        let reg = reg.clone();
        async move { reg.join(Class::Chat, "m", Origin::Background).await }
    });
    go.send_replace(true);
    let started = starter.await.unwrap().unwrap();
    let joined = joiner.await.unwrap().unwrap().expect("the start it joined");
    assert_eq!(joined.port(), started.port());
    assert_eq!(view(&reg, "m").unwrap().in_flight, 2);
    assert_eq!(owner(&reg, "m"), Origin::Owner, "the owner's start");
}

/// A joined start that fails is that start's error, not "not loaded": a
/// second attempt at a model that just failed to load would only pay its
/// load again.
#[tokio::test]
async fn a_joined_start_that_fails_is_that_starts_error() {
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![1]);
    let rt = runtime("m");
    let go = fake.hold_runs();
    *fake.fail_next_run.lock().unwrap() = true;

    let starter = tokio::spawn({
        let (reg, rt) = (reg.clone(), rt.clone());
        async move { reg.acquire(&spec(&rt)).await }
    });
    until("the start is in flight", || fake.runs() == 1).await;
    let joiner = tokio::spawn({
        let reg = reg.clone();
        async move { reg.join(Class::Chat, "m", Origin::Owner).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    go.send_replace(true);
    assert!(starter.await.unwrap().is_err());
    match joiner.await.unwrap() {
        Err(RuntimeError::Start { .. }) => {}
        other => panic!("expected the start's error, got {other:?}"),
    }
    assert_eq!(fake.runs(), 1);
}

// ---------------------------------------------------------------------------
// Draining for the owner (§4.5, entry 47)
// ---------------------------------------------------------------------------

/// While an owner admission waits for room, background claims refuse every
/// resident model and owner claims do not; the mark is on every entry of the
/// frame, and clears when the last waiter leaves.
#[tokio::test]
async fn draining_refuses_background_claims_until_the_last_owner_waiter_leaves() {
    let (a, b) = (healthy().await, healthy().await);
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![a.address().port(), b.address().port()]);
    let (m, n) = (runtime("m"), runtime("n"));
    drop(reg.acquire(&spec(&m)).await.unwrap());
    drop(reg.acquire_as(&spec(&n), Origin::Background).await.unwrap());

    let first = reg.drain_for_owner();
    let second = reg.drain_for_owner();
    assert!(reg.draining_for_owner());
    for v in reg.list() {
        assert!(v.draining_for_owner, "{} is draining", v.model_id);
        let frame = serde_json::to_value(&v).unwrap();
        assert_eq!(frame["draining_for_owner"], true);
    }
    for model in ["m", "n"] {
        assert!(
            reg.join(Class::Chat, model, Origin::Background)
                .await
                .unwrap()
                .is_none(),
            "a guest takes no new work on {model}"
        );
    }
    let mine = reg
        .join(Class::Chat, "n", Origin::Owner)
        .await
        .unwrap()
        .expect("the owner's join is never refused");
    assert_eq!(owner(&reg, "n"), Origin::Owner);
    drop(mine);

    drop(first);
    assert!(reg.draining_for_owner(), "one waiter is still there");
    drop(second);
    assert!(!reg.draining_for_owner());
    assert!(reg.list().iter().all(|v| !v.draining_for_owner));
    assert!(reg
        .join(Class::Chat, "m", Origin::Background)
        .await
        .unwrap()
        .is_some());
}

// ---------------------------------------------------------------------------
// Background eviction (§4.4: "background may evict only idle Background")
// ---------------------------------------------------------------------------

/// The stop a background eviction uses stops only a container that is still
/// idle and still the guest's: one the owner claimed since it was judged —
/// even a claim already released — is `Moved`, and a busy one `Busy`.
#[tokio::test]
async fn a_background_eviction_never_stops_the_owners_model() {
    let (a, b) = (healthy().await, healthy().await);
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![a.address().port(), b.address().port()]);
    let (m, n) = (runtime("m"), runtime("n"));

    drop(reg.acquire_as(&spec(&m), Origin::Background).await.unwrap());
    let judged = view(&reg, "m").unwrap().generation;
    drop(reg.join(Class::Chat, "m", Origin::Owner).await.unwrap());
    match reg.stop_idle_background(Class::Chat, "m", judged).await {
        Err(RuntimeError::Moved { .. }) => {}
        other => panic!("expected Moved, got {other:?}"),
    }
    assert!(view(&reg, "m").is_some(), "the owner's model is untouched");

    let busy = reg.acquire_as(&spec(&n), Origin::Background).await.unwrap();
    let judged = view(&reg, "n").unwrap().generation;
    match reg.stop_idle_background(Class::Chat, "n", judged).await {
        Err(RuntimeError::Busy { .. }) => {}
        other => panic!("expected Busy, got {other:?}"),
    }
    drop(busy);
    reg.stop_idle_background(Class::Chat, "n", judged)
        .await
        .expect("an idle guest is evictable");
    assert!(view(&reg, "n").is_none());
    assert_eq!(fake.stops(), 1);
}

// ---------------------------------------------------------------------------
// A guest's climb start (§12 entry 89)
// ---------------------------------------------------------------------------

/// A guest's climb start asks, in the lock hold that claims it, whether the
/// model is still the guest's and whether the owner waits for room: an owner
/// who parked on the climb after its drain made the model theirs, and the start
/// is refused — the mark cleared, the owner's parked claim served by the running
/// rung. The same while an owner admission waits for room.
#[tokio::test]
async fn a_guest_climb_start_is_refused_in_the_claiming_lock_hold() {
    let (first, second) = (healthy().await, healthy().await);
    let fake = Arc::new(Fake::default());
    let reg = registry(
        fake.clone(),
        vec![first.address().port(), second.address().port()],
    );
    let ladder = |model: &str, index: usize| {
        let mut rt = runtime(model);
        rt.llama = Some(LlamaArgs::Chat {
            gguf_path: format!("{model}-{index}.gguf"),
            params: Box::new(LlamaParams {
                ctx_size: Some(64 << index),
                parallel: Some(1),
                n_predict: Some(16),
                ..Default::default()
            }),
            args: vec![],
        });
        rt.rung = Some(RungPos { index, of: 2 });
        rt
    };

    // The owner parks on the guest's climb after its drain.
    let (base, top) = (ladder("lad", 0), ladder("lad", 1));
    let claim = reg
        .acquire_as(&spec(&base), Origin::Background)
        .await
        .unwrap();
    let mut climb = match reg.mark_climb(&claim, RungPos { index: 1, of: 2 }, "why") {
        Marked::Ticket(t) => t,
        other => panic!("expected the ticket, got {other:?}"),
    };
    climb.drain(None).await.unwrap();
    let parked = tokio::spawn({
        let (reg, base) = (reg.clone(), base.clone());
        async move { reg.acquire(&spec(&base)).await.map(drop) }
    });
    until("the owner parked on the climb", || {
        owner(&reg, "lad") == Origin::Owner
    })
    .await;
    match climb.start_for_guest(StartSpec::of(&spec(&top))) {
        ClimbStart::Refused(why) => assert!(why.contains("owner's model"), "{why}"),
        other => panic!("expected the refusal, got {other:?}"),
    }
    parked.await.unwrap().expect("served by the running rung");
    let v = view(&reg, "lad").unwrap();
    assert_eq!(v.rung.as_ref().map(|r| r.rung), Some(1), "not climbed");
    assert!(v.climbing.is_none(), "the mark is cleared");
    drop(claim);

    // An owner admission waits for room.
    let (base, top) = (ladder("lad2", 0), ladder("lad2", 1));
    let claim = reg
        .acquire_as(&spec(&base), Origin::Background)
        .await
        .unwrap();
    let mut climb = match reg.mark_climb(&claim, RungPos { index: 1, of: 2 }, "why") {
        Marked::Ticket(t) => t,
        other => panic!("expected the ticket, got {other:?}"),
    };
    climb.drain(None).await.unwrap();
    let waiting = reg.drain_for_owner();
    match climb.start_for_guest(StartSpec::of(&spec(&top))) {
        ClimbStart::Refused(why) => assert!(why.contains("waiting for room"), "{why}"),
        other => panic!("expected the refusal, got {other:?}"),
    }
    drop(waiting);
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == "run")
            .count(),
        2,
        "the two bases, no rung"
    );
    drop(claim);
}
