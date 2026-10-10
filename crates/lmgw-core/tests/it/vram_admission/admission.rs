//! Admission

use super::*;

/// A model larger than the GPU is refused at once, by name. Queueing it would
/// be a wait it can never win, and the caller would learn nothing.
#[tokio::test]
async fn a_model_bigger_than_the_gpu_is_refused_immediately() {
    let f = fixture(4 * GIB, 8 * GIB, GIB, 512).await;

    let started = std::time::Instant::now();
    let resp = chat(&f.gateway).await;

    assert_eq!(resp.status(), 507, "insufficient storage, not a timeout");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "it must not sit in the queue first"
    );
    let body: Value = resp.json().await.unwrap();
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("chat-model") && msg.contains("no eviction can make it fit"),
        "the refusal has to name the model and say waiting will not help: {body}"
    );
    assert_eq!(body["error"]["code"], "vram_too_large");

    // Refused before any container was created.
    assert!(f.runs().is_empty() && f.stops().is_empty());
}

/// The motivating failure of §9b, made to pass: a chat model fills the GPU and
/// an embedding request arrives. Before this, that was an OOM crash in
/// llama-server. Now the idle chat model's container is stopped, the embedder's
/// is started, and the request is answered.
#[tokio::test]
async fn an_embed_request_evicts_the_idle_chat_model_that_filled_the_gpu() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);

    // Ledger first: lmgw believes the chat model is holding 6 GiB, and the
    // driver agrees.
    let v = vram_status(&f.gateway).await;
    assert_eq!(v["estimated_resident_bytes"], 6 * GIB);
    assert_eq!(v["devices"][0]["used_bytes"], 6 * GIB);
    assert_eq!(v["free_bytes"], 2 * GIB);
    assert_eq!(v["resident"][0]["model"], "chat-model");
    assert_eq!(v["resident"][0]["container"], "chat");
    assert_eq!(v["resident"][0]["state"], "ready");

    assert_eq!(
        embed(&f.gateway).await.status(),
        200,
        "the request has to be answered"
    );

    assert_eq!(
        f.stops(),
        vec!["chat-model".to_string()],
        "the idle chat model is the victim"
    );
    assert_eq!(
        f.runs(),
        vec!["chat-model".to_string(), "embed-model".to_string()],
        "and the embedder is started before the request is forwarded"
    );
    let w = f.world();
    assert!(!w.loaded.contains("chat-model"));
    assert!(w.loaded.contains("embed-model"));
}

/// Nothing is evicted when it fits: a request that has room must not disturb a
/// model that is happily resident. And the forward lands on the container
/// `acquire` started, not on the route's configured base URL (§5).
#[tokio::test]
async fn a_request_that_fits_evicts_nothing_and_forwards_to_the_held_endpoint() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(embed(&f.gateway).await.status(), 200);

    assert!(f.stops().is_empty(), "there was room: {:?}", f.stops());
    assert_eq!(
        f.runs(),
        vec!["chat-model".to_string(), "embed-model".to_string()]
    );

    // The embedding request reached the *second* container — the one the
    // embed model was started on — and not the chat model's, nor the dead
    // class port the aux upstream row still carries.
    let embed_port = {
        let w = f.world();
        *w.ports
            .iter()
            .find(|(_, model)| model.as_str() == "embed-model")
            .map(|(port, _)| port)
            .expect("the embed model was published on a port")
    };
    let served = f
        ._second
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .any(|r| r.url.path() == "/v1/embeddings");
    assert_eq!(embed_port, f._second.address().port());
    assert!(served, "the forward has to land on the acquired container");
}

/// `lmgw__local_model_test` used to post at `settings.router.listen_port` and
/// let the router autoload behind admission's back — a free-rider the design
/// names by hand (§5). It now builds the model's own route, goes through
/// `admit`, and calls the container that came up, which is the whole point of
/// a test that claims to exercise "the real path".
#[tokio::test]
async fn the_local_model_test_tool_admits_and_calls_the_container_it_started() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;

    let v = crate::common::model_test_wire(
        lmgw_core::modelinfo::local_model_test(&f.state, "chat-model", None)
            .await
            .unwrap(),
    );
    assert_eq!(v["ok"], serde_json::json!(true));

    assert_eq!(f.runs(), vec!["chat-model".to_string()]);
    let served = f
        .first
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .any(|r| r.url.path() == "/v1/chat/completions");
    assert!(
        served,
        "the probe has to land on the container admission started"
    );
}

/// lmgw is not the only ingress: the dashboard publishes container ports and
/// tells clients to use them. A generation started that way is invisible to the
/// in-flight ledger and visible only in `/slots` — so a victim whose slots are
/// processing is skipped, even at the cost of refusing the newcomer (§10.7).
#[tokio::test]
async fn a_busy_model_is_never_evicted_and_the_refusal_names_it() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    // Traffic that arrived on the container's own port: nothing in lmgw's
    // ledger knows about it.
    f.world().busy.insert("chat-model".into());

    let started = std::time::Instant::now();
    let resp = embed(&f.gateway).await;

    assert_eq!(resp.status(), 503, "at capacity, not a client error");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "it should have waited out queue_timeout_seconds first"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "vram_queue_timeout");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("chat-model"),
        "the refusal has to name what held the GPU: {body}"
    );

    assert!(
        f.stops().is_empty(),
        "a generating model must not be stopped"
    );
}

