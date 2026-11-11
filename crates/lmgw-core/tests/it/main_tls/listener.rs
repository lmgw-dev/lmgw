//! The TLS listener serves the plain listener's router over TLS 1.2 and
//! 1.3 without asking for a client certificate (D7), picks a renewed pair
//! up at the next accept and keeps the old one while a new one does not
//! load (D8), stops with the plain listener and comes up again at the next
//! start (§4.7, a restart).

use std::time::Duration;

use lmgw_api_types::{SettingsFull, TlsListenerState};
use rustls::version::{TLS12, TLS13};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{chmod, handshake, serial_of, serve, write_replacing, Pki};
use crate::common::patience::{until, until_async};

#[tokio::test]
async fn it_serves_the_router_over_tls_1_2_and_1_3_without_a_client_certificate() {
    let pki = Pki::new();
    let gw = serve(&pki, |_| {}).await;
    for (versions, wanted) in [
        (&[&TLS13][..], rustls::ProtocolVersion::TLSv1_3),
        (&[&TLS12][..], rustls::ProtocolVersion::TLSv1_2),
    ] {
        let (_, got) = gw.handshake(&pki, versions).await.unwrap();
        assert_eq!(got, wanted);
    }
    // The same router: the version route answers on both listeners.
    let tls = pki
        .http()
        .get(format!("https://localhost:{}/api/version", gw.tls.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(tls.status(), 200);
    let plain = reqwest::get(format!("http://{}/api/version", gw.plain))
        .await
        .unwrap();
    assert_eq!(plain.status(), 200, "the plain listener stays on bind_addr");
    gw.stop().await;
}

#[tokio::test]
async fn a_renewed_pair_is_served_from_the_next_connection_and_a_broken_one_keeps_the_old() {
    let pki = Pki::new();
    let gw = serve(&pki, |_| {}).await;
    let (leaf, _) = gw.handshake(&pki, &[&TLS13]).await.unwrap();
    assert_eq!(serial_of(&leaf), "1001");

    // The deploy hook writes a renewed pair: the next connection gets it.
    pki.write(&["localhost", "127.0.0.1"], 2);
    let (leaf, _) = gw.handshake(&pki, &[&TLS13]).await.unwrap();
    assert_eq!(serial_of(&leaf), "1002", "served without a restart");

    // A certificate whose key is not the one beside it: the pair before
    // still serves, and Settings says why.
    let (cert, _) = pki.leaf(&["localhost"], 3);
    write_replacing(&pki.cert_path(), &cert);
    let (leaf, _) = gw.handshake(&pki, &[&TLS13]).await.unwrap();
    assert_eq!(serial_of(&leaf), "1002", "the old pair keeps serving");
    let status = gw.state.main_tls.status(&gw.state.snapshot().settings);
    let e = status.error.expect("the failed reload is shown");
    assert!(e.contains("does not belong to the certificate"), "{e}");
    assert_eq!(status.certificate.unwrap().serial, "1002");

    // A garbled key file: still the old pair.
    write_replacing(&pki.key_path(), "not a key\n");
    let (leaf, _) = gw.handshake(&pki, &[&TLS13]).await.unwrap();
    assert_eq!(serial_of(&leaf), "1002");

    // The hook finishes: a good pair again, picked up, and the error gone.
    pki.write(&["localhost", "127.0.0.1"], 4);
    let (leaf, _) = gw.handshake(&pki, &[&TLS13]).await.unwrap();
    assert_eq!(serial_of(&leaf), "1004");
    let status = gw.state.main_tls.status(&gw.state.snapshot().settings);
    assert_eq!(status.error, None);
    assert_eq!(status.state, TlsListenerState::Serving);
    gw.stop().await;
}

#[tokio::test]
async fn settings_show_the_certificate_and_what_it_does_not_cover() {
    let pki = Pki::new();
    let gw = serve(&pki, |s| {
        s.public_url = "https://lmgw.example:8443".into();
        s.agent_origin_suffix = "lmgw-apps.example".into();
    })
    .await;
    let full: SettingsFull = pki
        .http()
        .get(format!(
            "https://localhost:{}/api/settings-full",
            gw.tls.port()
        ))
        .header(
            "authorization",
            format!(
                "Bearer {}",
                gw.state
                    .snapshot()
                    .owner_key(lmgw_core::agents::token::OWNER_DASHBOARD)
                    .unwrap()
            ),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let t = full.tls_status;
    assert_eq!(t.state, TlsListenerState::Serving);
    assert_eq!(t.listening, Some(gw.tls.to_string()));
    let cert = t.certificate.unwrap();
    assert_eq!(cert.names, ["localhost", "127.0.0.1"]);
    assert_eq!(cert.serial, "1001");
    assert_eq!(t.warnings.len(), 2, "{:?}", t.warnings);
    assert!(t.warnings[0].contains("public URL's host (lmgw.example)"));
    assert!(t.warnings[1].contains("(*.lmgw-apps.example)"));
    assert!(!t.restart_pending);
    assert_eq!(full.public_base_url, "https://lmgw.example:8443");
    gw.stop().await;
}

/// D34: a reload that failed is tried again when either file's change time
/// moves, so a fix that only changes the key's mode (`chmod`, as `chown`,
/// `restorecon` and `chcon` do) is picked up at the next accept. A key its
/// group can read is a warning.
#[tokio::test]
async fn a_reload_that_failed_on_the_key_s_mode_is_retried_once_only_the_mode_is_fixed() {
    if unsafe { libc::geteuid() } == 0 {
        // Root reads a file of mode 000: nothing to fail on.
        return;
    }
    let pki = Pki::new();
    let gw = serve(&pki, |_| {}).await;
    // A renewed pair whose key this account cannot read.
    pki.write(&["localhost", "127.0.0.1"], 5);
    chmod(&pki.key_path(), 0o000);
    let (leaf, _) = gw.handshake(&pki, &[&TLS13]).await.unwrap();
    assert_eq!(serial_of(&leaf), "1001", "the pair before still serves");
    let status = gw.state.main_tls.status(&gw.state.snapshot().settings);
    let e = status.error.expect("the failed reload is shown");
    assert!(e.contains("reading the private key"), "{e}");
    // Only the mode is fixed: no write, no rename. The change time is the
    // stamp's one moving part; a coarse clock tick would leave it equal.
    let before = std::fs::metadata(pki.key_path()).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    chmod(&pki.key_path(), 0o600);
    {
        use std::os::unix::fs::MetadataExt;
        let after = std::fs::metadata(pki.key_path()).unwrap();
        assert_eq!(
            (before.mtime(), before.mtime_nsec(), before.ino()),
            (after.mtime(), after.mtime_nsec(), after.ino())
        );
    }
    let (leaf, _) = gw.handshake(&pki, &[&TLS13]).await.unwrap();
    assert_eq!(serial_of(&leaf), "1005", "picked up at the next accept");
    let status = gw.state.main_tls.status(&gw.state.snapshot().settings);
    assert_eq!(status.error, None);
    assert!(status.warnings.is_empty(), "{:?}", status.warnings);
    // Readable by its group: served, and warned of.
    tokio::time::sleep(Duration::from_millis(20)).await;
    chmod(&pki.key_path(), 0o640);
    gw.handshake(&pki, &[&TLS13]).await.unwrap();
    let w = gw
        .state
        .main_tls
        .status(&gw.state.snapshot().settings)
        .warnings;
    assert!(
        w.len() == 1 && w[0].contains("the file's group") && w[0].contains("0640"),
        "{w:?}"
    );
    gw.stop().await;
}

/// A restart (the tray's Restart gateway serves again on the same state):
/// the old TLS listener has released its port when `released` fires, while
/// a request it accepted is still in flight; the next start binds the same
/// port at once and serves the pair at its paths now, and the old
/// connection drains beside it.
#[tokio::test]
async fn a_restart_serves_the_same_tls_port_while_the_old_connection_drains() {
    let pki = Pki::new();
    let gw = serve(&pki, |_| {}).await;
    let port = gw.tls.port();
    // A connection held open with a request half sent: its body is still
    // to come, so the old server cannot finish it at its stop.
    let mut old = {
        let cfg = std::sync::Arc::new(pki.client(&[&TLS13]));
        let tcp = tokio::net::TcpStream::connect(gw.tls).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        tokio_rustls::TlsConnector::from(cfg)
            .connect(name, tcp)
            .await
            .unwrap()
    };
    let body = br#"{"token":"lmgw-not-a-key"}"#;
    old.write_all(
        format!(
            "POST /api/session HTTP/1.1\r\nHost: localhost:{port}\r\nContent-Type: \
             application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    old.write_all(&body[..4]).await.unwrap();
    old.flush().await.unwrap();
    let state = gw.state.clone();
    let old_at = state.stops.now_of(lmgw_core::server::Via::Tls);
    until("the half-sent request is in flight", || {
        state.stops.open_requests(old_at) == ["POST /api/session"]
    })
    .await;

    let (state, draining) = gw.stop_released().await;
    // A renewed pair at the same paths, and the same address: the start
    // binds it while the old connection is still open.
    pki.write(&["localhost", "127.0.0.1"], 7);
    let again = super::serve_on(state, &pki, |s| {
        s.tls.listen = format!("127.0.0.1:{port}");
    })
    .await;
    assert_eq!(again.tls.port(), port);
    let (leaf, _) = handshake(&pki, again.tls, &[&TLS13]).await.unwrap();
    assert_eq!(serial_of(&leaf), "1007");
    assert!(!draining.is_finished(), "the old server is still draining");
    assert_eq!(
        again.state.stops.open_requests(old_at),
        ["POST /api/session"]
    );

    // The old connection finishes its request and gets its answer.
    old.write_all(&body[4..]).await.unwrap();
    old.flush().await.unwrap();
    let mut head = [0u8; 12];
    old.read_exact(&mut head).await.unwrap();
    assert!(
        head.starts_with(b"HTTP/1.1 "),
        "{:?}",
        String::from_utf8_lossy(&head)
    );
    drop(old);
    tokio::time::timeout(Duration::from_secs(30), draining)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // The old start's end clears nothing of the new one's.
    let snap = again.state.snapshot();
    assert_eq!(
        snap.serving_tls.as_ref().map(|t| t.addr),
        Some(again.tls),
        "the new start still publishes its TLS listener"
    );
    assert_eq!(
        again.state.main_tls.status(&snap.settings).state,
        TlsListenerState::Serving
    );
    again.stop().await;
}

/// A desktop start whose pair does not load serves on without TLS and says
/// why; nothing waits on the TLS listener.
#[tokio::test]
async fn a_desktop_start_without_a_loadable_pair_serves_plain_and_says_why() {
    let pki = Pki::new();
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    super::set(&state, |s| {
        s.tls.cert_path = pki.cert_path().display().to_string();
        s.tls.key_path = pki.key_path().display().to_string();
        s.tls.listen = format!("127.0.0.1:{}", super::free_port());
    })
    .await;
    let listener = lmgw_core::server::bind(&state, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let plain = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(lmgw_core::server::serve(
        state.clone(),
        listener,
        async {
            let _ = stopped.await;
        },
        None,
    ));
    until_async("the plain listener answers", || async {
        reqwest::get(format!("http://{plain}/api/version"))
            .await
            .is_ok_and(|r| r.status() == 200)
    })
    .await;
    let status = state.main_tls.status(&state.snapshot().settings);
    assert_eq!(status.state, TlsListenerState::NotServing);
    let e = status.error.unwrap();
    assert!(e.contains("reading the certificate"), "{e}");
    // Nothing serves TLS, so no URL names it: the plain listener's.
    assert_eq!(state.snapshot().serving_tls, None);
    let key = state
        .snapshot()
        .owner_key(lmgw_core::agents::token::OWNER_DASHBOARD)
        .unwrap()
        .to_string();
    let get = |path: &'static str| {
        let key = key.clone();
        async move {
            reqwest::Client::new()
                .get(format!("http://{plain}{path}"))
                .bearer_auth(key)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let connect = get("/api/connect").await;
    assert!(
        connect["bases"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["url"].as_str().unwrap().starts_with("http://")),
        "{connect}"
    );
    let base = get("/api/wiring").await["base_url"].clone();
    assert!(base.as_str().unwrap().starts_with("http://"), "{base}");
    // And the server's own start refuses it instead (§1 step 6).
    let e = lmgw_core::server::bind_tls(&state).await.err().unwrap();
    assert!(
        format!("{e:#}").contains("the TLS listener's pair"),
        "{e:#}"
    );
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(30), served)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
