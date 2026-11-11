//! The serving loop's quit (headless server design §1 steps 7–8): the first
//! signal stops the server, the teardown runs once it returned, and a second
//! signal exits at once with 128 plus its number.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::state::AppState;
use lmgw_server::start::serve_until_quit;
use tokio::sync::{mpsc, oneshot};

#[tokio::test]
async fn the_teardown_runs_after_the_server_returned() {
    let state = AppState::init_for_tests().await.unwrap();
    let listener = lmgw_core::server::bind(&state, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (signals, rx) = mpsc::channel(2);
    // What the teardown saw: whether the server still accepted.
    let seen: Arc<Mutex<Option<bool>>> = Arc::default();
    let exits: Arc<Mutex<Vec<i32>>> = Arc::default();
    let serving = tokio::spawn({
        let (seen, exits) = (seen.clone(), exits.clone());
        serve_until_quit(
            state.clone(),
            listener,
            None,
            rx,
            move |code| exits.lock().unwrap().push(code),
            move |_| async move {
                let accepting = tokio::net::TcpStream::connect(addr).await.is_ok();
                *seen.lock().unwrap() = Some(accepting);
            },
        )
    });
    // Serving: the dashboard answers.
    let mut conn = None;
    for _ in 0..100 {
        if let Ok(c) = tokio::net::TcpStream::connect(addr).await {
            conn = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(conn.expect("the server accepts"));
    assert_eq!(*seen.lock().unwrap(), None, "no teardown before the quit");

    signals.send("SIGTERM").await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), serving)
        .await
        .expect("the stop ends within its bound")
        .unwrap()
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        Some(false),
        "the teardown ran once the listener had closed"
    );
    assert!(exits.lock().unwrap().is_empty(), "one signal never exits");
}

#[tokio::test]
async fn a_second_sigterm_exits_143_at_once() {
    let state = AppState::init_for_tests().await.unwrap();
    let listener = lmgw_core::server::bind(&state, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let (signals, rx) = mpsc::channel(2);
    let (exited, exit_code) = oneshot::channel();
    // A teardown that would never end: only the second signal gets out.
    let (_never, hang) = oneshot::channel::<()>();
    let serving = tokio::spawn(serve_until_quit(
        state.clone(),
        listener,
        None,
        rx,
        move |code| {
            let _ = exited.send(code);
        },
        move |_| async move {
            let _ = hang.await;
        },
    ));
    signals.send("SIGTERM").await.unwrap();
    signals.send("SIGTERM").await.unwrap();
    let code = tokio::time::timeout(Duration::from_secs(30), exit_code)
        .await
        .expect("the second signal exits")
        .unwrap();
    assert_eq!(code, 143);
    serving.abort();
}
