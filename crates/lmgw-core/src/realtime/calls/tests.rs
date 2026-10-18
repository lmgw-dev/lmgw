//! The hub's own checks, through the test hooks the network suites cannot
//! reach: a panic in str0m ends one call and the driver serves on, a driver
//! that died is started again with its groups said to be lost, and a panic
//! reading or answering an offer is said as the offer's refusal.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use super::offer::tests::client_offer;
use super::*;

async fn calls() -> Calls {
    let calls = Calls::default();
    let state = calls
        .bind_group(Group::Plain, &["127.0.0.1:0".parse().unwrap()], None)
        .await;
    assert!(matches!(state, GroupState::Bound(_)), "{state:?}");
    calls
}

fn offer(never_opened: Option<Duration>) -> Offer {
    Offer {
        sdp: client_offer(false, true),
        group: Group::Plain,
        local: "127.0.0.1".parse().unwrap(),
        served_at: None,
        creator: Principal::Anonymous,
        params: CallParams {
            never_opened,
            reorder: crate::config::WebRtcAudioSettings::default().reorder(),
            max_message: None,
        },
        keep: None,
    }
}

async fn ended(h: &mut CallHandle) -> Ended {
    let wait = async {
        loop {
            match h.events.next().await {
                Some(CallEvent::Ended(why)) => return why,
                Some(_) => {}
                None => panic!("the events ended without an end"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("the call ends")
}

#[tokio::test]
async fn a_panic_in_one_call_ends_only_it_and_the_driver_serves_on() {
    let calls = calls().await;
    let mut a = calls.offer(offer(None)).await.unwrap().handle;
    let mut b = calls
        .offer(offer(Some(Duration::from_millis(300))))
        .await
        .unwrap()
        .handle;
    calls.panic_for_tests(a.id());
    let why = ended(&mut a).await;
    assert!(
        matches!(&why, Ended::Failed(m) if m.contains("panicked") && m.contains("injected")),
        "{why:?}"
    );
    assert!(calls.get(a.id()).is_none(), "gone from the registry");
    // The driver went on: the other call is still timed, and new ones are
    // answered.
    assert_eq!(ended(&mut b).await, Ended::NeverConnected);
    let mut c = calls
        .offer(offer(Some(Duration::from_millis(100))))
        .await
        .unwrap()
        .handle;
    assert_eq!(ended(&mut c).await, Ended::NeverConnected);
}

#[tokio::test]
async fn a_dead_driver_is_started_again_and_its_groups_are_said_lost() {
    let calls = calls().await;
    let mut a = calls.offer(offer(None)).await.unwrap().handle;
    let released = calls.hangup(a.id(), None).unwrap();
    calls.kill_driver_for_tests();
    // Its calls' events end with it, and the registry forgets them as the
    // task unwinds: nothing lists them, a hangup's answer resolves.
    let wait = async { while a.events.next().await.is_some() {} };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("the dead driver's call ends");
    assert!(calls.get(a.id()).is_none());
    assert!(calls.list().is_empty());
    tokio::time::timeout(Duration::from_secs(30), released.wait())
        .await
        .expect("the hangup's answer resolves");
    let state = calls.group(Group::Plain);
    assert!(
        matches!(&state, GroupState::Failed(m) if m.contains("driver stopped")),
        "{state:?}"
    );
    let refused = calls.offer(offer(None)).await.err().unwrap();
    assert!(matches!(&refused, OfferError::Unavailable(m) if m.contains("driver stopped")));
    // The listener's next start binds and serves on the new driver.
    assert!(matches!(
        calls
            .bind_group(Group::Plain, &["127.0.0.1:0".parse().unwrap()], None)
            .await,
        GroupState::Bound(_)
    ));
    let mut b = calls
        .offer(offer(Some(Duration::from_millis(100))))
        .await
        .unwrap()
        .handle;
    assert_eq!(ended(&mut b).await, Ended::NeverConnected);
}

/// Dropped with the call: what an owner keeps for the call's life.
struct Kept(Arc<AtomicBool>);

impl Drop for Kept {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The driver's own path for a POST that went away (`Command::Register`):
/// no handle exists here, so nothing but the driver can close the call.
#[tokio::test]
async fn a_call_whose_registration_nobody_hears_is_closed_by_the_driver() {
    let calls = calls().await;
    let driver = calls.driver();
    let ip: IpAddr = "127.0.0.1".parse().unwrap();
    let (sock, port) = calls.pick(Group::Plain, ip, None).unwrap();
    let checked = offer::check(&client_offer(false, true), None).unwrap();
    let (max_message, by) = (checked.max_message, checked.by);
    let config = offer::Config {
        reorder: crate::config::WebRtcAudioSettings::default().reorder(),
    };
    let now = Instant::now();
    let candidate = SocketAddr::new(ip, port);
    let (mut rtc, _sdp) = offer::answer(checked, candidate, &config, now.into_std()).unwrap();
    let creds = rtc.direct_api().local_ice_credentials();
    let id: Arc<str> = "rtc_orphan".into();
    let _ending = calls.inner.registry.insert(
        CallInfo {
            id: id.clone(),
            creator: Principal::Anonymous,
            group: Group::Plain,
            candidate,
            state: CallState::Answered,
            created: now,
            connected: None,
            opened: None,
            remote: None,
            last_heard: None,
            unacked_since: None,
            stats: CallStats::default(),
            playout: None,
            held_by: None,
            session: None,
            thread: None,
        },
        sock,
    );
    let kept = Arc::new(AtomicBool::new(false));
    let (events, mut rx) = mpsc::unbounded_channel();
    let (reply, registered) = oneshot::channel();
    drop(registered);
    assert!(driver.send(Command::Register {
        call: Box::new(NewCall {
            id: id.clone(),
            ufrag: creds.ufrag,
            pass: creds.pass,
            sock,
            rtc,
            events,
            feedback: tokio::sync::watch::channel(Default::default()).0,
            queued: Default::default(),
            queue_limit: None,
            max_message,
            by,
            window: offer::sctp_window(),
            bound: None,
            keep: Some(Box::new(Kept(kept.clone()))),
        }),
        reply,
    }));
    let end = tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .expect("the orphan ends");
    assert!(
        matches!(end, Some(CallEvent::Ended(Ended::Closed))),
        "{end:?}"
    );
    assert!(calls.get(&id).is_none());
    assert!(kept.load(Ordering::SeqCst), "what it kept is let go");
}

/// The blocking steps of `Calls::offer` say a panic in str0m as the
/// refusal's reason (logged at ERROR there), whatever its payload.
#[tokio::test]
async fn a_panic_reading_an_offer_is_said_as_the_refusals_reason() {
    let e = tokio::task::spawn_blocking(|| panic!("injected {}", 7))
        .await
        .unwrap_err();
    assert_eq!(
        blocking_failed("reading the offer", e),
        "str0m panicked reading the offer: injected 7"
    );
    let e = tokio::task::spawn_blocking(|| std::panic::panic_any(7u8))
        .await
        .unwrap_err();
    assert_eq!(
        blocking_failed("answering the offer", e),
        "str0m panicked answering the offer: (no message)"
    );
}
