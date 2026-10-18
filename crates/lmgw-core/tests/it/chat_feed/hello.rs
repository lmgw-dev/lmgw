//! What `hello` tells a device about itself, and what the feed does to its
//! own connection (device link design §7.1, §7.2, WP5): the URL list and the
//! certificate's dates from the row, `dashboard_url` from `bind_addr`,
//! `TCP_USER_TIMEOUT` on the connection's socket, and the details `Links`
//! keeps for the card.

use std::time::Duration;

use serde_json::{json, Value};

use super::Feed;
use crate::device_chat::{op, pair};
use crate::realtime_chat_thread::{settings, world, World};
use crate::support::device_pair::listening;

/// `https://127.0.0.1:<port>` for the device listener: a row's URL list that
/// is loopback only.
async fn loopback_url(w: &World) -> String {
    format!("https://127.0.0.1:{}", listening(&w.state).await)
}

/// A device paired as another machine would be: not "this computer", with
/// `urls` (the first reaches the test's listener).
async fn remote(w: &World, name: &str, urls: Vec<String>) -> crate::device_chat::Device {
    pair(w, name, json!({ "local_only": false, "urls": urls })).await
}

/// §7.2: `hello.urls` is the row's list as the owner set it, in order, and
/// follows an edit at the next feed; `hello.cert` is the dates of the
/// certificate the row holds. Neither is on an owner's feed.
#[tokio::test]
async fn hello_carries_the_device_s_urls_and_certificate_dates() {
    let w = world(|_| {}).await;
    let first = loopback_url(&w).await;
    let urls = vec![first.clone(), "https://192.0.2.7:8788".to_string()];
    let d = remote(&w, "phone", urls.clone()).await;

    let f = Feed::open(&w, &d.client, "", None).await;
    let hello = f.hello();
    assert_eq!(hello["urls"], json!(urls), "{hello}");
    let ms = |key: &str| {
        chrono::DateTime::parse_from_rfc3339(hello["cert"][key].as_str().unwrap())
            .unwrap()
            .timestamp_millis()
    };
    assert_eq!(ms("not_after"), d.credentials.not_after_ms, "{hello}");
    assert_eq!(ms("notice_after"), d.credentials.notice_after_ms, "{hello}");
    super::documented("hello", hello);

    // An edit of the list reaches the next feed, in the owner's order.
    let reordered = vec![urls[1].clone(), first];
    let (s, v) = op(
        &w,
        "key_set",
        json!({ "id": d.id, "name": "phone", "urls": reordered }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let f = Feed::open(&w, &d.client, "", None).await;
    assert_eq!(f.hello()["urls"], json!(reordered), "{}", f.hello());

    let owner = Feed::open(&w, &w.gw.client(), "", None).await;
    let hello = owner.hello();
    assert_eq!(hello["urls"], json!([]), "an owner has no URL list");
    assert_eq!(hello["cert"], Value::Null);
    assert_eq!(hello["dashboard_url"], Value::Null);
}

/// §7.2, the owner's decision of 2026-10-15: a device paired as "this
/// computer" is told where the dashboard is whatever `bind_addr` is (a
/// wildcard bind reads as loopback for it); any other device only when the
/// bind address is one a device elsewhere can reach, a wildcard bind read as
/// the address its own connection arrived on.
#[tokio::test]
async fn hello_names_the_dashboard_to_this_computer_and_to_others_off_loopback() {
    let w = world(|s| s.bind_addr = "127.0.0.1:8001".into()).await;
    let here = pair(&w, "desktop", json!({})).await;
    let there = remote(&w, "phone", vec![loopback_url(&w).await]).await;
    let dashboard = |f: &Feed| f.hello()["dashboard_url"].clone();

    // Loopback bind: only this computer can use it.
    let f = Feed::open(&w, &here.client, "", None).await;
    assert_eq!(dashboard(&f), json!("http://127.0.0.1:8001/"));
    super::documented("hello", f.hello());
    let f = Feed::open(&w, &there.client, "", None).await;
    assert_eq!(dashboard(&f), Value::Null, "a remote row, a loopback bind");

    // A concrete address is itself, for every device.
    settings(&w.state, |s| s.bind_addr = "192.0.2.5:8001".into()).await;
    let f = Feed::open(&w, &here.client, "", None).await;
    assert_eq!(dashboard(&f), json!("http://192.0.2.5:8001/"));
    let f = Feed::open(&w, &there.client, "", None).await;
    assert_eq!(dashboard(&f), json!("http://192.0.2.5:8001/"));

    // A wildcard bind: loopback for this computer; for another device the
    // address its connection arrived on, which here is loopback itself, so
    // there is nothing to offer.
    settings(&w.state, |s| s.bind_addr = "0.0.0.0:8001".into()).await;
    let f = Feed::open(&w, &here.client, "", None).await;
    assert_eq!(dashboard(&f), json!("http://127.0.0.1:8001/"));
    let f = Feed::open(&w, &there.client, "", None).await;
    assert_eq!(dashboard(&f), Value::Null);
}

/// The server-side sockets of this process whose `TCP_USER_TIMEOUT` is
/// `secs` seconds.
fn sockets_with_user_timeout(secs: u32) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|fd| {
            let mut ms: libc::c_uint = 0;
            let mut len = std::mem::size_of::<libc::c_uint>() as libc::socklen_t;
            // SAFETY: `ms` and `len` are valid for the call; a descriptor
            // that is no TCP socket (or closed meanwhile) is an error.
            let r = unsafe {
                libc::getsockopt(
                    *fd,
                    libc::IPPROTO_TCP,
                    libc::TCP_USER_TIMEOUT,
                    (&raw mut ms).cast(),
                    &raw mut len,
                )
            };
            r == 0 && ms == secs * 1000
        })
        .count()
}

/// §7.1: a device's feed sets `TCP_USER_TIMEOUT` on its connection to its
/// keep-alive, and `Links` keeps that keep-alive, the peer and the start
/// beside the link, for as long as it is open. An owner's feed (on the plain
/// listener) sets none.
#[tokio::test]
async fn a_device_feed_sets_tcp_user_timeout_from_its_keepalive() {
    let w = world(|_| {}).await;
    let d = pair(&w, "phone", json!({})).await;
    assert_eq!(sockets_with_user_timeout(37), 0);

    let owner = Feed::open(&w, &w.gw.client(), "?keepalive_s=37", None).await;
    assert_eq!(sockets_with_user_timeout(37), 0, "the plain listener's");
    drop(owner);

    let f = Feed::open(&w, &d.client, "?keepalive_s=37", None).await;
    assert_eq!(f.hello()["keepalive_s"], 37);
    assert!(
        sockets_with_user_timeout(37) >= 1,
        "the device's socket carries it"
    );

    let details = w.state.devices.links.details(d.id);
    let [link] = details.as_slice() else {
        panic!("{details:?}")
    };
    assert_eq!(link.kind.as_str(), "feed");
    assert_eq!(link.keepalive_s, Some(37));
    assert!(link.peer.is_some_and(|p| p.ip().is_loopback()), "{link:?}");
    assert!(
        chrono::Utc::now().signed_duration_since(link.since) < chrono::Duration::seconds(30),
        "{link:?}"
    );

    // Closed: the link and the socket go.
    drop(f);
    for _ in 0..50 {
        if w.state.devices.links.details(d.id).is_empty() && sockets_with_user_timeout(37) == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "the link outlived its feed: {:?}",
        w.state.devices.links.details(d.id)
    );
}