/// A wait is a thing the owner can see while it is happening — position, what
/// it needs, how long it has been there — not an inference drawn afterwards
/// from a slow response.
#[tokio::test]
async fn a_waiting_request_is_visible_in_the_queue() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    f.world().busy.insert("chat-model".into());

    let gateway = f.gateway.clone();
    let waiting = tokio::spawn(async move { embed(&gateway).await.status() });

    tokio::time::sleep(Duration::from_millis(700)).await;
    let v = vram_status(&f.gateway).await;
    let queue = v["queue"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        queue.len(),
        1,
        "the wait must be visible while it lasts: {v}"
    );
    assert_eq!(queue[0]["position"], 1);
    assert_eq!(queue[0]["model"], "embed-model");
    assert_eq!(queue[0]["container"], "aux");
    assert_eq!(queue[0]["alias"], "embed/embed-model");
    assert_eq!(queue[0]["needs_bytes"], 3 * GIB + 512 * 1024 * 1024);
    assert!(queue[0]["waiting_ms"].as_u64().unwrap_or(0) > 0);
    assert!(!queue[0]["stage"].as_str().unwrap_or_default().is_empty());

    assert_eq!(waiting.await.unwrap(), 503);
    // The queue empties itself when the wait ends, however it ends.
    let v = vram_status(&f.gateway).await;
    assert!(v["queue"].as_array().is_none_or(|q| q.is_empty()), "{v}");
}

/// A wait that ends because the client hung up must leave the queue too. The
/// handler future is dropped mid-await when the connection closes, so nothing
/// written after the wait in `arbitrate` runs — observed on the live box as
/// two entries that outlived their disconnected clients and sat in the
/// dashboard's queue with no request behind them. The check runs well inside
/// the fixture's queue timeout, so it cannot pass by the wait merely expiring.
#[tokio::test]
async fn a_disconnected_client_leaves_the_queue() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    f.world().busy.insert("chat-model".into());

    let started = std::time::Instant::now();
    let gateway = f.gateway.clone();
    let waiting = tokio::spawn(async move { embed(&gateway).await.status() });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let v = vram_status(&f.gateway).await;
    assert_eq!(
        v["queue"].as_array().map_or(0, |q| q.len()),
        1,
        "the wait must be queued before the client goes away: {v}"
    );

    // The client goes away: aborting the task drops the request future and
    // with it the connection.
    waiting.abort();
    let _ = waiting.await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let v = vram_status(&f.gateway).await;
    assert!(
        v["queue"].as_array().is_none_or(|q| q.is_empty()),
        "a cancelled wait must not linger in the queue: {v}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the queue must have emptied on the disconnect, not on the timeout"
    );
    assert!(
        f.stops().is_empty(),
        "the busy model must not have been stopped on the way out"
    );
}

/// An already-running model is forwarded with no arbitration at all: the fast
/// path must not put a serialization point in front of traffic that fits, and
/// must not start a second container for a model that has one.
#[tokio::test]
async fn a_resident_model_is_forwarded_without_touching_the_lifecycle() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    assert_eq!(chat(&f.gateway).await.status(), 200);

    assert_eq!(f.runs(), vec!["chat-model".to_string()], "started once");
    assert!(f.stops().is_empty());
}

/// The §4 fix, pinned end to end: the decision gate is released *before* the
/// container start, so two models load at once. The barrier is the assertion —
/// it only releases when both `podman run`s are in flight simultaneously, so a
/// scheduler that held the gate across the load would hang here, and the
/// timeout turns that hang into a failure.
#[tokio::test]
async fn admissions_of_two_models_overlap_through_the_gate_and_the_start() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 512).await;
    *f.podman.barrier.lock().unwrap() = Some(Arc::new(tokio::sync::Barrier::new(2)));

    let state = f.state.clone();
    let one = tokio::spawn(async move {
        let route = state.snapshot().resolve("chat-model").unwrap();
        lmgw_core::vram::admit(&state, &route, "chat-model")
            .await
            .map(|h| h.map(|h| (h.port(), h)))
    });
    let state = f.state.clone();
    let two = tokio::spawn(async move {
        let route = state.snapshot().resolve("embed/embed-model").unwrap();
        lmgw_core::vram::admit(&state, &route, "embed/embed-model")
            .await
            .map(|h| h.map(|h| (h.port(), h)))
    });

    let joined = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(one, two) })
        .await
        .expect("the two admissions never overlapped — they were serialized");
    let a = joined.0.unwrap().unwrap().unwrap();
    let b = joined.1.unwrap().unwrap().unwrap();

    assert_ne!(a.0, b.0, "each model got its own container and port");
    let mut runs = f.runs();
    runs.sort();
    assert_eq!(
        runs,
        vec!["chat-model".to_string(), "embed-model".to_string()]
    );
    assert!(f.stops().is_empty(), "16 GiB fits both");
}
