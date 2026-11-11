//! The start's sign-in lines (ruling 15, §5.3, §12 "Login links"), as a
//! served start logs them: in container mode a set-password link while
//! account 1 has none, a fresh one each start and never a key, then only
//! where to sign in and the command; on a desktop with accounts on, today's
//! line and no set-password link. The desktop with accounts off is
//! `main_tls::scheme::the_login_line_names_the_bound_listener`.

use lmgw_core::config::deployment::{Deployment, StartFacts};
use lmgw_core::state::AppState;

use super::*;
use crate::common::captured_log::capture_log;

/// One start on a loopback port: what it logged up to its sign-in line.
async fn start_log(state: &SharedState) -> String {
    let (log, _guard) = capture_log();
    let listener = lmgw_core::server::bind(state, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (released_tx, released) = tokio::sync::oneshot::channel();
    let served = tokio::spawn(lmgw_core::server::serve(
        state.clone(),
        listener,
        async {
            let _ = stopped.await;
        },
        Some(released_tx),
    ));
    crate::common::patience::until("the sign-in line", || {
        let t = log.text();
        t.contains("set a password for account")
            || t.contains("sign in at")
            || t.contains("dashboard login:")
    })
    .await;
    let _ = stop.send(());
    let _ = released.await;
    let _ = served.await;
    log.text()
}

/// A test state with no WebRTC listener, as the TLS suite starts one.
async fn quiet_state() -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let mut s = state.snapshot().settings.clone();
    s.realtime.webrtc.webrtc_enabled = false;
    lmgw_core::store::save_settings(&state.db, &s)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    state
}

/// The token of the set-password link in `log`.
fn link_token(log: &str) -> String {
    let (_, rest) = log.split_once("/set-password#token=").expect(log);
    rest.split(|c: char| !c.is_ascii_hexdigit())
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn a_container_start_prints_a_set_password_link_and_never_a_key() {
    let state = quiet_state().await;
    state.set_start_for_tests(StartFacts {
        deployment: Deployment::Container,
        ..Default::default()
    });
    let key = crate::common::dashboard_key(&state);

    let first = start_log(&state).await;
    assert!(
        first.contains("set a password for account 'owner': http://127.0.0.1:")
            && first.contains("single use"),
        "{first}"
    );
    let second = start_log(&state).await;
    assert_ne!(
        link_token(&first),
        link_token(&second),
        "a fresh link each start"
    );
    for log in [&first, &second] {
        assert!(!log.contains(&key), "the owner key is in the log: {log}");
        assert!(
            !log.contains("dashboard login") && !log.contains("?token="),
            "{log}"
        );
    }

    // With a password: where to sign in and the command, nothing else.
    let hash = lmgw_core::accounts::password::hash(
        &lmgw_core::accounts::password::checked(PASSWORD).unwrap(),
    )
    .unwrap();
    lmgw_core::store::set_account_password(&state.db, 1, &hash, None)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let third = start_log(&state).await;
    assert!(
        third.contains("sign in at http://127.0.0.1:")
            && third.contains(lmgw_core::accounts::LOGIN_LINK_COMMAND),
        "{third}"
    );
    assert!(
        !third.contains("set a password") && !third.contains(&key) && !third.contains("token="),
        "{third}"
    );
}

#[tokio::test]
async fn a_desktop_with_accounts_on_keeps_today_s_line() {
    let state = quiet_state().await;
    let gw = serve(state.clone()).await;
    let _ = turn_on(&gw).await;
    let log = start_log(&state).await;
    assert!(
        log.contains("dashboard login: http://127.0.0.1:")
            && log.contains("/api/session/login?token="),
        "{log}"
    );
    assert!(
        !log.contains("set a password") && !log.contains("sign in at"),
        "{log}"
    );
}
